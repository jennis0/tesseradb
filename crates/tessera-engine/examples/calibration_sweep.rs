//! Two-axis sweep for the calibration task: at what per-request work size does the tile-loop
//! fan-out (`pool.install` + `with_min_len`) start winning over a serial fold, as a function of
//! the predictors available BEFORE the fan-out — `tiles_resolved` (tile count) and
//! `rows_in_ranges` (Σ`range.len()`, the total rows spanned pre-mask). Both are already
//! materialised by `Engine::viewport`'s pre-fan-out `tile_ranges_all` sweep (Task 6's zip) and
//! both are already surfaced on [`tessera_engine::StageTimings`], so this sweep reads them
//! post-hoc from real responses rather than reimplementing the resolution logic.
//!
//! **Why `compute_threads = 1` stands in for "serial".** The actual serial fallback this task
//! adds bypasses `pool.install` entirely; `compute_threads = 1` still calls `pool.install` (on a
//! single-worker pool) and so still pays SOME install/scheduling overhead the true fallback will
//! not. Using it as the "serial" arm of this sweep is therefore a conservative (pessimistic)
//! stand-in: if `compute_threads = 1` already beats `compute_threads = default` at some work
//! size, the true zero-overhead serial fallback beats it by at least as much. The crossover this
//! sweep finds is, if anything, biased toward UNDER-selecting the serial range, never over.
//!
//! Run: `cargo run --release --example calibration_sweep -p tessera-engine --features bench-timing`

use std::path::{Path, PathBuf};

use rand::{Rng, SeedableRng};
use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::Extent;
use tessera_types::IdentityKey;

const ITEM_LIMIT: u64 = 2_422_486;
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const REPS: usize = 40;

fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn ensure_bundle() -> PathBuf {
    let bundle_root = PathBuf::from("/tmp/tessera-2m4");
    if !bundle_root.join("CURRENT").exists() {
        let root = workspace_root();
        let args = BuildArgs {
            points: root.join("data/scaled/geometry.parquet"),
            pairs: root.join("data/scaled/pairs/categories-subclass.pairs.parquet"),
            out: bundle_root.clone(),
            extent: extent(),
            slice_id: "s0".to_string(),
            limit: Some(ITEM_LIMIT),
            identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
            identity_key_hex: TEST_KEY_HEX.to_string(),
            identity_epoch: 1,
            shard_id: 0,
        };
        build(&args).expect("2.4M fixture build should succeed");
    }
    bundle_root
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

/// Fix round 1: the sweep's ORIGINAL grant. Deterministic even-spacing across the dictionary, NOT
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

    // Fix round 1: `--dense` selects the bench's own `GrantShape::Random` grant construction
    // (matching the real validation workload's mask density) instead of this tool's original
    // deterministic even-spacing, which review found produced an unrepresentatively sparse mask.
    // Both are kept — see `spread_descriptors`/`random_grant`'s docs.
    let dense = std::env::args().any(|a| a == "--dense");
    let bundle_root = ensure_bundle();
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
        let mut serial_ns = Vec::with_capacity(REPS);
        let mut par_ns = Vec::with_capacity(REPS);
        let mut tiles_resolved = 0u64;
        let mut rows_in_ranges = 0u64;

        for _ in 0..REPS {
            let out = engine_serial
                .viewport(
                    &session1,
                    ViewportRequest::new("s0", shape.zoom, shape.bbox, 30),
                )
                .expect("viewport (serial)");
            serial_ns.push(out.timings.total_ns);
            tiles_resolved = out.timings.tiles_resolved;
            rows_in_ranges = out.timings.rows_in_ranges;
        }
        for _ in 0..REPS {
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
