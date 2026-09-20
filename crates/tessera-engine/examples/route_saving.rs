//! Measure what §7.2's `AllVisible` fast path actually saves, as a function of `cap`.
//!
//! The question this answers: an independent review costed the fast path at ~60 µs per viewport and
//! recommended deleting it, but that figure was taken at `cap <= 200`. The fast path fires when
//! `V <= cap`, so raising `cap` both widens *which* tiles qualify and deepens the work skipped on
//! each — the saving should scale with `cap`, and the review's conclusion may not survive the mark
//! budgets this deployment is heading for.
//!
//! Method: build one synthetic segment of `TILES * V` visible rows and treat each contiguous
//! `V`-row span as a tile. Selection takes a `Range<u32>` and a visible count — it does not care
//! that the range came from `tile_ranges` — so constructing the ranges directly measures the
//! per-tile work without a geometry fixture in the way.
//!
//! The branch is a private implementation detail (it is not selectable through the API, deliberately
//! — both branches compute the same definition), so this drives it through `Selection::of` by
//! choosing `V`: with θ saturated, `V = cap` serves everything and skips the counting pass, while
//! `V = cap + 1` must count and select. The two differ by one row of work out of `cap`, which is
//! under 1/cap of the total and far below the effect being measured.
//!
//! Run: `cargo run --release --example route_saving -p tessera-engine`
//!
//! **Reports the minimum of [`TRIALS`] trials, not a single timing or a mean.** This is a
//! microbenchmark on a shared box: a competing load (another bench, a test suite, a build) inflates
//! individual timings by 2-4x and a mean carries that straight into the result, while a single shot
//! can land anywhere. The minimum is the standard robust estimator for "how fast can this go" — it
//! discards scheduler interference rather than averaging it in. Check `uptime` before trusting a
//! reading anyway: no estimator rescues a measurement taken against a saturated CPU.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::compose::{compose, EffectiveMask};
use tessera_engine::projection::RowProjection;
use tessera_engine::select::{SelectParams, Selection, SelectionPart, SelectionParts, Threshold};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_spatial::{fixed32, tiler::sort_batch, Bounds, TilerItem};
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, TermId, TesseraId};

const EXTENT: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1024.0,
    y_min: 0.0,
    y_max: 1024.0,
};

/// A realistic viewport: a few hundred tiles.
const TILES: usize = 300;

/// Timing trials per configuration; the minimum is reported. See the module doc.
const TRIALS: u32 = 7;

/// splitmix64, so identities are uniform without pulling in the real keyed bijection.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() {
    println!(
        "{:>6}  {:>10}  {:>12}  {:>12}  {:>12}  {:>9}",
        "cap", "V/tile", "serve-all", "selecting", "saving", "saving %"
    );
    println!("{}", "-".repeat(72));

    for cap in [30usize, 100, 500, 1000] {
        let params = SelectParams {
            k_min: 2,
            cap,
            threshold: Threshold::Saturated,
        };

        // V = cap serves everything (no counting pass); V = cap + 1 must count and select. That one
        // extra row is the only difference in input, and it is under 1/cap of the work.
        let mut timings = Vec::new();
        for v_per_tile in [cap, cap + 1] {
            let (temp, seg, mask) = fixture(v_per_tile);
            let _ = &temp;

            // Each contiguous V-row span stands for one tile. Every one must genuinely take the
            // fast path, or this measures nothing.
            let ranges: Vec<(std::ops::Range<u32>, u64)> = (0..TILES)
                .map(|i| {
                    let start = (i * v_per_tile) as u32;
                    let r = start..start + v_per_tile as u32;
                    let vis = mask.count_range(r.clone());
                    (r, vis)
                })
                .collect();
            assert!(
                ranges.iter().all(|(_, vis)| *vis == v_per_tile as u64),
                "fixture must put exactly {v_per_tile} visible rows in every tile"
            );
            // Under saturation, serving everything means served == visible; needing selection means
            // served == cap < visible. Asserting this is what proves the two arms of the sweep are
            // actually hitting different branches.
            let served = Selection::of(
                &mask,
                &SelectionParts::new(&[SelectionPart::base(
                    &seg,
                    ranges[0].0.clone(),
                    ranges[0].1,
                )]),
                &params,
                ranges[0].1,
            )
            .rows
            .len();
            if v_per_tile == cap {
                assert_eq!(
                    served, v_per_tile,
                    "expected the serve-all branch at V = cap"
                );
            } else {
                assert_eq!(served, cap, "expected the selecting branch at V = cap + 1");
            }

            timings.push(best_of(TRIALS, 50, &ranges, &mask, &seg, &params));
        }

        let (fast, general) = (timings[0], timings[1]);
        let saving = general as i128 - fast as i128;
        let pct = if general > 0 {
            saving as f64 / general as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "{cap:>6}  {:>10}  {:>10} µs  {:>10} µs  {:>10} µs  {pct:>8.1}%",
            cap,
            fast / 1000,
            general / 1000,
            saving / 1000
        );
    }
    println!(
        "\nPer viewport of {TILES} tiles, release build. `saving` is what deleting the fast path\n\
         would cost; compare against the 10 ms p99 exit criterion."
    );
}

/// The fastest of `trials` timings, each averaging `reps` viewports.
fn best_of(
    trials: u32,
    reps: u32,
    ranges: &[(std::ops::Range<u32>, u64)],
    mask: &EffectiveMask,
    seg: &SegmentData,
    params: &SelectParams,
) -> u128 {
    (0..trials)
        .map(|_| time(reps, ranges, mask, seg, params))
        .min()
        .expect("TRIALS must be non-zero")
}

fn time(
    reps: u32,
    ranges: &[(std::ops::Range<u32>, u64)],
    mask: &EffectiveMask,
    seg: &SegmentData,
    params: &SelectParams,
) -> u128 {
    // Warm.
    for (r, vis) in ranges {
        black_box(Selection::of(
            mask,
            &SelectionParts::new(&[SelectionPart::base(seg, r.clone(), *vis)]),
            params,
            *vis,
        ));
    }
    let t0 = Instant::now();
    for _ in 0..reps {
        for (r, vis) in ranges {
            black_box(Selection::of(
                mask,
                &SelectionParts::new(&[SelectionPart::base(seg, r.clone(), *vis)]),
                params,
                *vis,
            ));
        }
    }
    t0.elapsed().as_nanos() / reps as u128
}

/// A segment of `TILES * v_per_tile` rows, all visible.
///
/// Geometry is irrelevant to what is being measured (selection consumes a row range and a visible
/// count), so the points are placed on a simple diagonal — enough to give every row a distinct
/// Morton code so `sort_batch` produces a total order, and nothing more.
fn fixture(v_per_tile: usize) -> (TempDir, SegmentData, EffectiveMask) {
    let temp = TempDir::new().unwrap();
    let total = TILES * v_per_tile;

    let mut items: Vec<TilerItem> = Vec::with_capacity(total);
    for n in 0..total as u64 {
        let fx = (mix(n) >> 40) as f64 / (1u64 << 24) as f64;
        let fy = (mix(n ^ 0xABCD) >> 40) as f64 / (1u64 << 24) as f64;
        items.push(TilerItem {
            tessera_id: TesseraId::new(mix(n ^ 0x5EED)),
            qx: fixed32(fx * 1023.0, EXTENT.x_min, EXTENT.x_max),
            qy: fixed32(fy * 1023.0, EXTENT.y_min, EXTENT.y_max),
            scalars: Vec::new(),
        });
    }

    let mut entity_ids: Vec<EntityId> = (0..items.len() as u64).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);
    write_segment(temp.path(), &items, &codes, &[]).unwrap();
    let seg = SegmentData {
        seg_id: "seg0".into(),
        row_count: items.len() as u32,
        morton: MortonSlice::load(&temp.path().join("morton.u32")).unwrap(),
        cuts: tessera_store::read::CutIndex::load(
            &temp.path().join(tessera_store::read::CutIndex::FILE),
            items.len() as u32,
        )
        .unwrap(),
        columns: ColumnsRef::load(&temp.path().join("columns.arrow")).unwrap(),
    };

    let bound = items.len() as u64;
    let all: Vec<u32> = (0..items.len() as u32).collect();
    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[all], 32).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();
    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], &postings, &[], bound)
        .unwrap();
    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).unwrap();
    let perm = RowSpace::new(
        std::sync::Arc::new(Permutation::load(&perm_path).unwrap()),
        bound as u32,
    );
    let base = Arc::new(RowProjection::walk(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let mask = compose(
        &satisfied,
        &Overlay::default(),
        &IngestBuffer::default(),
        base,
        &perm,
        // Nothing denied — this example measures the selection route.
        &croaring::Bitmap::new(),
    );
    (temp, seg, mask)
}
