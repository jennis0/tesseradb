//! **What does an epoch-sharded mask cost the viewport sweep?**
//!
//! The proposal: split a corpus into N epoch shards inside one process, and make the viewer mask a
//! treemap — a sorted vector of `(shard, croaring 32-bit Bitmap)`, a row addressed as shard in the
//! high word and a local `u32` in the low word. A tile that is one contiguous row range today
//! becomes N ranges, one per shard, each about 1/N as long. The sweep's three mask operations
//! (`viewport.rs` `tile_sweep`) are measured over a treemap of N leaves against the single bitmap
//! they replace, at N ∈ {1, 2, 4, 8}:
//!
//! 1. **count** — `RowProjection::range_cardinality` over each part of each tile.
//! 2. **decode** — the visible rows of each part, read the way `Selection::of` reads them: the
//!    tier chosen from `(visible, range.len())` by `select::decode_tier`, then the full range, a
//!    cursor over runs, or a batched value walk (`compose.rs` `for_each_run_in` and
//!    `decode_source`, on the diffs-empty route the steady state takes).
//! 3. **select** — `Selection::of` itself, unchanged, with N parts per tile: the m smallest
//!    `tessera_id`s across the parts with the bounded heap.
//!
//! A fourth column, **rows_in_range**, is `EffectiveMask::rows_in_range`'s materialised
//! `leaf ∩ range`, which is what the decode pays instead when the overlay diffs are non-empty.
//!
//! Synthetic and in memory: a universe of 2³⁰ rows, one bitmap over it at N = 1 or N leaves over
//! 2³⁰/N each at the same density, rows chosen independently at random (realistic masks measure
//! as essentially scattered under Morton order, run ratio 1.03–1.15 — `probes/results.md` §5),
//! plus one run-heavy variant for contrast. A tile at depth d is a range of 2³⁰/4ᵈ rows in the
//! single space and N ranges of 2³⁰/(N·4ᵈ) in the leaves, at the same map position, each range
//! starting at an arbitrary row within its container as a real tile's does.
//!
//! `Selection::of` takes one `EffectiveMask` over one row space, so for the select column the N
//! leaves are presented to it as a single view-space bitmap — leaf s at row base s·2³⁰/N — with
//! N `SelectionPart`s per tile. That is exactly the parts-and-ranges shape a treemap-backed mask
//! would hand the shipped loop; what it omits is the leaf lookup itself, which the count and
//! decode columns pay through the `Treemap` type below and which is a binary search over N ≤ 8
//! entries. The identity column is synthetic (random `u64` per row) and 2²⁶ rows long, so the
//! select column starts at the first depth whose single-space tile fits it; every part reads the
//! column at its own random offset so consecutive tiles do not share cache lines.
//!
//! ```text
//! nice -n 10 cargo run --release -p tessera-bench --bin epoch_shard_treemap_mask -- \
//!     --scratch /tmp/epoch-shard --out result.json
//! ```

use std::hint::black_box;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::buffer::Buffer;
use clap::Parser;
use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use serde::Serialize;

use tessera_engine::compose::{compose, EffectiveMask, RowProjection};
use tessera_engine::select::{
    decode_tier, DecodeTier, SelectParams, Selection, SelectionPart, SelectionParts, Threshold,
};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::write::{write_columns_from_parts, write_permutation};
use tessera_store::{Permutation, RowSpace};
use tessera_types::TermId;

/// 2³⁰ rows: the row space every case is a partition of.
const UNIVERSE: u64 = 1 << 30;
/// The server default for `theta_target_marks`, so θ's depth progression is the deployed one.
const M_TARGET: u64 = 16;
/// `select.rs`'s value-tier batch: 1024 × 4 B, a 4 KiB stack buffer.
const VALUE_BUF_LEN: usize = 1024;
/// `compose.rs`'s run-cursor batch.
const RUN_BUF_LEN: usize = 64;
/// Mean run length of the run-heavy variant; the gap length follows from the density.
const RUN_MEAN: f64 = 256.0;
/// Rows the mask builder hands to `add_many` at a time.
const SINK_FLUSH: usize = 1 << 20;

#[derive(Parser)]
#[command(about = "Cost of the sweep's mask operations over an N-shard treemap against one bitmap")]
struct Args {
    /// Shard counts to measure.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
    shards: Vec<u32>,
    /// Mask coverages, in percent of the universe, rows scattered uniformly.
    #[arg(long, value_delimiter = ',', default_value = "50,10,1,0.1")]
    coverage_pct: Vec<f64>,
    /// Coverage of the run-heavy contrast variant, in percent; 0 skips it.
    #[arg(long, default_value_t = 10.0)]
    run_heavy_pct: f64,
    /// Tiles per request, where the depth has that many.
    #[arg(long, default_value_t = 256)]
    tiles: usize,
    /// Timed requests per cell; the median is reported.
    #[arg(long, default_value_t = 5)]
    samples: usize,
    /// Untimed requests before the samples.
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    /// Deepest tile depth. A depth whose per-leaf range is under one row is skipped.
    #[arg(long, default_value_t = 14)]
    max_depth: u8,
    /// Selection cap, `min(k, k_max_marks)`.
    #[arg(long, default_value_t = 30)]
    cap: usize,
    /// Selection floor, `k_min`.
    #[arg(long, default_value_t = 4)]
    k_min: usize,
    /// log₂ of the synthetic identity column's length. 26 is 512 MB.
    #[arg(long, default_value_t = 26)]
    id_rows_log2: u32,
    #[arg(long, default_value_t = 0x5EED)]
    seed: u64,
    /// Where the synthetic segment's files go; removed at exit.
    #[arg(long)]
    scratch: Option<PathBuf>,
    /// JSON record of every cell.
    #[arg(long)]
    out: Option<PathBuf>,
}

// ---------------------------------------------------------------------------------------------
// The thing under test.
// ---------------------------------------------------------------------------------------------

/// N epoch shards inside one mask: a sorted vector of `(shard, Bitmap)`, each bitmap over the
/// shard's own row space. A row is `(shard, local)`; every operation finds the leaf and delegates
/// to the croaring call `EffectiveMask` makes today on its one bitmap.
struct Treemap {
    leaves: Vec<(u32, Bitmap)>,
}

impl Treemap {
    fn leaf(&self, shard: u32) -> &Bitmap {
        let i = self
            .leaves
            .binary_search_by_key(&shard, |(s, _)| *s)
            .expect("every shard a tile names is a leaf");
        &self.leaves[i].1
    }

    /// `RowProjection::range_cardinality`, per leaf.
    fn range_cardinality(&self, shard: u32, r: Range<u32>) -> u64 {
        self.leaf(shard).range_cardinality(r)
    }

    /// `EffectiveMask::rows_in_range` with the diffs empty: `leaf ∩ r`, materialised.
    fn rows_in_range(&self, shard: u32, r: Range<u32>) -> Bitmap {
        self.leaf(shard).and(&Bitmap::from_range(r))
    }

    /// `EffectiveMask::for_each_visible_run` on the diffs-empty route: a cursor over the leaf.
    fn for_each_run(&self, shard: u32, r: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
        for_each_run_in(self.leaf(shard), r, f);
    }

    /// `EffectiveMask::decode_source` on the diffs-empty route: the leaf itself, for the caller's
    /// own `reset_at_or_after` / `next_many` walk.
    fn decode_source(&self, shard: u32) -> &Bitmap {
        self.leaf(shard)
    }

    fn cardinality(&self) -> u64 {
        self.leaves.iter().map(|(_, b)| b.cardinality()).sum()
    }
}

/// `compose.rs`'s private `for_each_run_in`, transcribed unchanged: walk `bitmap ∩ r` as
/// ascending half-open runs, one FFI crossing per `RUN_BUF_LEN` runs.
fn for_each_run_in(bitmap: &Bitmap, r: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
    if r.start >= r.end {
        return;
    }
    let mut cursor = bitmap.cursor();
    cursor.reset_at_or_after(r.start);
    let mut buf = [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF_LEN];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            return;
        }
        for run in &buf[..n] {
            if run.start >= r.end {
                return;
            }
            if run.last >= r.end - 1 {
                f(run.start..r.end);
                return;
            }
            f(run.start..run.last + 1);
        }
    }
}

/// One part's visible rows, read the way `Selection::of` reads them: the tier from
/// `(visible, range.len())`, then the same croaring calls. Returns the rows visited and an xor of
/// them, so the walk cannot be optimised away.
fn decode_part(map: &Treemap, shard: u32, r: Range<u32>, visible: u64) -> (u64, u32) {
    let len = u64::from(r.end - r.start);
    let mut visited = 0u64;
    let mut acc = 0u32;
    match decode_tier(visible, len) {
        DecodeTier::FullRange => {
            visited += len;
            acc ^= r.start ^ r.end;
        }
        DecodeTier::Runs => map.for_each_run(shard, r, &mut |run| {
            visited += u64::from(run.end - run.start);
            acc ^= run.start;
        }),
        DecodeTier::Values => {
            let mut iter = map.decode_source(shard).iter();
            iter.reset_at_or_after(r.start);
            let mut buf = [0u32; VALUE_BUF_LEN];
            let mut remaining = visible;
            'decode: while remaining > 0 {
                let want = remaining.min(VALUE_BUF_LEN as u64) as usize;
                let n = iter.next_many(&mut buf[..want]);
                if n == 0 {
                    break;
                }
                for &row in &buf[..n] {
                    if row >= r.end {
                        break 'decode;
                    }
                    visited += 1;
                    acc ^= row;
                }
                remaining -= n as u64;
            }
        }
    }
    (visited, acc)
}

// ---------------------------------------------------------------------------------------------
// Synthetic masks and the synthetic segment.
// ---------------------------------------------------------------------------------------------

/// splitmix64 — a few nanoseconds per draw, which 2³⁰ draws per leaf needs.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Exponential with the given mean, rounded up to a whole row.
    fn exponential(&mut self, mean: f64) -> u64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        (-(1.0 - u).ln() * mean).ceil().max(1.0) as u64
    }
}

/// Ascending rows into a bitmap through `add_many`, the way a row projection is built — bitset
/// or array containers by density, never run containers, whatever the run structure.
struct RowSink {
    bitmap: Bitmap,
    buf: Vec<u32>,
}

impl RowSink {
    fn new() -> Self {
        RowSink {
            bitmap: Bitmap::new(),
            buf: Vec::with_capacity(SINK_FLUSH),
        }
    }

    fn push(&mut self, row: u32) {
        self.buf.push(row);
        if self.buf.len() == SINK_FLUSH {
            self.bitmap.add_many(&self.buf);
            self.buf.clear();
        }
    }

    fn finish(mut self) -> Bitmap {
        self.bitmap.add_many(&self.buf);
        self.bitmap
    }
}

/// One leaf of `rows` rows at density `p`: rows chosen independently (scattered), or geometric
/// runs and gaps with mean run `RUN_MEAN` (run-heavy).
fn build_leaf(rows: u64, p: f64, run_heavy: bool, seed: u64) -> Bitmap {
    let mut rng = SplitMix(seed);
    let mut sink = RowSink::new();
    if run_heavy {
        let mean_gap = RUN_MEAN * (1.0 - p) / p;
        let mut pos = 0u64;
        loop {
            pos += rng.exponential(mean_gap);
            if pos >= rows {
                break;
            }
            let end = (pos + rng.exponential(RUN_MEAN)).min(rows);
            for row in pos..end {
                sink.push(row as u32);
            }
            pos = end;
            if pos >= rows {
                break;
            }
        }
    } else {
        let threshold = (p * 4_294_967_296.0) as u64;
        for row in 0..rows {
            if (rng.next() >> 32) < threshold {
                sink.push(row as u32);
            }
        }
    }
    sink.finish()
}

fn build_treemap(shards: u32, p: f64, run_heavy: bool, seed: u64) -> Treemap {
    let rows = UNIVERSE / u64::from(shards);
    Treemap {
        leaves: (0..shards)
            .map(|s| {
                (
                    s,
                    build_leaf(rows, p, run_heavy, seed ^ (u64::from(s) << 40)),
                )
            })
            .collect(),
    }
}

/// The leaves as one view-space bitmap, leaf s at row base s·(2³⁰/N), wrapped in a real
/// `EffectiveMask` with empty diffs — what `Selection::of` takes.
fn view_space_mask(map: &Treemap, scratch: &Path) -> EffectiveMask {
    let shard_rows = UNIVERSE / map.leaves.len() as u64;
    let mut concat = Bitmap::new();
    for (s, leaf) in &map.leaves {
        concat.or_inplace(&leaf.add_offset((u64::from(*s) * shard_rows) as i64));
    }
    // `compose` consults the row space only for buffered entities, and the buffer is empty, so an
    // empty permutation stands in for one.
    let perm_path = scratch.join("permutation.bin");
    write_permutation(&perm_path, &[], 0).expect("write empty permutation");
    let space = RowSpace::new(
        Arc::new(Permutation::load(&perm_path).expect("load empty permutation")),
        UNIVERSE as u32,
    );
    let satisfied: FxHashSet<TermId> = FxHashSet::default();
    compose(
        &satisfied,
        &Overlay::default(),
        &IngestBuffer::default(),
        Arc::new(RowProjection::from_rows(concat)),
        &space,
        &Bitmap::new(),
    )
}

/// A segment whose identity column is `rows` random `u64`s, each row in a leaf cell of its own.
///
/// **One row a cell, because the identities are random.** Row order is `(morton, tessera_id)`, so
/// identities ascend within a cell and selection reads them expecting that; a segment claiming one
/// cell over unsorted identities would be a segment no producer can write. One row a cell is the
/// shape that asks nothing of the identities, and it is the shape that makes selection read every
/// visible row — which is the cost this bench is measuring.
fn synthetic_segment(rows: usize, seed: u64, scratch: &Path) -> SegmentData {
    let mut rng = SplitMix(seed);
    let ids: Vec<u64> = (0..rows).map(|_| rng.next()).collect();
    let residual: Vec<u32> = vec![0; rows];
    let columns = scratch.join("columns.arrow");
    write_columns_from_parts(
        &columns,
        Buffer::from_vec(ids),
        Buffer::from_vec(residual),
        rows,
    )
    .expect("write synthetic columns");
    let mut codes: Vec<u8> = Vec::with_capacity(rows * 4);
    let mut starts: Vec<u8> = Vec::with_capacity(rows * 4);
    for row in 0..rows as u32 {
        codes.extend_from_slice(&row.to_le_bytes());
        starts.extend_from_slice(&row.to_le_bytes());
    }
    let morton = scratch.join("morton.u32");
    std::fs::write(&morton, &codes).expect("write morton codes");
    let cuts = scratch.join(tessera_store::read::CutIndex::FILE);
    std::fs::write(&cuts, &starts).expect("write cell starts");
    SegmentData {
        seg_id: "epoch-shard-synthetic".into(),
        row_count: rows as u32,
        morton: MortonSlice::load(&morton).expect("load morton codes"),
        cuts: tessera_store::read::CutIndex::load(&cuts, rows as u32).expect("load cell starts"),
        columns: ColumnsRef::load(&columns).expect("load synthetic columns"),
    }
}

// ---------------------------------------------------------------------------------------------
// Measurement.
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct Cell {
    variant: String,
    coverage_pct: f64,
    shards: u32,
    depth: u8,
    /// Tiles in the request: `min(--tiles, 4^depth)`.
    tiles: usize,
    /// Rows per part — the tile's range in one leaf.
    part_rows: u64,
    /// Visible rows summed over the request, from the count pass.
    visible: u64,
    op: &'static str,
    us: Vec<f64>,
    us_median: f64,
}

#[derive(Serialize)]
struct LeafStats {
    variant: String,
    coverage_pct: f64,
    shards: u32,
    cardinality: u64,
    containers: u32,
    array_containers: u32,
    bitset_containers: u32,
    run_containers: u32,
}

#[derive(Serialize)]
struct Report {
    cpu: String,
    cores: usize,
    threads: usize,
    universe_rows: u64,
    id_rows: usize,
    tiles: usize,
    samples: usize,
    warmup: usize,
    cap: usize,
    k_min: usize,
    m_target: u64,
    seed: u64,
    leaves: Vec<LeafStats>,
    cells: Vec<Cell>,
}

const OPS: [&str; 4] = ["count", "decode", "rows_in_range", "select"];

fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v[v.len() / 2]
}

fn measure(warmup: usize, samples: usize, mut f: impl FnMut()) -> Vec<f64> {
    for _ in 0..warmup {
        f();
    }
    (0..samples)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect()
}

/// The tile positions of one depth: every tile where there are at most `tiles` of them,
/// otherwise `tiles` distinct positions at random. The same positions serve every N.
fn positions(depth: u8, tiles: usize, seed: u64) -> Vec<u64> {
    let count = 1usize << (2 * u32::from(depth));
    if count <= tiles {
        return (0..count as u64).collect();
    }
    let mut rng = StdRng::seed_from_u64(seed ^ (u64::from(depth) << 48));
    rand::seq::index::sample(&mut rng, count, tiles)
        .into_iter()
        .map(|p| p as u64)
        .collect()
}

struct Case<'a> {
    variant: &'a str,
    coverage_pct: f64,
    map: &'a Treemap,
    mask: &'a EffectiveMask,
    segment: &'a SegmentData,
    id_rows: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_depth(args: &Args, case: &Case<'_>, depth: u8, cells: &mut Vec<Cell>) {
    let shards = case.map.leaves.len() as u32;
    let shard_rows = UNIVERSE / u64::from(shards);
    let tile_rows = UNIVERSE >> (2 * u32::from(depth));
    let part_rows = tile_rows / u64::from(shards);
    if part_rows == 0 {
        eprintln!("  depth {depth}: per-leaf range under one row at N={shards}, skipped");
        return;
    }
    let positions = positions(depth, args.tiles, args.seed);
    let tiles = positions.len();

    // The request's parts: for each tile, `(shard, local range)` per leaf. Each part's start is
    // moved by up to one container (65,536 rows) of jitter, bounded so the part stays inside its
    // leaf: a real tile's range comes from `partition_point` over the Morton column and begins at
    // an arbitrary row, whereas `2³⁰/4ᵈ` is a multiple of the container width up to depth 7, and
    // a range that starts and ends on container boundaries is counted from the header alone —
    // an unrealistic advantage that a split into N parts would then appear to lose.
    let mut jitter = StdRng::seed_from_u64(args.seed ^ (u64::from(depth) << 40) ^ (u64::from(shards) << 8));
    let parts: Vec<Vec<(u32, Range<u32>)>> = positions
        .iter()
        .map(|&p| {
            (0..shards)
                .map(|s| {
                    let room = shard_rows - (p + 1) * part_rows;
                    let delta = jitter.gen_range(0..=room.min(65_535));
                    let start = p * part_rows + delta;
                    (s, start as u32..(start + part_rows) as u32)
                })
                .collect()
        })
        .collect();

    // The count pass, once outside the timers: op 1's output is op 2's and op 3's input, as in
    // `tile_sweep`.
    let visible: Vec<Vec<u64>> = parts
        .iter()
        .map(|tile| {
            tile.iter()
                .map(|(s, r)| case.map.range_cardinality(*s, r.clone()))
                .collect()
        })
        .collect();
    let total_visible: u64 = visible.iter().flatten().sum();

    let mut record = |op: &'static str, us: Vec<f64>| {
        let us_median = median(&us);
        cells.push(Cell {
            variant: case.variant.to_string(),
            coverage_pct: case.coverage_pct,
            shards,
            depth,
            tiles,
            part_rows,
            visible: total_visible,
            op,
            us,
            us_median,
        });
    };

    let map = case.map;
    let us = measure(args.warmup, args.samples, || {
        let mut sum = 0u64;
        for tile in &parts {
            for (s, r) in tile {
                sum += map.range_cardinality(*s, r.clone());
            }
        }
        black_box(sum);
    });
    record("count", us);

    let us = measure(args.warmup, args.samples, || {
        let mut visited = 0u64;
        let mut acc = 0u32;
        for (tile, vis) in parts.iter().zip(&visible) {
            for ((s, r), v) in tile.iter().zip(vis) {
                let (n, a) = decode_part(map, *s, r.clone(), *v);
                visited += n;
                acc ^= a;
            }
        }
        black_box((visited, acc));
    });
    record("decode", us);

    let us = measure(args.warmup, args.samples, || {
        let mut card = 0u64;
        for tile in &parts {
            for (s, r) in tile {
                card += map.rows_in_range(*s, r.clone()).cardinality();
            }
        }
        black_box(card);
    });
    record("rows_in_range", us);

    if tile_rows > case.id_rows {
        eprintln!(
            "  depth {depth}: tile of {tile_rows} rows exceeds the {} -row id column, select skipped",
            case.id_rows
        );
        return;
    }

    // Each part reads the identity column at its own offset, chosen once here: `off ≤ view_start`
    // keeps `row_base = view_start − off` non-negative, and `off + part_rows ≤ id_rows` keeps
    // the slice inside the column.
    let mut rng = StdRng::seed_from_u64(args.seed ^ (u64::from(depth) << 48) ^ u64::from(shards));
    let offsets: Vec<Vec<u32>> = parts
        .iter()
        .map(|tile| {
            tile.iter()
                .map(|(s, r)| {
                    let view_start = u64::from(*s) * shard_rows + u64::from(r.start);
                    let max_off = view_start.min(case.id_rows - part_rows);
                    rng.gen_range(0..=max_off) as u32
                })
                .collect()
        })
        .collect();

    // The view-space mask must agree with the leaves about every part, or the select column
    // would be measuring a different mask. Checked on the first tile, outside the timers.
    for ((s, r), v) in parts[0].iter().zip(&visible[0]) {
        let start = (u64::from(*s) * shard_rows + u64::from(r.start)) as u32;
        let end = start + (r.end - r.start);
        assert_eq!(
            case.mask.count_range(start..end),
            *v,
            "view-space mask disagrees with leaf {s} over {r:?}"
        );
    }

    let params = SelectParams {
        k_min: args.k_min,
        cap: args.cap,
        // θ's occupied-tile anchor (§7.2) takes `N_occ(depth)` from the view's own Morton order,
        // and this case's identity column is synthetic — its codes are not the view row space's,
        // so a count over it would mean nothing. `4^depth` is used instead: it is the largest
        // `N_occ` the grid allows, so this measures the selection at the loosest threshold the
        // definition can produce, which is the most per-tile work it can be asked for.
        threshold: Threshold::at_depth(
            case.mask.visible_total(),
            M_TARGET,
            1u64 << (2 * u32::from(depth)),
        ),
    };
    let segment = case.segment;
    let mask = case.mask;
    let us = measure(args.warmup, args.samples, || {
        let mut served = 0usize;
        for ((tile, vis), offs) in parts.iter().zip(&visible).zip(&offsets) {
            let sel_parts: Vec<SelectionPart<'_>> = tile
                .iter()
                .zip(vis)
                .zip(offs)
                .map(|(((s, r), v), off)| {
                    let view_start = u64::from(*s) * shard_rows + u64::from(r.start);
                    SelectionPart {
                        segment,
                        range: *off..*off + (r.end - r.start),
                        row_base: (view_start - u64::from(*off)) as u32,
                        visible: *v,
                    }
                })
                .collect();
            let tile_visible: u64 = vis.iter().sum();
            let selected = Selection::of(mask, &SelectionParts::new(&sel_parts), &params, tile_visible);
            served += selected.rows.len();
        }
        black_box(served);
    });
    record("select", us);
}

fn fmt_us(us: f64) -> String {
    if us < 10.0 {
        format!("{us:.2}")
    } else if us < 100.0 {
        format!("{us:.1}")
    } else {
        format!("{us:.0}")
    }
}

fn find<'a>(cells: &'a [Cell], variant: &str, op: &str, shards: u32, depth: u8) -> Option<&'a Cell> {
    cells
        .iter()
        .find(|c| c.variant == variant && c.op == op && c.shards == shards && c.depth == depth)
}

/// One markdown table per variant and operation: rows are depths, columns are N, and the last
/// column is the largest N over the smallest.
fn print_tables(cells: &[Cell], shards: &[u32], max_depth: u8) {
    let mut variants: Vec<String> = Vec::new();
    for c in cells {
        if !variants.contains(&c.variant) {
            variants.push(c.variant.clone());
        }
    }
    let (n_lo, n_hi) = (shards[0], shards[shards.len() - 1]);
    for variant in &variants {
        for op in OPS {
            println!("\n### {variant} — {op} (µs per request, median of samples)\n");
            print!("| depth | tiles | rows/tile (N=1) | visible/request |");
            for n in shards {
                print!(" N={n} |");
            }
            println!(" N={n_hi} / N={n_lo} |");
            print!("|---|---|---|---|");
            for _ in shards {
                print!("---|");
            }
            println!("---|");
            for depth in 0..=max_depth {
                let Some(base) = find(cells, variant, op, n_lo, depth) else {
                    continue;
                };
                print!(
                    "| {depth} | {} | {} | {} |",
                    base.tiles,
                    base.part_rows * u64::from(n_lo),
                    base.visible
                );
                for n in shards {
                    match find(cells, variant, op, *n, depth) {
                        Some(c) => print!(" {} |", fmt_us(c.us_median)),
                        None => print!(" — |"),
                    }
                }
                match find(cells, variant, op, n_hi, depth) {
                    Some(c) => println!(" {:.2} |", c.us_median / base.us_median),
                    None => println!(" — |"),
                }
            }
        }
    }

    println!("\n### Largest N={n_hi} / N={n_lo} ratio per operation\n");
    println!("| op | ratio | variant | depth | N={n_lo} µs | N={n_hi} µs |");
    println!("|---|---|---|---|---|---|");
    for op in OPS {
        let mut best: Option<(f64, &Cell, &Cell)> = None;
        for hi in cells.iter().filter(|c| c.op == op && c.shards == n_hi) {
            if let Some(lo) = find(cells, &hi.variant, op, n_lo, hi.depth) {
                let ratio = hi.us_median / lo.us_median;
                if best.is_none_or(|(r, _, _)| ratio > r) {
                    best = Some((ratio, lo, hi));
                }
            }
        }
        if let Some((ratio, lo, hi)) = best {
            println!(
                "| {op} | {ratio:.2} | {} | {} | {} | {} |",
                hi.variant,
                hi.depth,
                fmt_us(lo.us_median),
                fmt_us(hi.us_median)
            );
        }
    }
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() {
    let args = Args::parse();
    let scratch = args.scratch.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("tessera-epoch-shard-{}", std::process::id()))
    });
    std::fs::create_dir_all(&scratch).expect("create scratch dir");
    let id_rows = 1usize << args.id_rows_log2;

    let mut variants: Vec<(String, f64, bool)> = args
        .coverage_pct
        .iter()
        .map(|&p| (format!("scattered {p}%"), p, false))
        .collect();
    if args.run_heavy_pct > 0.0 {
        variants.push((
            format!("run-heavy {}% (mean run {RUN_MEAN})", args.run_heavy_pct),
            args.run_heavy_pct,
            true,
        ));
    }

    eprintln!("identity column: {id_rows} rows");
    let t = Instant::now();
    let segment = synthetic_segment(id_rows, args.seed ^ 0x1D, &scratch);
    eprintln!("  written and mapped in {:.1} s", t.elapsed().as_secs_f64());

    let mut cells: Vec<Cell> = Vec::new();
    let mut leaves: Vec<LeafStats> = Vec::new();
    for (variant, coverage_pct, run_heavy) in &variants {
        for &shards in &args.shards {
            eprintln!("{variant}, N={shards}: building");
            let t = Instant::now();
            let map = build_treemap(shards, coverage_pct / 100.0, *run_heavy, args.seed);
            let mask = view_space_mask(&map, &scratch);
            eprintln!(
                "  built in {:.1} s, {} visible of {UNIVERSE}",
                t.elapsed().as_secs_f64(),
                map.cardinality()
            );
            for (s, leaf) in &map.leaves {
                let st = leaf.statistics();
                leaves.push(LeafStats {
                    variant: variant.clone(),
                    coverage_pct: *coverage_pct,
                    shards,
                    cardinality: leaf.cardinality(),
                    containers: st.n_containers,
                    array_containers: st.n_array_containers,
                    bitset_containers: st.n_bitset_containers,
                    run_containers: st.n_run_containers,
                });
                if *s == 0 {
                    eprintln!(
                        "  leaf 0: {} containers ({} array, {} bitset, {} run)",
                        st.n_containers,
                        st.n_array_containers,
                        st.n_bitset_containers,
                        st.n_run_containers
                    );
                }
            }
            let case = Case {
                variant,
                coverage_pct: *coverage_pct,
                map: &map,
                mask: &mask,
                segment: &segment,
                id_rows: id_rows as u64,
            };
            for depth in 0..=args.max_depth {
                let t = Instant::now();
                run_depth(&args, &case, depth, &mut cells);
                eprintln!("  depth {depth} done in {:.1} s", t.elapsed().as_secs_f64());
            }
        }
    }

    print_tables(&cells, &args.shards, args.max_depth);

    if let Some(out) = &args.out {
        let report = Report {
            cpu: cpu_model(),
            cores: std::thread::available_parallelism().map_or(0, |n| n.get()),
            threads: 1,
            universe_rows: UNIVERSE,
            id_rows,
            tiles: args.tiles,
            samples: args.samples,
            warmup: args.warmup,
            cap: args.cap,
            k_min: args.k_min,
            m_target: M_TARGET,
            seed: args.seed,
            leaves,
            cells,
        };
        std::fs::write(out, serde_json::to_string_pretty(&report).expect("serialise"))
            .expect("write result json");
        eprintln!("wrote {}", out.display());
    }

    drop(segment);
    let _ = std::fs::remove_dir_all(&scratch);
}
