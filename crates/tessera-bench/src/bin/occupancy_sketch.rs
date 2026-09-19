//! `N_occ(d)` by a **HyperLogLog sketch** against the two exact accumulators it would replace,
//! over one point set re-dealt into 1..512 segments.
//!
//! The fixture, the split shapes and the instruments are `occupancy_segments`'s, unchanged —
//! the corpus's own Morton column inverted through `tessera_spatial::unsplit32`, a visible set
//! defined over *points* so `N_occ` is constant along the segment axis, a counting
//! `#[global_allocator]` with `croaring::configure_rust_alloc` routing CRoaring's own `malloc`
//! through it, and peak **live** bytes rather than a before/after difference. What is new is the
//! measurement core: four complete routes — walk and accumulator together — priced against the
//! same mask in the same process, so the comparison is not between binaries.
//!
//! * **`union`** — `main`'s arm: a counter at one segment, a Roaring union over tile indices above
//!   that.
//! * **`tiered`** — `perf/nocc-tiered-accumulator`'s: a counter at one segment, a direct-mapped
//!   bitset over the whole `4^d` grid to depth 12, a compacting sorted buffer below that.
//! * **`flat`** — one sketch at the requested depth and nothing else, which is what a session pays
//!   per zoom level if the shallower rungs are discarded.
//! * **`ladder`** — `occupied_tiles_ladder`: one walk at the requested depth filling every depth at
//!   or below it, with the running maximum applied.
//!
//! The **session totals** are the sum over the depths swept, and they are the figure that decides
//! this: `union` and `tiered` pay a walk per depth however the session moves, `ladder zoom-in` pays
//! one per new *deepest* depth (the worst case, a session stepping down one level at a time), and
//! `ladder deepest-first` is the one walk a session that reaches the deepest level first pays for
//! the whole ladder.
//!
//! **Accuracy** is against `exact_ladder`, a linear scan over every visible row that shares no code
//! with either the walk or the sketch, checked at `--oracle` against the hash-set scan as well.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::compose::{compose, EffectiveMask};
use tessera_engine::projection::RowProjection;
use tessera_engine::occupancy::{
    for_each_occupied_tile, occupied_tiles_ladder_with_precision, TileSketch,
};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_spatial::unsplit32;
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, MortonCode, TermId, TesseraId};

#[derive(Parser)]
#[command(about = "N_occ(d): the multi-segment union arm against the single-segment counter")]
struct Args {
    /// A built segment's `morton.u32`. Its codes are the fixture's geometry.
    #[arg(long)]
    morton: PathBuf,
    /// Use only the first `--rows` codes (0 = all of them).
    #[arg(long, default_value_t = 0)]
    rows: usize,
    /// Segment counts to sweep.
    #[arg(long, value_delimiter = ',', default_values_t = [1usize, 2, 4, 8, 16, 64])]
    segments: Vec<usize>,
    /// Depths to sweep.
    #[arg(long, value_delimiter = ',', default_values_t = [0u8, 4, 8, 12, 16])]
    depths: Vec<u8>,
    /// The share of points the session may see.
    #[arg(long, default_value_t = 1.0)]
    visible: f64,
    #[arg(long, default_value_t = 0xA11CE)]
    seed: u64,
    /// How the row column is dealt into segments: `interleave`, `contiguous` or `flush`.
    #[arg(long, default_value = "interleave")]
    split: String,
    /// Rows per flush segment under `--split flush`. The base segment takes the remainder.
    #[arg(long, default_value_t = 10_000)]
    flush_rows: usize,
    /// Timed repetitions per cell.
    #[arg(long, default_value_t = 5)]
    repeats: usize,
    /// Also run the hash-set oracle at every cell, as the check on the exact ladder.
    #[arg(long, default_value_t = false)]
    oracle: bool,
    /// Sketch precisions to sweep. The engine's own must be one of them.
    #[arg(long, value_delimiter = ',', default_values_t = [12u32, 14])]
    precisions: Vec<u32>,
    /// Write the whole run as JSON here.
    #[arg(long)]
    json: Option<PathBuf>,
}

// ------------------------------------------------------------------------------------------
// Instruments
// ------------------------------------------------------------------------------------------

/// Live bytes, and the high-water mark of live bytes, while [`TRACKING`] is set.
///
/// **A peak, not a difference.** `occupied_tiles` frees its union before it returns, so before and
/// after are the same number whatever the union cost in between.
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are side effects on relaxed
// atomics and never influence the pointer returned.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            note_alloc(layout.size() as u64);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            note_alloc(layout.size() as u64);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            note_alloc(new_size as u64);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn note_alloc(size: u64) {
    TOTAL.fetch_add(size, Ordering::Relaxed);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` with the counters on, returning `(peak live bytes, total bytes allocated)`.
fn measuring<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    TOTAL.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let out = f();
    TRACKING.store(false, Ordering::Relaxed);
    (out, PEAK.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed))
}

// ------------------------------------------------------------------------------------------
// The fixture
// ------------------------------------------------------------------------------------------

/// One split of the point set: its segments, their row bases, and where each point landed.
struct SegmentLayout {
    _temps: Vec<TempDir>,
    segments: Vec<SegmentData>,
    bases: Vec<u32>,
    /// `row_of[p]` is the view-row-space row id of source point `p`.
    row_of: Vec<u32>,
    rows: u32,
}

impl SegmentLayout {
    fn with_bases(&self) -> Vec<(&SegmentData, u32)> {
        self.segments.iter().zip(self.bases.iter().copied()).collect()
    }
}

/// How a point set is dealt into segments.
#[derive(Clone, Copy, PartialEq)]
enum Split {
    /// Round-robin: every segment spans the whole extent.
    Interleave,
    /// Equal Morton ranges: the segments' extents barely meet.
    Contiguous,
    /// One base segment plus `parts - 1` of `flush_rows`, drawn from across the extent.
    Flush { flush_rows: usize },
}

/// Deal `0..n` into `parts` groups. Every group's indices ascend, so each segment's Morton column
/// ascends exactly as a written one does.
fn deal(n: usize, parts: usize, split: Split) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); parts];
    match split {
        Split::Interleave => {
            for i in 0..n {
                groups[i % parts].push(i);
            }
        }
        Split::Contiguous => {
            let each = n.div_ceil(parts);
            for i in 0..n {
                groups[(i / each).min(parts - 1)].push(i);
            }
        }
        Split::Flush { flush_rows } => {
            let flushes = parts - 1;
            let want = flush_rows * flushes;
            assert!(want < n, "the flush segments want more rows than the corpus has");
            // A stride pick spreads the flushed points over the whole extent, which is what a
            // window of arrivals is: the flushes take every `stride`-th point and the base keeps
            // the rest.
            let stride = n.checked_div(want).unwrap_or(0);
            let mut taken = 0usize;
            for i in 0..n {
                if stride > 0 && i % stride == 0 && taken < want {
                    groups[1 + taken % flushes].push(i);
                    taken += 1;
                } else {
                    groups[0].push(i);
                }
            }
        }
    }
    groups
}

/// Write `codes` as `parts` segments, recording where every source point landed.
fn build_layout(codes: &[u32], parts: usize, split: Split) -> SegmentLayout {
    let groups = deal(codes.len(), parts, split);
    let mut temps = Vec::new();
    let mut segments = Vec::new();
    let mut bases = Vec::new();
    let mut row_of = vec![u32::MAX; codes.len()];
    let mut base = 0u32;

    for (g, group) in groups.iter().enumerate() {
        // The axes are recovered from the stored code through the exact inverse of the split that
        // produced it, so `sort_batch` re-derives the same code for every point.
        let mut items: Vec<TilerItem> = group
            .iter()
            .map(|&p| {
                let (qx, qy) = unsplit32(MortonCode::new(codes[p]), 0);
                TilerItem {
                    // The source index rides as the identity, which is how the row order the
                    // segment was actually written in is read back below.
                    tessera_id: TesseraId::new(p as u64),
                    qx,
                    qy,
                    scalars: Vec::new(),
                }
            })
            .collect();
        let mut entity_ids: Vec<EntityId> = (0..items.len() as u64).map(EntityId::new).collect();
        let written = sort_batch(&mut items, &mut entity_ids);
        for (pos, item) in items.iter().enumerate() {
            row_of[item.tessera_id.raw() as usize] = base + pos as u32;
        }

        let temp = TempDir::new().expect("a temp dir for the segment");
        write_segment(temp.path(), &items, &written, &[]).expect("write_segment");
        let data = SegmentData {
            seg_id: format!("bench-{g}"),
            row_count: items.len() as u32,
            morton: MortonSlice::load(&temp.path().join("morton.u32")).expect("morton"),
            cuts: tessera_store::read::CutIndex::load(
                &temp.path().join(tessera_store::read::CutIndex::FILE),
                items.len() as u32,
            )
            .expect("cuts"),
            columns: ColumnsRef::load(&temp.path().join("columns.arrow")).expect("columns"),
        };
        bases.push(base);
        base += items.len() as u32;
        segments.push(data);
        temps.push(temp);
    }

    assert!(row_of.iter().all(|r| *r != u32::MAX), "every point took a row");
    SegmentLayout {
        _temps: temps,
        segments,
        bases,
        row_of,
        rows: base,
    }
}

/// An [`EffectiveMask`] over exactly `visible_rows` of a `row_count`-row space.
///
/// Built the way `tests/selection.rs` builds one: postings over an identity permutation, no
/// overlay and no buffer, so the composed mask's diffs are empty and the walk takes
/// `for_each_visible_run`'s in-place route — which is the route a deployment takes between denies.
fn mask_over(visible_rows: &[u32], row_count: u32) -> (TempDir, EffectiveMask) {
    let temp = TempDir::new().expect("a temp dir for the mask");
    let bound = row_count as u64;

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[visible_rows.to_vec()], 32).expect("write_postings");
    let postings = PostingsReader::open(&postings_path, false).expect("postings");

    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], 0, &postings, &[], bound)
        .expect("fragment");

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).expect("write_permutation");
    let perm = RowSpace::new(
        Arc::new(Permutation::load(&perm_path).expect("permutation")),
        row_count,
    );

    let base = Arc::new(RowProjection::walk(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let overlay = Overlay::default();
    let buffer = IngestBuffer::default();
    let denied = tessera_engine::denied_rows_of(&overlay, &perm);
    let mask = compose(&satisfied, &overlay, &buffer, base, &perm, &denied);
    (temp, mask)
}


// ------------------------------------------------------------------------------------------
// The exact reference, and the four routes under comparison
// ------------------------------------------------------------------------------------------

/// The exact `N_occ(d)` for every `d` in `0..=depth`, by a linear scan over every visible row.
///
/// Shares no code with the walk or the sketch: it reads each visible row's Morton code, shifts it
/// to each depth's tile index, and unions the results in a Roaring bitmap per depth. The
/// `last`-value guard is an optimisation over an idempotent `add`, so it cannot change an answer.
fn exact_ladder(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> Vec<u64> {
    let mut seen: Vec<Bitmap> = (0..=depth).map(|_| Bitmap::new()).collect();
    let mut last = vec![u64::MAX; depth as usize + 1];
    for (seg, base) in segments {
        let codes = seg.morton.u32();
        mask.for_each_visible_run(*base..*base + seg.row_count, |run| {
            for row in run.start..run.end {
                let code = u64::from(codes[(row - base) as usize]);
                for d in 0..=depth as usize {
                    let tile = code >> (32 - 2 * d as u32);
                    if tile != last[d] {
                        last[d] = tile;
                        seen[d].add(tile as u32);
                    }
                }
            }
        });
    }
    seen.iter().map(Bitmap::cardinality).collect()
}

/// `N_occ(depth)` by a full hash-set scan — the harness's oldest answer, kept as the check on
/// [`exact_ladder`] at a handful of cells.
fn oracle(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> u64 {
    let shift = 32 - 2 * u32::from(depth);
    let mut seen: FxHashSet<u64> = FxHashSet::default();
    for (seg, base) in segments {
        let codes = seg.morton.u32();
        mask.for_each_visible_run(*base..*base + seg.row_count, |run| {
            for row in run.start..run.end {
                seen.insert(u64::from(codes[(row - base) as usize]) >> shift);
            }
        });
    }
    seen.len() as u64
}

/// **`main`'s arm**: a plain counter at one segment, a Roaring union over tile indices above that.
fn route_union(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> u64 {
    match segments {
        [] => 0,
        [(segment, row_base)] => {
            let mut count = 0u64;
            for_each_occupied_tile(mask, segment, *row_base, depth, |_| count += 1);
            count
        }
        many => {
            let mut union = Bitmap::new();
            for (segment, row_base) in many {
                for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
                    union.add(tile as u32);
                });
            }
            union.cardinality()
        }
    }
}

/// The tiered branch's `BITSET_MAX_DEPTH`.
const TIER_BITSET_MAX_DEPTH: u8 = 12;
/// The tiered branch's `COMPACTION_SLACK`.
const TIER_COMPACTION_SLACK: usize = 4;
/// The tiered branch's `FIRST_COMPACTION`.
const TIER_FIRST_COMPACTION: usize = 1 << 16;

/// **`perf/nocc-tiered-accumulator`'s arm**: a counter at one segment, a direct-mapped bitset over
/// the whole `4^d` grid to depth 12, and a compacting sorted buffer below that.
fn route_tiered(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> u64 {
    match segments {
        [] => 0,
        [(segment, row_base)] => {
            let mut count = 0u64;
            for_each_occupied_tile(mask, segment, *row_base, depth, |_| count += 1);
            count
        }
        many if depth <= TIER_BITSET_MAX_DEPTH => {
            let words = 1usize << (2 * u32::from(depth)).saturating_sub(6);
            let mut seen = vec![0u64; words];
            for (segment, row_base) in many {
                for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
                    let tile = tile as usize;
                    seen[tile >> 6] |= 1u64 << (tile & 63);
                });
            }
            seen.iter().map(|w| u64::from(w.count_ones())).sum()
        }
        many => {
            let mut tiles: Vec<u32> = Vec::new();
            let mut compact_at = TIER_FIRST_COMPACTION;
            for (segment, row_base) in many {
                for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
                    tiles.push(tile as u32);
                    if tiles.len() >= compact_at {
                        tiles.sort();
                        tiles.dedup();
                        compact_at = tiles
                            .len()
                            .saturating_mul(TIER_COMPACTION_SLACK)
                            .max(TIER_FIRST_COMPACTION);
                    }
                });
            }
            tiles.sort();
            tiles.dedup();
            tiles.len() as u64
        }
    }
}

/// **The exact ladder, by the same one walk** — the sketch's real competitor for the
/// fill-every-shallower-depth claim, which is orthogonal to whether the count is exact.
///
/// The descent is `occupied_tiles_ladder`'s: the emissions ascend, so the depth-*d'* ancestor
/// changes exactly `N_occ_s(d')` times and a match at one depth means a match at every shallower
/// one. What differs is the accumulator behind each depth. One segment needs none at all — the
/// number of changes **is** the count, so seventeen `u64` counters are exact and free. Two or more
/// need the tiered branch's arms, seventeen of them at once: a direct-mapped bitset per depth to
/// 12 (2.8 MB for the stack of them) and a compacting sorted buffer above.
fn route_exact_ladder(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
) -> Vec<u64> {
    let levels = depth as usize + 1;
    let mut last = vec![u64::MAX; levels];
    let single = segments.len() <= 1;
    let mut counters = vec![0u64; levels];
    let mut bitsets: Vec<Vec<u64>> = if single {
        Vec::new()
    } else {
        (0..levels)
            .map(|d| {
                if d <= TIER_BITSET_MAX_DEPTH as usize {
                    vec![0u64; 1usize << (2 * d as u32).saturating_sub(6)]
                } else {
                    Vec::new()
                }
            })
            .collect()
    };
    let mut buffers: Vec<Vec<u32>> = if single {
        Vec::new()
    } else {
        (0..levels).map(|_| Vec::new()).collect()
    };
    let mut compact_at = vec![TIER_FIRST_COMPACTION; levels];

    for (segment, row_base) in segments {
        for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
            let mut d = depth as usize;
            loop {
                let ancestor = tile >> (2 * (depth as usize - d) as u32);
                if ancestor == last[d] {
                    break;
                }
                last[d] = ancestor;
                if single {
                    counters[d] += 1;
                } else if d <= TIER_BITSET_MAX_DEPTH as usize {
                    let t = ancestor as usize;
                    bitsets[d][t >> 6] |= 1u64 << (t & 63);
                } else {
                    buffers[d].push(ancestor as u32);
                    if buffers[d].len() >= compact_at[d] {
                        buffers[d].sort();
                        buffers[d].dedup();
                        compact_at[d] = buffers[d]
                            .len()
                            .saturating_mul(TIER_COMPACTION_SLACK)
                            .max(TIER_FIRST_COMPACTION);
                    }
                }
                if d == 0 {
                    break;
                }
                d -= 1;
            }
        });
    }

    (0..levels)
        .map(|d| {
            if single {
                counters[d]
            } else if d <= TIER_BITSET_MAX_DEPTH as usize {
                bitsets[d].iter().map(|w| u64::from(w.count_ones())).sum()
            } else {
                buffers[d].sort();
                buffers[d].dedup();
                buffers[d].len() as u64
            }
        })
        .collect()
}

/// **The sketch at one depth only** — no ladder, so this is what a session pays per zoom level if
/// the shallower rungs are thrown away.
fn route_sketch_flat(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    precision: u32,
) -> u64 {
    let mut sketch = TileSketch::with_precision(precision);
    for (segment, row_base) in segments {
        for_each_occupied_tile(mask, segment, *row_base, depth, |tile| sketch.add(tile));
    }
    sketch.estimate().min(1u64 << (2 * u32::from(depth)))
}

// ------------------------------------------------------------------------------------------

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// The minimum of `repeats` timed calls, after one discarded warm-up.
fn timed<T>(repeats: usize, mut f: impl FnMut() -> T) -> (u128, u128, T) {
    std::hint::black_box(f());
    let mut ts: Vec<u128> = Vec::with_capacity(repeats);
    let mut out = f();
    for _ in 0..repeats {
        let t0 = Instant::now();
        out = f();
        ts.push(t0.elapsed().as_nanos());
        std::hint::black_box(&out);
    }
    (*ts.iter().min().expect("a timed call"), median(ts.clone()), out)
}

fn main() {
    // Before any bitmap exists, so the union's containers are counted by the allocator above.
    // SAFETY: nothing in this process has allocated a CRoaring object yet.
    unsafe { croaring::configure_rust_alloc() };
    let args = Args::parse();
    let split = match args.split.as_str() {
        "interleave" => Split::Interleave,
        "contiguous" => Split::Contiguous,
        "flush" => Split::Flush {
            flush_rows: args.flush_rows,
        },
        other => {
            eprintln!("--split is 'interleave', 'contiguous' or 'flush', not '{other}'");
            std::process::exit(2);
        }
    };

    let slice = MortonSlice::load(&args.morton).expect("the morton column");
    let all = slice.u32();
    let take = if args.rows == 0 {
        all.len()
    } else {
        args.rows.min(all.len())
    };
    let codes: Vec<u32> = all[..take].to_vec();
    drop(slice);
    println!(
        "{} codes from {}, split {}, visible {:.4}, precision {:?}",
        codes.len(),
        args.morton.display(),
        args.split,
        args.visible,
        args.precisions
    );

    // The visible set, over source points. Deterministic, and the same set at every segment count.
    let visible_points: Vec<usize> = if args.visible >= 1.0 {
        (0..codes.len()).collect()
    } else {
        let cut = (args.visible * u64::MAX as f64) as u64;
        (0..codes.len())
            .filter(|&p| {
                // SplitMix64 over the point index: deterministic, and uncorrelated with the code.
                let mut z = (p as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ args.seed;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                (z ^ (z >> 31)) < cut
            })
            .collect()
    };
    println!("visible points: {}", visible_points.len());

    let deepest = *args.depths.iter().max().expect("a depth to sweep");
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut sessions: Vec<serde_json::Value> = Vec::new();
    // Depth -> the exact N_occ every segment count must agree on.
    let mut expected: BTreeMap<u8, u64> = BTreeMap::new();

    println!();
    println!(
        "{:>5} {:>5} {:>11} {:>11} {:>8} {:>11} {:>11} {:>11} {:>11} {:>11} {:>9} {:>9}",
        "segs", "depth", "exact", "sketch", "err %", "union ns", "tiered ns", "exactlad ns",
        "flat ns", "ladder ns", "ladder kB", "exlad kB"
    );

    for &parts in &args.segments {
        let layout = build_layout(&codes, parts, split);
        let visible_rows: Vec<u32> = {
            let mut v: Vec<u32> = visible_points.iter().map(|&p| layout.row_of[p]).collect();
            v.sort_unstable();
            v
        };
        let (_mask_temp, mask) = mask_over(&visible_rows, layout.rows);
        let segments = layout.with_bases();
        let shape: Vec<u32> = layout.segments.iter().map(|s| s.row_count).collect();
        let head: Vec<String> = shape.iter().take(3).map(u32::to_string).collect();
        println!(
            "-- {parts} segment(s), rows {}{}",
            head.join(" + "),
            if shape.len() > 3 {
                format!(" + … ({} more)", shape.len() - 3)
            } else {
                String::new()
            }
        );

        // The exact ladder, once per cell, for every depth this run touches.
        let exact = exact_ladder(&mask, &segments, deepest);
        if args.oracle {
            for &depth in &args.depths {
                let got = oracle(&mask, &segments, depth);
                assert_eq!(
                    got, exact[depth as usize],
                    "depth {depth}, {parts} segments: the exact ladder answered {} and the \
                     hash-set oracle {got}",
                    exact[depth as usize]
                );
            }
        }

        // The ladder, once per depth, so the running total over a session's depths can be summed
        // from the same measurements the per-cell rows report.
        let mut union_total = 0u128;
        let mut tiered_total = 0u128;
        let mut flat_total = 0u128;
        let mut ladder_total = 0u128;
        let mut ladder_deepest = 0u128;
        let mut exact_ladder_total = 0u128;
        let mut exact_ladder_deepest = 0u128;

        for &depth in &args.depths {
            let exact_d = exact[depth as usize];
            match expected.get(&depth) {
                None => {
                    expected.insert(depth, exact_d);
                }
                Some(&want) => assert_eq!(
                    exact_d, want,
                    "depth {depth}: {parts} segments answered {exact_d} where the point set's \
                     N_occ is {want} — a split changed the answer"
                ),
            }

            let (union_ns, union_med, union_n) =
                timed(args.repeats, || route_union(&mask, &segments, depth));
            assert_eq!(union_n, exact_d, "the union arm disagreed at depth {depth}");
            let (tiered_ns, tiered_med, tiered_n) =
                timed(args.repeats, || route_tiered(&mask, &segments, depth));
            assert_eq!(tiered_n, exact_d, "the tiered arm disagreed at depth {depth}");

            let mut per_precision = serde_json::Map::new();
            let mut headline: Option<(u64, u128, u128, u64)> = None;
            for &precision in &args.precisions {
                let (ladder_ns, _, ladder) = timed(args.repeats, || {
                    occupied_tiles_ladder_with_precision(&mask, &segments, depth, precision)
                });
                let (_, ladder_peak, _) = measuring(|| {
                    occupied_tiles_ladder_with_precision(&mask, &segments, depth, precision)
                });
                let est = ladder.at(depth);
                // Monotonicity, before and after the running maximum, over every rung this one
                // walk filled — one build, not a rebuild per depth.
                let raw: Vec<u64> = (0..=depth).map(|d| ladder.raw_at(d)).collect();
                let served: Vec<u64> = (0..=depth).map(|d| ladder.at(d)).collect();
                let raw_inversions = raw.windows(2).filter(|w| w[1] < w[0]).count();
                let served_inversions = served.windows(2).filter(|w| w[1] < w[0]).count();
                let errors: Vec<f64> = (0..=depth as usize)
                    .map(|d| {
                        if exact[d] == 0 {
                            0.0
                        } else {
                            100.0 * (served[d] as f64 - exact[d] as f64) / exact[d] as f64
                        }
                    })
                    .collect();
                let (flat_ns, flat_peak) = if precision == tessera_engine::occupancy::SKETCH_PRECISION
                {
                    let (ns, _, _) = timed(args.repeats, || {
                        route_sketch_flat(&mask, &segments, depth, precision)
                    });
                    let (_, peak, _) =
                        measuring(|| route_sketch_flat(&mask, &segments, depth, precision));
                    (ns, peak)
                } else {
                    (0, 0)
                };
                per_precision.insert(
                    precision.to_string(),
                    serde_json::json!({
                        "ladder_estimate": est,
                        "ladder_ns": ladder_ns,
                        "ladder_peak_bytes": ladder_peak,
                        "flat_ns": flat_ns,
                        "flat_peak_bytes": flat_peak,
                        "raw_inversions_below": raw_inversions,
                        "served_inversions_below": served_inversions,
                        "raw_ladder": raw,
                        "served_ladder": served,
                        "error_pct_ladder": errors,
                    }),
                );
                if precision == tessera_engine::occupancy::SKETCH_PRECISION {
                    headline = Some((est, flat_ns, ladder_ns, ladder_peak));
                }
            }
            let (est, flat_ns, ladder_ns, ladder_peak) =
                headline.expect("the engine's own precision must be in the sweep");

            let (exact_ladder_ns, _, exact_rungs) =
                timed(args.repeats, || route_exact_ladder(&mask, &segments, depth));
            assert_eq!(
                exact_rungs[depth as usize], exact_d,
                "the exact ladder route disagreed at depth {depth}"
            );
            let (_, exact_ladder_peak, _) =
                measuring(|| route_exact_ladder(&mask, &segments, depth));

            let (_, union_peak, _) = measuring(|| route_union(&mask, &segments, depth));
            let (_, tiered_peak, _) = measuring(|| route_tiered(&mask, &segments, depth));

            union_total += union_ns;
            tiered_total += tiered_ns;
            flat_total += flat_ns;
            ladder_total += ladder_ns;
            exact_ladder_total += exact_ladder_ns;
            if depth == deepest {
                ladder_deepest = ladder_ns;
                exact_ladder_deepest = exact_ladder_ns;
            }

            let err = if exact_d == 0 {
                0.0
            } else {
                100.0 * (est as f64 - exact_d as f64) / exact_d as f64
            };
            println!(
                "{parts:>5} {depth:>5} {exact_d:>11} {est:>11} {err:>7.2}% {union_ns:>11} \
                 {tiered_ns:>11} {exact_ladder_ns:>11} {flat_ns:>11} {ladder_ns:>11} {:>9} {:>9}",
                ladder_peak / 1024,
                exact_ladder_peak / 1024
            );

            rows.push(serde_json::json!({
                "segments": parts,
                "depth": depth,
                "exact": exact_d,
                "estimate": est,
                "error_pct": err,
                "union_ns": union_ns,
                "union_ns_median": union_med,
                "tiered_ns": tiered_ns,
                "tiered_ns_median": tiered_med,
                "union_peak_bytes": union_peak,
                "tiered_peak_bytes": tiered_peak,
                "exact_ladder_ns": exact_ladder_ns,
                "exact_ladder_peak_bytes": exact_ladder_peak,
                "precisions": per_precision,
            }));
        }

        println!(
            "   session totals, depths {:?}: union {:.3} ms, tiered {:.3} ms, sketch-per-depth \
             {:.3} ms, sketch-ladder zoom-in {:.3} ms, sketch-ladder deepest-first {:.3} ms, \
             exact-ladder zoom-in {:.3} ms, exact-ladder deepest-first {:.3} ms",
            args.depths,
            union_total as f64 / 1e6,
            tiered_total as f64 / 1e6,
            flat_total as f64 / 1e6,
            ladder_total as f64 / 1e6,
            ladder_deepest as f64 / 1e6,
            exact_ladder_total as f64 / 1e6,
            exact_ladder_deepest as f64 / 1e6,
        );
        sessions.push(serde_json::json!({
            "segments": parts,
            "depths": args.depths,
            "union_total_ns": union_total,
            "tiered_total_ns": tiered_total,
            "sketch_per_depth_total_ns": flat_total,
            "ladder_zoom_in_total_ns": ladder_total,
            "ladder_deepest_first_ns": ladder_deepest,
            "exact_ladder_zoom_in_total_ns": exact_ladder_total,
            "exact_ladder_deepest_first_ns": exact_ladder_deepest,
        }));
    }

    if let Some(path) = &args.json {
        let doc = serde_json::json!({
            "morton": args.morton,
            "rows": codes.len(),
            "visible_points": visible_points.len(),
            "visible_fraction": args.visible,
            "split": args.split,
            "repeats": args.repeats,
            "precisions": args.precisions,
            "cells": rows,
            "sessions": sessions,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap() + "\n")
            .expect("write the json");
        println!("\nwrote {}", path.display());
    }
}
