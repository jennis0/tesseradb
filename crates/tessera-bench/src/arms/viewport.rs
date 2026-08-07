//! **Ask 3: viewport generation as a function of point count and k** — single viewport, panning,
//! and zooming — plus the C4 coverage sweep.
//!
//! # Why there is a battery mode rather than only a random sweep
//!
//! `docs/evidence/memos/2026-07-30-tail-attribution.md` showed that a uniform-random p99 mostly
//! measures the *input distribution*: Sigma-visible spans 1 to 1.4x10^8 across random bboxes, and
//! repeating one fixed viewport collapsed p99-p50 from 47.4 ms to 5.1 ms. Its recommendation was
//! a fixed representative-viewport battery chosen across the corpus's actual density spectrum,
//! each gated on its own p99-p50, plus normalised-cost ceilings — and that nobody should keep
//! treating an undifferentiated p99 as an exit criterion.
//!
//! `Battery` implements that: viewports are drawn, ranked by measured Sigma-visible, and the
//! deciles are kept, so each cell has a known and reportable density rather than an accidental
//! one. `Random` is retained for comparability with the existing Python harnesses, but its
//! records carry the Sigma-visible distribution so a reader can bucket rather than average.
//!
//! # The C4 discharge
//!
//! `CoverageSweep` emits, per cell, latency *and* `rows_in_ranges - sigma_visible` — rows scanned
//! that this principal cannot see. Appendix C leaves C4 open with "quantify before treating as
//! acceptable", and SA §9 asks for it measured from day one split by mask sparsity. That is the
//! same sweep as the performance measurement, so it is built once and read twice.

use tessera_authz::PostingsReader;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;

use crate::arms::{Context, Result};
use crate::corpus::{build_grant_to_coverage, gen_viewports, Dictionary, GrantShape, TermStats};
use crate::report::{Stages, Work};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Deciles of the corpus's own density spectrum, each repeated — the workload-variance-free
    /// stall detector.
    Battery,
    /// A fixed viewport translated across the extent at constant zoom: what panning costs.
    Pan,
    /// Nested viewports at increasing depth about a fixed centre: what zooming costs.
    Zoom,
    /// Uniform-random bboxes at mixed zooms — the existing harnesses' shape, kept for
    /// comparability and reported bucketed, never as a bare p99.
    Random,
    /// Latency against principal coverage, holding geometry fixed. Doubles as the C4 artifact.
    CoverageSweep,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "battery" => Some(Mode::Battery),
            "pan" => Some(Mode::Pan),
            "zoom" => Some(Mode::Zoom),
            "random" => Some(Mode::Random),
            "coverage-sweep" => Some(Mode::CoverageSweep),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Battery => "battery",
            Mode::Pan => "pan",
            Mode::Zoom => "zoom",
            Mode::Random => "random",
            Mode::CoverageSweep => "coverage-sweep",
        }
    }
}

/// The server's own defaults. A bench cell that does not sweep these must measure them, or it
/// measures a deployment nobody runs. Keep in step with `tessera-server`'s `DEFAULT_*` constants.
pub const DEFAULT_K_MAX_MARKS: usize = 500;
pub const DEFAULT_THETA_TARGET: u64 = 16;

/// Design §7.2's clause parameters to sweep. Grouped rather than passed loose because they are
/// resolved together, default together, and are the two knobs whose effect on selection cost is
/// independent of `k`.
pub struct SelectionSweep<'a> {
    /// The cap clause. One `Engine::open` per value.
    pub k_max_marks: &'a [usize],
    /// The θ anchor target. One `Engine::open` per value.
    pub theta_targets: &'a [u64],
}

pub fn run(
    ctx: &Context,
    modes: &[String],
    ks: &[usize],
    coverages: &[f64],
    zoom: u8,
    seed: u64,
    selection: &SelectionSweep<'_>,
) -> Result<()> {
    let mut run = ctx.open("viewport")?;

    let modes: Vec<Mode> = modes
        .iter()
        .map(|m| Mode::parse(m).ok_or_else(|| format!("unknown mode {m:?}")))
        .collect::<std::result::Result<_, _>>()?;

    if !cfg!(feature = "bench-timing") {
        eprintln!(
            "WARNING: built without `bench-timing` — stage breakdowns will be absent, and the \
             whole point of this arm is stage attribution."
        );
    }

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let bundle = open_bundle(&fixture.root)?;
        let q = bundle.manifest.quantisation;
        let extent_span = q.x_max - q.x_min;
        let slice_id = bundle
            .partitions
            .values()
            .next()
            .and_then(|p| p.slices.keys().next().cloned())
            .unwrap_or_else(|| "s0".to_string());
        drop(bundle);

        // §7.2's clause parameters are resolved from `EngineConfig` at open, not per request, so
        // sweeping them costs one `Engine::open` per combination — and `open` digest-verifies every
        // byte of the bundle. Both lists therefore default to a single value (the server's own), so
        // an ordinary run pays nothing; pass more than one only when the sweep is the point.
        let selection_grid: Vec<(usize, u64)> = selection
            .k_max_marks
            .iter()
            .copied()
            .flat_map(|cap| {
                selection
                    .theta_targets
                    .iter()
                    .copied()
                    .map(move |th| (cap, th))
            })
            .collect();

        for &(cap_marks, theta_target) in &selection_grid {
            for &coverage_target in coverages {
                let (grant, coverage) = build_grant_to_coverage(
                    &stats,
                    &postings,
                    GrantShape::Random,
                    coverage_target,
                    fixture.scale,
                    seed,
                )?;
                if grant.terms.is_empty() {
                    continue;
                }

                // One engine per (fixture, coverage). `Engine::open` digest-verifies every byte, so
                // this is the expensive setup — never inside a timed loop.
                let tmp = std::env::temp_dir().join(format!(
                    "tessera-bench-engine-{}-{}",
                    std::process::id(),
                    coverage_target
                ));
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp)?;
                let engine = Engine::open(
                    &fixture.root,
                    &tmp.join("cache"),
                    &tmp.join("wal.log"),
                    Passthrough::new(),
                    EngineConfig {
                        token_max_lifetime_secs: 3600,
                        max_k: *ks.iter().max().unwrap_or(&200).max(&cap_marks),
                        k_min: 2,
                        k_max_marks: cap_marks,
                        theta_target_marks: theta_target,
                        max_underlay_offset: 4,
                        max_underlay_cells: 8192,
                        max_tiles_per_request: 262_144,
                        compute_threads: tessera_engine::default_compute_threads(),
                        flush_max_age_secs: 90,
                        max_merged_segment_bytes: None,
                        // Compaction §9's trigger is off unless a deployment configures one.
                        compaction: tessera_engine::CompactionSchedule::off(),
                    },
                )?;
                let session = engine.authorise(grant.auth_json(&dictionary).as_bytes())?;

                for &mode in &modes {
                    let plan = if matches!(mode, Mode::Battery | Mode::CoverageSweep) {
                        density_deciles(&engine, &session, &slice_id, extent_span, zoom, seed)?
                    } else {
                        build_plan(mode, extent_span, zoom, seed)
                    };
                    if plan.is_empty() {
                        eprintln!(
                        "viewport: no non-empty viewport found at zoom {zoom} for {}/{} cov{:.4} \
                         — skipping",
                        fixture.scale, fixture.label_set, coverage_target
                    );
                        continue;
                    }

                    for &k in ks {
                        // **The row-projection cache fill must not land in a sample.** The first
                        // viewport of a session crosses entity space into row space over the whole
                        // fragment; it was measured at 8.8 s for a 69M-item mask, and the 10^9 k-sweep
                        // recorded a 9.46 s warm-up. Every existing harness excludes it and reports
                        // it separately; so does this one.
                        let warm = engine.viewport(
                            &session,
                            ViewportRequest::new(&slice_id, plan[0].0, plan[0].1, k),
                        )?;
                        let warmup_ns = warm.timings.total_ns;
                        let built = warm.timings.row_projection_built;

                        for (index, (vz, bbox)) in plan.iter().enumerate() {
                            let cell_id = format!(
                                "viewport/{}/{}/{}/cov{:.4}/k{}/z{}/v{}",
                                fixture.scale,
                                fixture.label_set,
                                mode.name(),
                                coverage_target,
                                k,
                                vz,
                                index
                            );
                            if run.ledger.is_done(&cell_id) {
                                run.skipped += 1;
                                continue;
                            }

                            let mut last = None;
                            let samples = crate::metrics::repeat(ctx.repeat, || {
                                let out = engine
                                    .viewport(
                                        &session,
                                        ViewportRequest::new(&slice_id, *vz, *bbox, k),
                                    )
                                    .expect("viewport");
                                let t = out.timings;
                                last = Some(t);
                                out
                            });
                            let Some(t) = last else { continue };

                            let work = Work {
                                containers: 0, // filled below from the engine's own counters
                                mask_cardinality: 0,
                                coverage,
                                run_ratio: 0.0,
                                tiles_resolved: t.tiles_resolved,
                                tiles_nonempty: t.tiles_nonempty,
                                sigma_visible: t.sigma_visible,
                                rows_in_ranges: t.rows_in_ranges,
                                rows_materialised: t.select_rows_visited,
                                underlay_cells_evaluated: t.underlay_cells_evaluated,
                                points_gathered: t.points_gathered,
                                pages_touched: 0,
                                bytes_touched: 0,
                                degenerate: coverage >= 0.999,
                            };

                            let mut flags = Vec::new();
                            if built {
                                flags.push("row_projection_built_in_warmup".to_string());
                            }
                            // F1, reported per cell rather than only in the canary test: when
                            // selection materialises far more rows than it returns, the selection
                            // path is O(Sigma-visible) and design §10.4's prescription is unapplied.
                            if t.select_rows_visited > t.points_gathered.saturating_mul(4)
                                && t.points_gathered > 0
                            {
                                flags.push(format!(
                                    "selection_overdraw={}x",
                                    t.select_rows_visited / t.points_gathered.max(1)
                                ));
                            }

                            run.emit(
                                cell_id,
                                fixture,
                                serde_json::json!({
                                    "mode": mode.name(),
                                    "k": k,
                                    "zoom": vz,
                                    "bbox": bbox,
                                    "viewport_index": index,
                                    "coverage_target": coverage_target,
                                    "coverage_actual": coverage,
                                    // §7.2's clause parameters. Selection cost depends on these and not
                                    // only on `k`: `k_max_marks` bounds the heap and the gather, while
                                    // `theta_target_marks` decides how many tiles take the serve-all
                                    // branch rather than counting and selecting.
                                    "k_max_marks": cap_marks,
                                    "theta_target_marks": theta_target,
                                    "w": grant.terms.len(),
                                    "warmup_ns": warmup_ns,
                                    // C4's numerator: rows scanned this principal cannot see.
                                    "unauthorised_rows_scanned":
                                        t.rows_in_ranges.saturating_sub(t.sigma_visible),
                                    "seed": seed,
                                }),
                                work,
                                samples,
                                Some(Stages::from_engine(&t, run.clock_lap_ns)),
                                flags,
                            )?;
                        }
                    }
                }
                let _ = std::fs::remove_dir_all(&tmp);
            }
        }
    }

    run.finish();
    Ok(())
}

/// The viewports a mode visits.
fn build_plan(mode: Mode, extent: f64, zoom: u8, seed: u64) -> Vec<(u8, [f64; 4])> {
    match mode {
        // A pan is the same window translated at constant zoom — geometry varies, area does not,
        // so a latency change is a density change and nothing else.
        Mode::Pan => {
            let span = extent / 2f64.powi(zoom.saturating_sub(4).max(1) as i32);
            (0..12)
                .map(|i| {
                    let x = (extent - span) * (i as f64 / 11.0);
                    let y = (extent - span) * 0.5;
                    (zoom, [x, y, x + span, y + span])
                })
                .collect()
        }
        // Nested windows about one centre: area shrinks 4x per level while the centre's content
        // stays the same, which is the zoom-in gesture a client actually makes.
        Mode::Zoom => (0..=12u8)
            .step_by(2)
            .map(|z| {
                let span = extent / 2f64.powi(z.max(1) as i32);
                let lo = (extent - span) / 2.0;
                (z, [lo, lo, lo + span, lo + span])
            })
            .collect(),
        // Handled by `density_deciles`, which needs a live engine to probe with.
        Mode::Battery | Mode::CoverageSweep => gen_viewports(10, extent, seed, &[zoom]),
        Mode::Random => gen_viewports(50, extent, seed, &[4, 5, 6, 7, 8, 9, 10, 11, 12]),
    }
}

/// Ten viewports spanning the corpus's *actual* density spectrum at one zoom.
///
/// **Why this needs a probing pass rather than a random draw.** The corpus geometry is UMAP
/// output quantised onto a 2^16 x 2^16 grid, so points are concentrated and most of the extent is
/// empty: a uniform-random window at zoom 8 usually contains nothing at all, and a "battery" of
/// such windows measures the cost of finding nothing ten times. The tail-attribution memo's
/// recommendation was explicitly deciles *of the density distribution*, which means the
/// distribution has to be measured first.
///
/// The probe runs each candidate once at `k=0` — counting only, no gather — then ranks by the
/// engine's own `sigma_visible` and keeps evenly-spaced non-empty deciles. Cost is one cheap pass
/// over a candidate pool, paid once per (fixture, coverage, mode).
fn density_deciles(
    engine: &Engine,
    session: &tessera_engine::Session,
    slice: &str,
    extent: f64,
    zoom: u8,
    seed: u64,
) -> Result<Vec<(u8, [f64; 4])>> {
    const POOL: usize = 400;
    let candidates = gen_viewports(POOL, extent, seed, &[zoom]);

    let mut measured: Vec<(u64, (u8, [f64; 4]))> = Vec::new();
    for (z, bbox) in candidates {
        let out = engine.viewport(session, ViewportRequest::new(slice, z, bbox, 0))?;
        let sigma = out.timings.sigma_visible;
        if sigma > 0 {
            measured.push((sigma, (z, bbox)));
        }
    }
    if measured.is_empty() {
        return Ok(Vec::new());
    }
    measured.sort_unstable_by_key(|(sigma, _)| *sigma);

    // Evenly-spaced deciles of the non-empty population, always including the extremes: the
    // sparsest viewport and the densest are the two the gate most needs to distinguish.
    let n = measured.len();
    let mut picked = Vec::new();
    for i in 0..10 {
        let idx = (i * (n - 1)) / 9;
        let entry = measured[idx].1;
        if !picked.contains(&entry) {
            picked.push(entry);
        }
    }
    Ok(picked)
}
