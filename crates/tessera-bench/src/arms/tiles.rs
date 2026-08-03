//! **Ask 2: Morton cell generation as a function of points-in-cell, k, and overall point count.**
//!
//! The tile half of that ask: `tiles_for_bbox` -> `Tile::code_range` -> `tile_ranges` ->
//! `count_range`, which is design §2.6's retrieve steps 5 and 6. The *k* half lives in the gather
//! probe, where access pattern rather than sampler identity is the axis.
//!
//! **Zoom 0..=3 is included deliberately.** Every existing harness sweeps 4..12. The probes
//! measured counting as nearly free at depth 0 (a billion rows in 191 us) because bitmap cost is
//! O(containers touched) rather than O(cardinality) — but that was the *count*, and the whole
//! selection path below it was never measured at all. The coarse zooms are where a tile holds the
//! most points, so they are where the "points in cell" axis is actually exercised.
//!
//! **The claim under test** is scaling-analysis §1's: per-tile cost depends on tile capacity and
//! coverage, *not* on corpus size. Emitting `ns_per_tile` and `ns_per_container` for every scale
//! is what makes that falsifiable rather than assumed.

use std::sync::Arc;

use tessera_authz::PostingsReader;
use tessera_engine::compose::RowProjection;
use tessera_engine::{compose, EffectiveMask};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_spatial::{tiles_for_bbox, Extent};
use tessera_store::read::open_bundle;
use tessera_store::tile_ranges_all;

use crate::arms::{Context, Result};
use crate::corpus::{build_grant_to_coverage, GrantShape, TermStats};
use crate::report::Work;

pub fn run(ctx: &Context, zooms: &[u8], coverages: &[f64], seed: u64) -> Result<()> {
    let mut run = ctx.open("tiles")?;

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let bundle = open_bundle(&fixture.root)?;

        let Some(partition) = bundle.partitions.values().next() else {
            continue;
        };
        let Some(slice) = partition.slices.values().next() else {
            continue;
        };
        let Some(segment) = slice.segments.first() else {
            continue;
        };

        let q = &bundle.manifest.quantisation;
        let extent = Extent {
            x_min: q.x_min,
            x_max: q.x_max,
            y_min: q.y_min,
            y_max: q.y_max,
        };
        let full_bbox = [q.x_min, q.y_min, q.x_max, q.y_max];

        for &target in coverages {
            let (grant, coverage) = build_grant_to_coverage(
                &stats,
                &postings,
                GrantShape::Random,
                target,
                fixture.scale,
                seed,
            )?;
            if grant.terms.is_empty() {
                continue;
            }

            // The mask is built ONCE, outside every timed loop. `RowProjection::new` crosses
            // entity space into row space over the whole fragment (I4's only bridge) and was
            // measured at 8.8 s for a 69M-item mask — if it ever drifts onto a per-viewport
            // path the system is dead (probes/optimisations.md §3.2). It gets its own arm; it
            // does not get to contaminate this one.
            let fragment = crate::postings::union(&postings, &grant.terms)?;
            let containers = crate::metrics::containers(&fragment);
            let frozen = frozen_fragment(&postings, &grant.terms)?;
            let projection = Arc::new(RowProjection::new(&frozen, &slice.row_space));
            let satisfied: rustc_hash::FxHashSet<tessera_types::TermId> =
                grant.terms.iter().copied().collect();

            // Composition is outside the timed loop too: this arm measures tile enumeration and
            // counting, and `compose`'s own cost (linear in overlay + buffer size) is a different
            // question with a different arm. With an empty overlay and buffer the effective mask
            // is the projection, which is exactly what a steady-state read path sees.
            let mask: EffectiveMask = compose(
                &satisfied,
                &Overlay::new(),
                &IngestBuffer::new(),
                Arc::clone(&projection),
                &slice.row_space,
            );

            for &zoom in zooms {
                let cell_id = format!(
                    "tiles/{}/{}/cov{:.4}/z{}",
                    fixture.scale, fixture.label_set, target, zoom
                );
                if run.ledger.is_done(&cell_id) {
                    run.skipped += 1;
                    continue;
                }

                let tiles = tiles_for_bbox(full_bbox, zoom, &extent);

                // `tile_ranges_all`, not a per-tile `tile_ranges` loop. This arm's whole claim is
                // that it measures design §2.6's retrieve steps 5 and 6 *as the engine performs
                // them*, and the engine resolves a viewport's tiles in one galloping sweep. Timing
                // the per-tile primitive instead would report a number that no request pays — and
                // would have kept reporting the pre-sweep cost as though nothing had changed.
                let samples = crate::metrics::repeat(ctx.repeat, || {
                    let mut sigma = 0u64;
                    let mut spanned = 0u64;
                    let mut nonempty = 0u64;
                    for range in tile_ranges_all(segment, &tiles) {
                        spanned += range.len() as u64;
                        let visible = mask.count_range(range);
                        if visible > 0 {
                            nonempty += 1;
                            sigma += visible;
                        }
                    }
                    (sigma, spanned, nonempty)
                });

                // Re-derive the counters once, unmeasured.
                let (mut sigma, mut spanned, mut nonempty) = (0u64, 0u64, 0u64);
                for range in tile_ranges_all(segment, &tiles) {
                    spanned += range.len() as u64;
                    let visible = mask.count_range(range);
                    if visible > 0 {
                        nonempty += 1;
                        sigma += visible;
                    }
                }

                let work = Work {
                    containers,
                    mask_cardinality: fragment.cardinality(),
                    coverage,
                    run_ratio: crate::metrics::run_ratio(&fragment, fixture.scale),
                    tiles_resolved: tiles.len() as u64,
                    tiles_nonempty: nonempty,
                    sigma_visible: sigma,
                    rows_in_ranges: spanned,
                    degenerate: coverage >= 0.999,
                    ..Default::default()
                };

                run.emit(
                    cell_id,
                    fixture,
                    serde_json::json!({
                        "zoom": zoom,
                        "coverage_target": target,
                        "coverage_actual": coverage,
                        "w": grant.terms.len(),
                        "seed": seed,
                        // The "points in cell" axis, made explicit rather than left implicit in
                        // the zoom: mean rows spanned per tile that had anything visible.
                        "mean_rows_per_nonempty_tile":
                            if nonempty > 0 { spanned as f64 / nonempty as f64 } else { 0.0 },
                        "mean_visible_per_nonempty_tile":
                            if nonempty > 0 { sigma as f64 / nonempty as f64 } else { 0.0 },
                    }),
                    work,
                    samples,
                    None,
                    Vec::new(),
                )?;
            }
        }
    }

    run.finish();
    Ok(())
}

/// A `FrozenFragment` for the given terms, via the cache API (the only constructor).
///
/// The cache directory is a throwaway keyed on the term set, so this is setup, never measurement:
/// the authorise arm times `build_fragment` directly for exactly the reason this function would
/// otherwise obscure — `get_or_build` memoises, and a shared cache would turn later cells into
/// cache hits without saying so.
fn frozen_fragment(
    postings: &PostingsReader,
    terms: &[tessera_types::TermId],
) -> Result<std::sync::Arc<tessera_authz::FrozenFragment>> {
    let dir = std::env::temp_dir().join(format!("tessera-bench-frag-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cache = tessera_authz::FragmentCache::new(&dir, [0u8; 32], [1u8; 32]);
    Ok(cache.get_or_build(
        terms,
        [2u8; 32],
        terms.len() as u32,
        postings,
        &[],
        u64::MAX,
    )?)
}
