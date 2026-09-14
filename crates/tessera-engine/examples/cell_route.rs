//! The measured basis for `select.rs`'s per-cell cost table — re-runnable evidence, in the same
//! spirit as `decode_tiers.rs` and `route_saving.rs`.
//!
//! Selection evaluates a dense tile per leaf Morton cell rather than per visible row: identities
//! ascend within a cell, so `C_θ` there is a prefix length and the cell's smallest identities are
//! the head of that prefix. Whether that is cheaper than reading the rows depends on one property
//! of the corpus — how many rows a cell holds — and this sweeps it.
//!
//! Two arms over identical inputs:
//!
//! - `scan` — the mechanism the route replaced, transcribed: one contiguous identity slice per
//!   part, a branchless filter-count for the threshold, then the peek-reject heap over every
//!   visible row. This is the `scan_slice` form `select.rs` carried for the two dense tiers.
//! - `route` — `Selection::of` exactly as shipped, so the figure is the code that runs and not a
//!   model of it.
//!
//! The two must return the same rows at every point of the sweep; the example asserts it, because
//! a cost table for a route that answers differently is a table for nothing.
//!
//! **What to read off it.** `ns/row` for the scan is flat in cells (it reads rows), `ns/cell` for
//! the route is flat in rows a cell (it reads cells), and the break-even is where the route's
//! per-cell cost meets the scan's per-row cost times the rows in a cell. The two GBIF corpora in
//! `select.rs`'s table sit either side of it: 7.4 rows a cell at 25.8M rows, 83.4 at 3.50G.
//!
//! The absolute figures are one box's and the ratio is what travels. The threshold is the real
//! depth-0 cut for this row count (`Threshold::at_depth`, `m_target = 16`, `N_occ = 1`), which is
//! the whole-map zoom the route exists for; a cut that admitted most identities would put the
//! count's binary search on its slow end at every cell and measure a case no viewport asks for.
//!
//! Run with: `cargo run --release --example cell_route -p tessera-engine`

use std::collections::BinaryHeap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::compose::{compose, EffectiveMask, RowProjection};
use tessera_engine::select::{
    decode_tier, DecodeTier, SelectParams, Selection, SelectionPart, SelectionParts, Threshold,
};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_spatial::{fixed32, tiler::sort_batch, Bounds, TilerItem};
use tessera_store::read::{ColumnsRef, CutIndex, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, TermId, TesseraId};

const ROWS: u32 = 1 << 20;
const EXTENT: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1024.0,
    y_min: 0.0,
    y_max: 1024.0,
};
/// `min(k, k_max_marks)`. Two of them: the client default and the operating point, to show the
/// break-even is not a function of the cap.
const CAPS: [usize; 2] = [30, 500];
const TRIALS: u32 = 5;
const REPS: u32 = 3;

fn main() {
    // The depth-0 cut this row count really produces: one occupied tile, `m_target = 16`.
    let threshold = Threshold::at_depth(u64::from(ROWS), 16, 1);
    println!(
        "{ROWS} rows, depth-0 threshold {threshold:?}, best of {TRIALS}x{REPS}\n\
         'route' is Selection::of as shipped; 'scan' is the per-row mechanism it replaced.\n"
    );

    for cap in CAPS {
        println!("cap = {cap}");
        println!(
            "{:>9} {:>9} {:>11} {:>11} {:>11} {:>11}",
            "rows/cell", "cells", "scan ns/row", "route ns/c", "route ns/r", "route/scan"
        );
        for rows_per_cell in [1u32, 2, 4, 8, 16, 32, 64, 128] {
            let cells = ROWS / rows_per_cell;
            let seg = segment_of(rows_per_cell);
            // Every row visible: the whole-range tier, which is the zoom-0 whole-extent request
            // the change was measured on.
            let visible: Vec<u32> = (0..ROWS).collect();
            let (_t, mask) = mask_over(&visible, ROWS);
            let range = 0..ROWS;
            let vis = mask.count_range(range.clone());
            assert_eq!(
                decode_tier(vis, u64::from(ROWS)),
                DecodeTier::FullRange,
                "the sweep means to measure the dense route"
            );
            let params = SelectParams {
                k_min: 1,
                cap,
                threshold,
            };
            let parts = [SelectionPart::base(&seg.data, range.clone(), vis)];
            let parts = SelectionParts::new(&parts);

            // Same answer, or the figures below compare two different computations.
            let route_rows = Selection::of(&mask, &parts, &params, vis).rows;
            let scan_rows = scan(&seg, &range, &params, vis).0;
            assert_eq!(
                route_rows, scan_rows,
                "route and scan disagree at {rows_per_cell} rows a cell"
            );

            let scan_ns = best_of(|| {
                let (rows, c) = scan(&seg, &range, &params, vis);
                rows.len() as u64 ^ c
            });
            let route_ns = best_of(|| {
                let out = Selection::of(&mask, &parts, &params, vis);
                out.rows.len() as u64 ^ out.rows_visited
            });

            println!(
                "{rows_per_cell:>9} {cells:>9} {:>11.3} {:>11.3} {:>11.3} {:>11.2}",
                scan_ns as f64 / f64::from(ROWS),
                route_ns as f64 / f64::from(cells),
                route_ns as f64 / f64::from(ROWS),
                route_ns as f64 / scan_ns as f64,
            );
        }
        println!();
    }
}

fn best_of(mut f: impl FnMut() -> u64) -> u128 {
    black_box(f()); // warm
    (0..TRIALS)
        .map(|_| {
            let t0 = Instant::now();
            for _ in 0..REPS {
                black_box(f());
            }
            t0.elapsed().as_nanos() / REPS as u128
        })
        .min()
        .expect("TRIALS is non-zero")
}

/// The mechanism the cell route replaced, transcribed: a branchless filter-count over the tile's
/// contiguous identity slice, then the peek-reject heap over every visible row.
///
/// Every row of the range is visible in this example, which is what lets the arm be a plain slice
/// scan — the whole-range tier's shape, and the one the route competes with.
fn scan(
    seg: &Segment,
    range: &std::ops::Range<u32>,
    params: &SelectParams,
    visible: u64,
) -> (Vec<u32>, u64) {
    let ids = &seg.data.columns.tessera_id()[range.start as usize..range.end as usize];
    let mut c_theta: u64 = 0;
    match params.threshold {
        Threshold::Saturated => c_theta += ids.len() as u64,
        Threshold::Cut(cut) => c_theta += ids.iter().filter(|&&id| id < cut).count() as u64,
    }
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(params.cap + 1);
    for (i, &id) in ids.iter().enumerate() {
        if heap.len() == params.cap {
            if id >= heap.peek().expect("non-empty at len == cap").0 {
                continue;
            }
            heap.pop();
        }
        heap.push((id, range.start + i as u32));
    }
    let floor = params.k_min.min(params.cap);
    let m = params
        .cap
        .min(floor.max(usize::try_from(c_theta).unwrap_or(usize::MAX)))
        .min(usize::try_from(visible).unwrap_or(usize::MAX));
    let mut kept: Vec<(u64, u32)> = heap.into_vec();
    kept.sort_unstable();
    kept.truncate(m);
    (kept.into_iter().map(|(_, row)| row).collect(), c_theta)
}

/// A segment plus the temp dir backing its mmaps, which must outlive it.
struct Segment {
    _temp: TempDir,
    data: SegmentData,
}

/// A segment of [`ROWS`] rows laid out `rows_per_cell` to each occupied leaf Morton cell.
///
/// The positions are a lattice so the count is exact rather than distributional: a cell's row
/// count is what the sweep's x-axis says it is, not what a random scatter happened to produce.
/// Identities are random, and `sort_batch` puts them in the `(morton, tessera_id)` order a real
/// segment has — which is the order the route reads them in.
fn segment_of(rows_per_cell: u32) -> Segment {
    let mut rng = StdRng::seed_from_u64(0xCE11_u64 * u64::from(rows_per_cell) + 7);
    let positions = ROWS / rows_per_cell;
    let side = (f64::from(positions).sqrt().ceil()) as u32;
    let step = 1024.0f32 / side as f32;
    let temp = TempDir::new().unwrap();
    let mut items: Vec<TilerItem> = (0..ROWS)
        .map(|i| {
            let cell = i % positions;
            let (x, y) = ((cell % side) as f32 * step, (cell / side) as f32 * step);
            TilerItem {
                tessera_id: TesseraId::new(rng.gen()),
                qx: fixed32(f64::from(x), EXTENT.x_min, EXTENT.x_max),
                qy: fixed32(f64::from(y), EXTENT.y_min, EXTENT.y_max),
                scalars: Vec::new(),
            }
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..u64::from(ROWS)).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);
    write_segment(temp.path(), &items, &codes, &[]).unwrap();

    let data = SegmentData {
        seg_id: "cell-route".to_string(),
        row_count: ROWS,
        morton: MortonSlice::load(&temp.path().join("morton.u32")).unwrap(),
        cuts: CutIndex::load(&temp.path().join(CutIndex::FILE), ROWS).unwrap(),
        columns: ColumnsRef::load(&temp.path().join("columns.arrow")).unwrap(),
    };
    assert_eq!(
        data.cuts.starts().len() as u32,
        positions,
        "the lattice must put exactly {rows_per_cell} rows in each of {positions} cells"
    );
    Segment { _temp: temp, data }
}

/// An [`EffectiveMask`] over exactly `visible_rows`, with no overlay or buffer effects — the same
/// construction `tests/selection.rs` and `decode_tiers.rs` use.
fn mask_over(visible_rows: &[u32], row_count: u32) -> (TempDir, EffectiveMask) {
    let temp = TempDir::new().unwrap();
    let bound = u64::from(row_count);

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[visible_rows.to_vec()], 32).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], 0, &postings, &[], bound)
        .unwrap();

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).unwrap();
    let perm = RowSpace::new(Arc::new(Permutation::load(&perm_path).unwrap()), row_count);

    let base = Arc::new(RowProjection::new(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let overlay = Overlay::default();
    let buffer = IngestBuffer::default();
    let denied = tessera_engine::denied_rows_of(&overlay, &perm);
    let mask = compose(&satisfied, &overlay, &buffer, base, &perm, &denied);
    (temp, mask)
}
