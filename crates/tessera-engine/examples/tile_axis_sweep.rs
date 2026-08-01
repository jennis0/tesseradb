//! **The second axis the calibration campaign never swept: tile count, varied independently of
//! row count.**
//!
//! `examples/calibration_sweep.rs` swept `rows_in_ranges` — the quantity
//! `SERIAL_FALLBACK_MAX_ROWS` is compared against — across two shape families, and the
//! calibration report's §14.4 concluded from that data that no row-count constant can classify
//! serial-vs-parallel correctly. Its closing sentence names "row count, tile count" as the
//! disproved quantities, but **tile count was never a swept variable in that campaign**: the
//! `natural` family resolves a near-constant ~289 tiles at every zoom by construction (span =
//! extent / 2^(zoom−4) is exactly 16 cells wide at every depth → 17² tiles), and `full-extent`
//! tops out at 1,024 (z5). Nothing in the campaign exceeds 1,024 tiles. The one shape above it
//! that exists anywhere — `benches/viewport.rs`'s 8,281-tile / 242,221-row request — is a ~2×
//! *parallel* win at 2.42M, the least parallel-friendly scale
//! (`docs/evidence/memos/2026-07-31-viewport-bench-regression.md`). Within a family where tile count
//! barely varies, tile count cannot discriminate; that is a property of the sampling, not of the
//! predictor.
//!
//! This tool sweeps the missing axis. Shapes are a **crossed grid**: a bbox anchored at the
//! extent's origin with side `f · 65536` for several `f`, each resolved at several depths. Holding
//! `f` fixed and raising the depth multiplies tile count by ~4 while leaving rows-in-range
//! essentially unchanged (the only movement is the shrinking overhang of tiles that straddle the
//! bbox edge — a few percent, reported per row so it can be checked rather than assumed). Holding
//! the depth fixed and varying `f` moves rows by orders of magnitude at a comparable tile count.
//! A two-term predictor `a·tiles + b·rows` is fittable on that grid and is not fittable on the
//! campaign's.
//!
//! **Why the arms differ from `calibration_sweep`'s, and why this is the stronger instrument.**
//! That sweep's "serial" arm is `compute_threads = 1`, which still enters `pool.install` and so
//! still pays scheduling overhead the real serial fold does not — its own module doc says so and
//! calls the resulting crossover conservative. Here both engines are built with the *same*
//! `compute_threads = default` and the *same* pool, and the only difference between them is the
//! per-`Engine` threshold override (`set_serial_fallback_max_rows_for_test`, `bench-timing`-gated,
//! `session.rs`): `u64::MAX` forces the serial fold on every request, `0` forces the
//! `pool.install` fan-out on every request. That makes each row a genuine single-variable A/B of
//! the exact branch in `viewport.rs` the constant selects — the form of evidence the regression
//! memo found decisive — rather than a proxy comparison between two differently-configured
//! engines.
//!
//! Reps are **interleaved** (serial, parallel, serial, parallel, …) rather than run in two blocks,
//! so a drift in machine state during a shape's measurement is charged to both arms equally
//! instead of entirely to whichever arm ran second.
//!
//! Run (always through the bench slot — `scripts/bench-slot.sh`):
//! ```text
//! cargo run --release --example tile_axis_sweep -p tessera-engine --features bench-timing \
//!   -- --bundle data/bench-fixtures/1e8 [--sparse] [--reps N] [--max-tiles N] [--contiguity-only]
//!      [--target-coverage FRACTION]
//! ```
//!
//! Output is an aligned table for reading plus one `CSV,` line per shape for analysis.

use std::path::{Path, PathBuf};

use croaring::Bitmap;
use rand::SeedableRng;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig, RowProjection};
use tessera_plugin::Passthrough;
use tessera_spatial::{tiles_for_bbox, Extent};
use tessera_store::{open_bundle, tile_ranges_all, Bundle};

const REPS: usize = 25;
const WARMUP: usize = 3;

/// The TRUE, mask-independent predictor pair — the same computation `Engine::viewport` performs
/// before it chooses a branch (`tiles_for_bbox` then `tile_ranges_all`, summed pre-mask), against
/// the bundle opened directly with no session involved. See `calibration_sweep.rs`'s module doc
/// for why `StageTimings::rows_in_ranges` must not be used for this.
fn true_predictors(bundle: &Bundle, slice: &str, zoom: u8, bbox: [f64; 4]) -> (u64, u64) {
    let q = bundle.manifest.quantisation;
    let extent = Extent {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let tiles = tiles_for_bbox(bbox, zoom, &extent);
    let slice_data = bundle
        .partitions
        .values()
        .find_map(|p| p.slices.get(slice))
        .expect("slice should exist");
    let segment = slice_data.segments.first().expect("one segment (R4)");
    let ranges = tile_ranges_all(segment, &tiles);
    let rows: u64 = ranges.iter().map(|r| r.len() as u64).sum();
    (tiles.len() as u64, rows)
}

fn arg_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn bundle_root_from_args() -> PathBuf {
    PathBuf::from(arg_value("--bundle").unwrap_or_else(|| {
        panic!(
            "usage: tile_axis_sweep --bundle <path> [--sparse] [--reps N] [--max-tiles N]\n  \
             [--contiguity-only] [--target-coverage FRACTION]\n  (durable fixtures: data/bench-fixtures/{{2m4,1e8,1e9}}/)"
        )
    }))
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

/// `calibration_sweep`'s `--dense` grant, which is the bench's own `GrantShape::Random`
/// construction (`tessera_bench::corpus::build_grant`) — the realistic one.
/// Duplicated rather than imported for the same layering reason `calibration_sweep` duplicates it.
fn random_grant(all: &[String], w: usize, seed: u64) -> Vec<String> {
    use rand::seq::SliceRandom;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.shuffle(&mut rng);
    idx.truncate(w.min(all.len()));
    idx.into_iter().map(|i| all[i].clone()).collect()
}

/// `calibration_sweep`'s original deterministic even-spacing grant, kept as `--sparse` so a run
/// here is comparable with the campaign's sparse column.
fn spread_descriptors(all: &[String], w: usize) -> Vec<String> {
    let step = (all.len() / w).max(1);
    (0..w)
        .map(|i| all[(i * step) % all.len()].clone())
        .collect()
}

/// Roaring containers a bitmap spans — the count of distinct high 16 bits among its values.
///
/// **The cost model's unit** ("O(containers touched), not O(cardinality)"). Transcribed from
/// `tessera_bench::metrics::containers`, which is the canonical implementation and itself mirrors
/// the probes' `containers(bm) = len(unique(asarray(bm) >> 16))`; duplicated here for the
/// same layering reason `random_grant` above is duplicated — `tessera-bench` is a binary crate
/// above this one and nothing may depend on it (`scripts/check-layers.sh`).
fn containers(bitmap: &Bitmap) -> u64 {
    let mut count = 0u64;
    let mut last_high: Option<u32> = None;
    for value in bitmap.iter() {
        let high = value >> 16;
        if last_high != Some(high) {
            count += 1;
            last_high = Some(high);
        }
    }
    count
}

/// Mean run length of set bits over the `1/(1−p)` expectation for a uniformly random set of the
/// same density — `probes/results.md` §5's quantity, so a figure here is directly comparable with
/// that table. 1.00 means no clustering beyond chance. Transcribed from
/// `tessera_bench::metrics::run_ratio` for the same layering reason as `containers` above.
fn run_ratio(bitmap: &Bitmap, universe: u64) -> f64 {
    let cardinality = bitmap.cardinality();
    if cardinality == 0 || universe == 0 {
        return 0.0;
    }
    let mut runs = 0u64;
    let mut prev: Option<u32> = None;
    for value in bitmap.iter() {
        if prev.map(|p| value != p + 1).unwrap_or(true) {
            runs += 1;
        }
        prev = Some(value);
    }
    if runs == 0 {
        return 0.0;
    }
    let mean_run = cardinality as f64 / runs as f64;
    let p = cardinality as f64 / universe as f64;
    if p >= 1.0 {
        return 1.0;
    }
    mean_run / (1.0 / (1.0 - p))
}

/// Smallest `w` whose `random_grant(all, w, seed)` reaches `target` coverage of `universe`, found
/// by binary search over `w` and evaluated by actually authorising the credential.
///
/// **Why this exists (the label-axis check, 2026-08-01).** A fixed `w = 10` does not mean the same
/// thing in two label sets: on `categories-archive`'s ~40-term dictionary it grants ~48% of the
/// corpus, on `surnames`' ~400,000-term dictionary it grants 0.005%. Comparing crossovers across
/// label sets at fixed `w` therefore varies contiguity **and** coverage together, and the two have
/// opposite expected effects on per-tile work. Holding coverage fixed and letting the label set
/// supply the contiguity is the single-variable form of that comparison — the same construction
/// `probes/results.md` §5 used when it reported run ratio at a common "head 25%".
///
/// Coverage is monotone in `w` only in expectation, not exactly (terms overlap), so the search
/// returns the smallest `w` it *found* at or above the target and prints what it achieved rather
/// than asserting it hit it.
fn width_for_coverage(engine: &Engine, all: &[String], target: f64, universe: u64) -> usize {
    let coverage_of = |w: usize| -> f64 {
        let terms = random_grant(all, w, 0);
        let json = terms
            .iter()
            .map(|t| format!("{t:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let auth = format!(r#"{{"terms": [{json}]}}"#);
        let s = engine
            .authorise(auth.as_bytes())
            .expect("authorise (coverage search)");
        let card = s.fragment.view().cardinality();
        card as f64 / universe as f64
    };
    let (mut lo, mut hi) = (1usize, all.len());
    if coverage_of(hi) < target {
        eprintln!(
            "coverage search: the whole dictionary reaches only {:.4} < {target:.4}; using all {hi} terms",
            coverage_of(hi)
        );
        return hi;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if coverage_of(mid) >= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

struct Shape {
    label: String,
    zoom: u8,
    bbox: [f64; 4],
}

/// The crossed grid. `frac` is the bbox side as a fraction of the extent; the depths listed
/// against it are chosen so the resulting tile counts span roughly 16 → 65,536 across the grid,
/// with several (frac, depth) pairs landing at a similar tile count over very different row
/// counts — that overlap is what lets the two terms be separated.
///
/// `frac = 0.353546` reproduces `benches/viewport.rs`'s bbox (23,170 of 65,536 units), the
/// project's only pre-existing high-tile-count probe, so this grid contains that exact shape.
fn shapes() -> Vec<Shape> {
    let ext = 65536.0;
    let grid: &[(f64, &str, &[u8])] = &[
        (1.0, "f100", &[2, 3, 4, 5, 6, 7, 8]),
        (0.353546, "f35", &[4, 5, 6, 7, 8, 9, 10]),
        (0.125, "f12", &[5, 6, 7, 8, 9, 10, 11]),
        (0.044189, "f04", &[7, 8, 9, 10, 11, 12]),
        (0.015625, "f01", &[8, 9, 10, 11, 12, 13]),
    ];
    let mut out = Vec::new();
    for (frac, tag, depths) in grid {
        let side = ext * frac;
        for &zoom in *depths {
            out.push(Shape {
                label: format!("{tag}/z{zoom}"),
                zoom,
                bbox: [0.0, 0.0, side, side],
            });
        }
    }
    // Two campaign shapes carried along unchanged as anchors, so this run can be checked against
    // the campaign's own tables rather than only against itself: a `natural`-family window (the
    // ~289-tile shape `calibration_sweep` swept at every zoom) and `full-extent/z5` (its
    // 1,024-tile ceiling, already present above as `f100/z5` but listed here under the campaign's
    // own label for cross-reference).
    for zoom in [6u8, 8, 10] {
        let span = ext / 2f64.powi(zoom.saturating_sub(4).max(1) as i32);
        out.push(Shape {
            label: format!("natural/z{zoom}"),
            zoom,
            bbox: [0.0, 0.0, span, span],
        });
    }
    out
}

fn main() {
    if !cfg!(feature = "bench-timing") {
        println!("built without `bench-timing` — the threshold override does not exist; re-run with --features bench-timing.");
        return;
    }

    let sparse = std::env::args().any(|a| a == "--sparse");
    let reps: usize = arg_value("--reps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(REPS);
    let max_tiles: u64 = arg_value("--max-tiles")
        .and_then(|s| s.parse().ok())
        .unwrap_or(131_072);
    let bundle_root = bundle_root_from_args();

    let target_coverage: Option<f64> = arg_value("--target-coverage").and_then(|s| s.parse().ok());
    let all = all_descriptors(&bundle_root);
    println!("bundle: {}", bundle_root.display());

    let bundle = open_bundle(&bundle_root).expect("bundle should open for row-range resolution");

    let cfg = EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 1000,
        k_min: 2,
        k_max_marks: 500,
        theta_target_marks: 16,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: tessera_engine::default_compute_threads(),
        pin_ttl_secs: 300,
        pins_per_session_max: 4,
    };

    // Both engines are configured IDENTICALLY, including the thread pool. The single variable
    // between them is the threshold override below.
    let tmp1 = tempfile::tempdir().unwrap();
    let engine_serial = Engine::open(
        &bundle_root,
        &tmp1.path().join("cache"),
        &tmp1.path().join("wal.log"),
        Passthrough::new(),
        cfg,
    )
    .expect("engine (forced serial) should open");
    let tmp2 = tempfile::tempdir().unwrap();
    let engine_par = Engine::open(
        &bundle_root,
        &tmp2.path().join("cache"),
        &tmp2.path().join("wal.log"),
        Passthrough::new(),
        cfg,
    )
    .expect("engine (forced parallel) should open");

    // `total_rows_in_ranges < threshold` selects the serial fold (`viewport.rs`'s
    // `should_fold_serially`), so u64::MAX is "always serial" and 0 is "always parallel".
    engine_serial.set_serial_fallback_max_rows_for_test(u64::MAX);
    engine_par.set_serial_fallback_max_rows_for_test(0);

    // The grant. `--target-coverage` needs an engine to authorise against, so grant selection sits
    // here rather than up with the other argument parsing.
    let universe = {
        let sd = bundle
            .partitions
            .values()
            .find_map(|p| p.slices.get("s0"))
            .expect("slice s0");
        sd.segments.first().expect("one segment (R4)").row_count as u64
    };
    let (terms, mode) = match target_coverage {
        Some(target) => {
            let w = width_for_coverage(&engine_serial, &all, target, universe);
            (
                random_grant(&all, w, 0),
                format!("coverage-matched (random, --target-coverage {target})"),
            )
        }
        None if sparse => (
            spread_descriptors(&all, 10),
            "spread (deterministic, --sparse)".to_string(),
        ),
        None => (
            random_grant(&all, 10, 0),
            "random (bench GrantShape::Random, default)".to_string(),
        ),
    };
    let terms_json = terms
        .iter()
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let auth = format!(r#"{{"terms": [{terms_json}]}}"#);
    println!(
        "grant mode: {} (w={}), dictionary={}, reps={}, max_tiles={}\n",
        mode,
        terms.len(),
        all.len(),
        reps,
        max_tiles
    );

    let session1 = engine_serial.authorise(auth.as_bytes()).expect("authorise");
    let session2 = engine_par.authorise(auth.as_bytes()).expect("authorise");

    // ---------------------------------------------------------------------------------------
    // The premise block: how contiguous is the mask this sweep actually runs against?
    //
    // `probes/results.md` §5 measures run ratio 1.00 → 5.11 across label sets, but for ITS grant
    // scenarios (head-25% etc.), not for the grants swept here (`GrantShape::Random` w=10, or the
    // deterministic even spacing under `--sparse`). Contiguity is containers-touched and
    // containers-touched is the design's cost model, so it feeds per-tile work directly — but a
    // run-ratio figure measured under a different grant does not transfer. This block measures the
    // grant that is about to be swept, so a label-set comparison can be checked rather than
    // inherited. Entity space AND row space, because §5 reports both and claims they agree.
    //
    // Outside every timed region; costs one extra projection.
    {
        let slice_data = bundle
            .partitions
            .values()
            .find_map(|p| p.slices.get("s0"))
            .expect("slice s0");
        let segment = slice_data.segments.first().expect("one segment (R4)");
        let universe = segment.row_count as u64;
        let view = session1.fragment.view();
        let ent: &Bitmap = &view;
        let proj = RowProjection::new(&session1.fragment, &slice_data.permutation);
        let rows = proj.bitmap();
        let ent_card = ent.cardinality();
        let row_card = rows.cardinality();
        println!(
            "grant contiguity (universe {universe} rows): \
             entity-space card={ent_card} cov={:.4}% containers={} run_ratio={:.3} | \
             row-space card={row_card} cov={:.4}% containers={} run_ratio={:.3}",
            100.0 * ent_card as f64 / universe as f64,
            containers(ent),
            run_ratio(ent, universe),
            100.0 * row_card as f64 / universe as f64,
            containers(rows),
            run_ratio(rows, universe),
        );
        println!(
            "GRANT,{},{},{},{:.6},{},{},{:.6},{}",
            universe,
            ent_card,
            containers(ent),
            run_ratio(ent, universe),
            row_card,
            containers(rows),
            run_ratio(rows, universe),
            terms.join("|"),
        );
        println!();
        if std::env::args().any(|a| a == "--contiguity-only") {
            return;
        }
    }

    // Pays the row-projection cache fill once per engine; excluded from every figure.
    for (e, s) in [(&engine_serial, &session1), (&engine_par, &session2)] {
        let _ = e
            .viewport(
                s,
                ViewportRequest::new("s0", 8, [0.0, 0.0, 4096.0, 4096.0], 30),
            )
            .expect("warm-up");
    }

    println!(
        "{:>12} {:>5} {:>9} {:>13} {:>12} {:>12} {:>7} {:>7} {:>8}",
        "shape",
        "zoom",
        "tiles",
        "rows_in_rng",
        "serial_ns",
        "par_ns",
        "ratio",
        "winner",
        "ns/tile"
    );
    println!("{}", "-".repeat(104));
    println!("CSV,shape,zoom,tiles,rows,serial_p50_ns,par_p50_ns,ratio,serial_p10_ns,par_p10_ns");

    for shape in shapes() {
        let (tiles, rows) = true_predictors(&bundle, "s0", shape.zoom, shape.bbox);
        if tiles > max_tiles {
            println!(
                "{:>12} {:>5} {:>9}   (skipped: above --max-tiles)",
                shape.label, shape.zoom, tiles
            );
            continue;
        }

        let req = || ViewportRequest::new("s0", shape.zoom, shape.bbox, 30);
        for _ in 0..WARMUP {
            let _ = engine_serial
                .viewport(&session1, req())
                .expect("warm-up serial");
            let _ = engine_par
                .viewport(&session2, req())
                .expect("warm-up parallel");
        }

        let mut serial_ns = Vec::with_capacity(reps);
        let mut par_ns = Vec::with_capacity(reps);
        // Interleaved, so drift in machine state is charged to both arms equally.
        for _ in 0..reps {
            let a = engine_serial
                .viewport(&session1, req())
                .expect("viewport (serial)");
            serial_ns.push(a.timings.total_ns);
            let b = engine_par
                .viewport(&session2, req())
                .expect("viewport (parallel)");
            par_ns.push(b.timings.total_ns);
        }
        serial_ns.sort_unstable();
        par_ns.sort_unstable();
        let pct = |v: &[u64], p: usize| v[(v.len() * p / 100).min(v.len() - 1)];
        let serial_med = pct(&serial_ns, 50);
        let par_med = pct(&par_ns, 50);
        let ratio = par_med as f64 / serial_med.max(1) as f64;
        let winner = if ratio < 1.0 { "PAR" } else { "SERIAL" };
        let ns_per_tile = serial_med as f64 / tiles.max(1) as f64;

        println!(
            "{:>12} {:>5} {:>9} {:>13} {:>12} {:>12} {:>7.2} {:>7} {:>8.1}",
            shape.label, shape.zoom, tiles, rows, serial_med, par_med, ratio, winner, ns_per_tile
        );
        println!(
            "CSV,{},{},{},{},{},{},{:.4},{},{}",
            shape.label,
            shape.zoom,
            tiles,
            rows,
            serial_med,
            par_med,
            ratio,
            pct(&serial_ns, 10),
            pct(&par_ns, 10)
        );
    }
}
