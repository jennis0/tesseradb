//! Two-axis sweep for the calibration task: at what per-request work size does the tile-loop
//! fan-out (`pool.install` + `with_min_len`) start winning over a serial fold, as a function of
//! the predictor available BEFORE the fan-out — `rows_in_ranges` (Σ`range.len()`, the total rows
//! spanned pre-mask, over EVERY resolved tile).
//!
//! **§14 bug found and fixed: `StageTimings.rows_in_ranges` is NOT that predictor and must not be
//! read as a stand-in for it.** `tile_result` counts a tile's `range.len()` into its local
//! `TileStats` before checking `visible == 0`, but on that empty-tile branch it returns `Ok(None)`
//! and `Engine::viewport`'s fold loop discards the whole `TileStats` for a `None` result — so
//! `StageTimings.rows_in_ranges` (what this sweep used to print and correlate against) silently
//! excludes every tile the session's OWN MASK made empty. It is therefore mask-dependent, while
//! the actual predictor the engine's serial/parallel branch reads (`total_rows_in_ranges` in
//! `viewport.rs`) is computed directly from `ranges` BEFORE any masking and includes every
//! resolved tile regardless of visibility. The gap between the two is small when few tiles are
//! empty (most of §2's original 2.42M runs) and can be enormous when many are (§14's 1e9 runs:
//! one grant's `StageTimings` figure came in 26x smaller than the other's for the geometrically
//! IDENTICAL shape, which is the tell — a mask-independent quantity cannot legitimately move
//! between two sessions over the same tiles). Fixed by computing the true, mask-independent
//! `rows_in_ranges` directly here (`true_rows_in_ranges`, via `tessera_store::tile_ranges_all` on
//! the bundle opened outright, no session involved) rather than trusting the engine's own
//! post-request telemetry for a pre-request decision.
//!
//! **Why `compute_threads = 1` stands in for "serial".** The actual serial fallback this task
//! adds bypasses `pool.install` entirely; `compute_threads = 1` still calls `pool.install` (on a
//! single-worker pool) and so still pays SOME install/scheduling overhead the true fallback will
//! not. Using it as the "serial" arm of this sweep is therefore a conservative (pessimistic)
//! stand-in: if `compute_threads = 1` already beats `compute_threads = default` at some work
//! size, the true zero-overhead serial fallback beats it by at least as much. The crossover this
//! sweep finds is, if anything, biased toward UNDER-selecting the serial range, never over.
//!
//! Run: `cargo run --release --example calibration_sweep -p tessera-engine --features
//! bench-timing -- --bundle <path> [--dense]`
//!
//! **§14 re-calibration (post-B9, three scales).** Originally hard-coded to a single
//! `ensure_bundle()`-built 2.4M fixture at a fixed `/tmp` path. B9's three-tier adaptive
//! selection decode (`perf(engine): gate the run decode behind a measured three-tier adaptive
//! choice`) changed the per-row cost this whole calibration was fitted against, and the
//! bundle-build rewrite (`8c671f1`) made 1e8/1e9-scale fixtures cheap enough to build routinely
//! — so a fixed single-scale bundle stopped being the right shape for this tool. `--bundle` now
//! takes any already-built bundle root directly (no building here — durable fixtures live under
//! `data/bench-fixtures/{2m4,1e8,1e9}/`, provisioned once outside this tool, matching how the
//! 1e9 bench report's own bundle was produced) so the SAME sweep logic runs unchanged at every
//! scale.

use std::path::{Path, PathBuf};

use rand::{Rng, SeedableRng};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::{tiles_for_bbox, Bounds};
use tessera_store::{open_bundle, tile_ranges_all, Bundle};

const REPS: usize = 40;

/// The TRUE, mask-independent predictor value — see the module doc's §14 note for why
/// `StageTimings.rows_in_ranges` cannot be used for this. Resolves tiles and their row ranges
/// exactly as `Engine::viewport` does (`tiles_for_bbox` then `tile_ranges_all`), against the
/// bundle opened directly, no session or mask involved at any point.
fn true_rows_in_ranges(bundle: &Bundle, view: &str, zoom: u8, bbox: [f64; 4]) -> (u64, u64) {
    let q = bundle.manifest.quantisation;
    let extent = Bounds {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let tiles = tiles_for_bbox(bbox, zoom, &extent);
    let view_data = bundle
        .partitions
        .values()
        .find_map(|p| p.views.get(view))
        .expect("view should exist");
    let segment = view_data.segments.first().expect("one segment (R4)");
    let ranges = tile_ranges_all(segment, &tiles);
    let rows: u64 = ranges.iter().map(|r| r.len() as u64).sum();
    (tiles.len() as u64, rows)
}

/// `--bundle <path>` is required — this tool no longer builds a fixture itself (see the module
/// doc). Panics with a usage message rather than silently falling back to a stale default, since
/// a silent fallback is exactly how §2's original sweep ended up calibrated against the wrong
/// scale's cost model for as long as it did.
fn bundle_root_from_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let idx = args
        .iter()
        .position(|a| a == "--bundle")
        .unwrap_or_else(|| {
            panic!(
                "usage: calibration_sweep --bundle <path> [--dense]\n  \
                 (durable fixtures: data/bench-fixtures/{{2m4,1e8,1e9}}/)"
            )
        });
    PathBuf::from(args.get(idx + 1).expect("--bundle needs a path argument"))
}

fn all_descriptors(bundle_root: &Path) -> Vec<String> {
    let dict_path = bundle_root.join("v00000/dictionary/terms-0.dict");
    let data = std::fs::read(dict_path).unwrap();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        out.push(String::from_utf8(data[pos..pos + len].to_vec()).unwrap());
        pos += len;
    }
    out
}

/// The sweep's sparse grant. Deterministic even-spacing across the dictionary, NOT
/// representative of the validation workload's actual grant construction — review caught that
/// this produced a mask ~20x sparser than `bench_concurrency.py`'s own w=10 random-term grant
/// (`sigma_visible=21` at the exact validation bbox vs the real run's ~433 points/request on the
/// same shape), and that mask density is a cost driver the predictor (deliberately pre-mask) does
/// not model. Kept for comparison against [`random_grant`]'s dense-mask re-run — see the module
/// doc and the calibration report's fix-round-1 section for both tables side by side.
fn spread_descriptors(all: &[String], w: usize) -> Vec<String> {
    let step = (all.len() / w).max(1);
    (0..w)
        .map(|i| all[(i * step) % all.len()].clone())
        .collect()
}

/// The bench's OWN grant construction (`tessera_bench::corpus::build_grant`'s
/// `GrantShape::Random` arm, duplicated rather than imported — `tessera-engine` cannot depend on
/// `tessera-bench`, same layering reason [`viewports`] duplicates `gen_viewports`): shuffle every
/// term with `StdRng::seed_from_u64(seed)`, truncate to `w`. This is the dense-mask grant fix
/// round 1 asked for — a uniform random sample of the vocabulary rather than deterministic even
/// spacing, which is what actually produces a mask density comparable to the real validation runs.
fn random_grant(all: &[String], w: usize, seed: u64) -> Vec<String> {
    use rand::seq::SliceRandom;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.shuffle(&mut rng);
    idx.truncate(w.min(all.len()));
    idx.into_iter().map(|i| all[i].clone()).collect()
}

/// One sweep shape: a (zoom, bbox) pair with a label describing how it was constructed.
struct Shape {
    label: String,
    zoom: u8,
    bbox: [f64; 4],
}

fn shapes() -> Vec<Shape> {
    let mut out = Vec::new();
    let ext = 65536.0;
    // Natural viewport shapes at increasing zoom (`gen_viewports`' own span formula: extent /
    // 2^(zoom-4)), several random draws per zoom for density variety — this is the regime a real
    // client produces (fixed-size window, varying position).
    for zoom in [4u8, 5, 6, 7, 8, 10, 12, 14] {
        let span = ext / 2f64.powi(zoom.saturating_sub(4).max(1) as i32);
        for seed in 0u64..8 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed * 97 + zoom as u64);
            let x0 = rng.gen_range(0.0..(ext - span).max(f64::MIN_POSITIVE));
            let y0 = rng.gen_range(0.0..(ext - span).max(f64::MIN_POSITIVE));
            out.push(Shape {
                label: format!("natural/z{zoom}/s{seed}"),
                zoom,
                bbox: [x0, y0, x0 + span, y0 + span],
            });
        }
    }
    // Few-tiles-huge-work edge case: the WHOLE extent at low zoom. Tile count is small
    // (4^zoom) but every tile spans a huge row range (the whole corpus divided among very few
    // tiles) -- the case a tile-count-only predictor would misjudge.
    for zoom in [1u8, 2, 3, 4, 5] {
        out.push(Shape {
            label: format!("full-extent/z{zoom}"),
            zoom,
            bbox: [0.0, 0.0, ext, ext],
        });
    }
    out
}

fn main() {
    if !cfg!(feature = "bench-timing") {
        println!("built without `bench-timing` — StageTimings fields would be zero; re-run with --features bench-timing.");
        return;
    }

    // `--dense` selects the bench's own `GrantShape::Random` grant construction
    // (matching the real validation workload's mask density) instead of this tool's original
    // deterministic even-spacing, which review found produced an unrepresentatively sparse mask.
    // Both are kept — see `spread_descriptors`/`random_grant`'s docs.
    let args: Vec<String> = std::env::args().collect();
    let dense = args.iter().any(|a| a == "--dense");
    // §14: optional rep-count override — at 1e8/1e9 scale a per-request time large enough to make
    // the default 40 reps slow is plausible and was not measured in advance, so this is here to
    // adjust without a rebuild rather than assumed unnecessary.
    let reps: usize = args
        .iter()
        .position(|a| a == "--reps")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(REPS);
    let bundle_root = bundle_root_from_args();
    println!("bundle: {}", bundle_root.display());
    let all = all_descriptors(&bundle_root);
    let terms = if dense {
        random_grant(&all, 10, 0)
    } else {
        spread_descriptors(&all, 10)
    };
    let terms_json = terms
        .iter()
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let auth = format!(r#"{{"terms": [{terms_json}]}}"#);
    println!(
        "grant mode: {} (w={})\n",
        if dense {
            "random (--dense, bench GrantShape::Random)"
        } else {
            "spread (deterministic)"
        },
        terms.len()
    );

    // §14: opened directly (no session) purely to compute the true, mask-independent predictor
    // value per shape — see `true_rows_in_ranges`'s doc.
    let bundle = open_bundle(&bundle_root).expect("bundle should open for row-range resolution");

    let cfg = |compute_threads: usize| EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 1000,
        k_min: 2,
        k_max_marks: 500,
        theta_target_marks: 16,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads,
        flush_max_age_secs: 90,
        max_merged_segment_bytes: None,
        tier_width: None,
        segment_floor_bytes: None,
        coalesce_width: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
    };

    let tmp1 = tempfile::tempdir().unwrap();
    let engine_serial = Engine::open(
        &bundle_root,
        &tmp1.path().join("cache"),
        &tmp1.path().join("wal.log"),
        Passthrough::new(),
        cfg(1),
    )
    .expect("engine (threads=1) should open");
    let session1 = engine_serial.authorise(auth.as_bytes()).expect("authorise");

    let tmp2 = tempfile::tempdir().unwrap();
    let engine_par = Engine::open(
        &bundle_root,
        &tmp2.path().join("cache"),
        &tmp2.path().join("wal.log"),
        Passthrough::new(),
        cfg(tessera_engine::default_compute_threads()),
    )
    .expect("engine (threads=default) should open");
    let session2 = engine_par.authorise(auth.as_bytes()).expect("authorise");

    // Warm-up: pays the row-projection cache fill once per engine, excluded from every figure.
    let _ = engine_serial
        .viewport(
            &session1,
            ViewportRequest::new("s0", 8, [0.0, 0.0, 4096.0, 4096.0], 30),
        )
        .expect("warm-up");
    let _ = engine_par
        .viewport(
            &session2,
            ViewportRequest::new("s0", 8, [0.0, 0.0, 4096.0, 4096.0], 30),
        )
        .expect("warm-up");

    println!(
        "{:>22} {:>6} {:>10} {:>12} {:>12} {:>12} {:>9} {:>7}",
        "shape", "zoom", "tiles", "rows_in_rng", "serial_ns", "par_ns", "ratio", "winner"
    );
    println!("{}", "-".repeat(100));

    for shape in shapes() {
        let mut serial_ns = Vec::with_capacity(reps);
        let mut par_ns = Vec::with_capacity(reps);
        // §14: the TRUE predictor, mask-independent, computed once per shape directly from the
        // bundle — NOT `out.timings.{tiles_resolved,rows_in_ranges}` (see module doc's bug note).
        let (tiles_resolved, rows_in_ranges) =
            true_rows_in_ranges(&bundle, "s0", shape.zoom, shape.bbox);

        for _ in 0..reps {
            let out = engine_serial
                .viewport(
                    &session1,
                    ViewportRequest::new("s0", shape.zoom, shape.bbox, 30),
                )
                .expect("viewport (serial)");
            serial_ns.push(out.timings.total_ns);
        }
        for _ in 0..reps {
            let out = engine_par
                .viewport(
                    &session2,
                    ViewportRequest::new("s0", shape.zoom, shape.bbox, 30),
                )
                .expect("viewport (parallel)");
            par_ns.push(out.timings.total_ns);
        }
        serial_ns.sort_unstable();
        par_ns.sort_unstable();
        let serial_med = serial_ns[serial_ns.len() / 2];
        let par_med = par_ns[par_ns.len() / 2];
        let ratio = par_med as f64 / serial_med.max(1) as f64;
        let winner = if ratio < 1.0 { "PAR" } else { "SERIAL" };

        println!(
            "{:>22} {:>6} {:>10} {:>12} {:>12} {:>12} {:>9.2} {:>7}",
            shape.label,
            shape.zoom,
            tiles_resolved,
            rows_in_ranges,
            serial_med,
            par_med,
            ratio,
            winner
        );
    }
}
