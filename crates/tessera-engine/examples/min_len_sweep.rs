//! `with_min_len` grain sweep for the calibration task — run once per candidate value of
//! `TILE_PAR_MIN_LEN` (hand-edited in `src/viewport.rs`, rebuilt, rerun; there is no runtime knob
//! for this by design — see the calibration report for why one was not added).
//!
//! Restricted to shapes established (by `calibration_sweep`, run first) as reliably on the
//! PARALLEL side of the crossover, since a chunk-size grain only matters once the fan-out
//! actually runs. §14 found this is the `full-extent` family (few, very large tiles spanning
//! most/all of the corpus) consistently, at every scale and both grant densities tried — the
//! `natural` family's parallel-favouring shapes turned out to be scale- and grant-sensitive (see
//! the calibration report), so this tool no longer assumes a fixed bbox is "clearly parallel"
//! without checking; it prints `rows_in_rng` for exactly that reason.
//!
//! §14: parameterised over `--bundle <path>` (durable fixtures under
//! `data/bench-fixtures/{2m4,1e8,1e9}/`, no on-the-fly building — same reasoning as
//! `calibration_sweep`'s module doc) and `--dense` (same grant-construction choice). Also uses
//! the TRUE, mask-independent row-range predictor rather than `StageTimings.rows_in_ranges` — see
//! `calibration_sweep`'s module doc for the bug this works around.
//!
//! Run: `cargo run --release --example min_len_sweep -p tessera-engine --features bench-timing --
//! --bundle <path> [--dense] [--reps N]`

use std::path::PathBuf;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::{tiles_for_bbox, Bounds};
use tessera_store::{open_bundle, tile_ranges_all, Bundle};

const REPS: usize = 60;

fn bundle_root_from_args(args: &[String]) -> PathBuf {
    let idx = args
        .iter()
        .position(|a| a == "--bundle")
        .unwrap_or_else(|| panic!("usage: min_len_sweep --bundle <path> [--dense] [--reps N]"));
    PathBuf::from(args.get(idx + 1).expect("--bundle needs a path argument"))
}

fn all_descriptors(bundle_root: &std::path::Path) -> Vec<String> {
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

fn spread_descriptors(all: &[String], w: usize) -> Vec<String> {
    let step = (all.len() / w).max(1);
    (0..w)
        .map(|i| all[(i * step) % all.len()].clone())
        .collect()
}

/// See `calibration_sweep::random_grant`'s doc — identical duplication of
/// `tessera_bench::corpus::build_grant`'s `GrantShape::Random` arm.
fn random_grant(all: &[String], w: usize, seed: u64) -> Vec<String> {
    use rand::seq::SliceRandom;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.shuffle(&mut rng);
    idx.truncate(w.min(all.len()));
    idx.into_iter().map(|i| all[i].clone()).collect()
}

/// See `calibration_sweep::true_rows_in_ranges`'s doc — same fix, same reasoning.
fn true_rows_in_ranges(bundle: &Bundle, slice: &str, zoom: u8, bbox: [f64; 4]) -> (u64, u64) {
    let q = bundle.manifest.quantisation;
    let extent = Bounds {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dense = args.iter().any(|a| a == "--dense");
    let reps: usize = args
        .iter()
        .position(|a| a == "--reps")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(REPS);
    let bundle_root = bundle_root_from_args(&args);
    println!(
        "bundle: {}  grant: {}  reps: {reps}",
        bundle_root.display(),
        if dense {
            "dense (random w=10)"
        } else {
            "sparse (spread w=10)"
        }
    );

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

    let bundle = open_bundle(&bundle_root).expect("bundle should open for row-range resolution");

    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 1000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
            flush_max_age_secs: 90,
            max_merged_segment_bytes: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
        },
    )
    .expect("engine should open");
    let session = engine.authorise(auth.as_bytes()).expect("authorise");

    // The `full-extent` family — the one shape family §14 found reliably parallel-favouring at
    // every scale and grant density tried (see the calibration report). `natural/z4` is kept too:
    // at 2.42M/1e8 (and the sparse grant at 1e9) it is also reliably parallel-favouring, so a
    // reader can see whether the chosen `with_min_len` value holds up on a second shape family,
    // not just the one that is always safe.
    let ext = 65536.0;
    let shapes: Vec<(&str, u8, [f64; 4])> = vec![
        ("natural/z4", 4, [10000.0, 10000.0, 42768.0, 42768.0]),
        ("full-extent/z1", 1, [0.0, 0.0, ext, ext]),
        ("full-extent/z2", 2, [0.0, 0.0, ext, ext]),
        ("full-extent/z3", 3, [0.0, 0.0, ext, ext]),
        ("full-extent/z4", 4, [0.0, 0.0, ext, ext]),
        ("full-extent/z5", 5, [0.0, 0.0, ext, ext]),
    ];

    let _ = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 8, [0.0, 0.0, 4096.0, 4096.0], 30),
        )
        .expect("warm-up");

    println!(
        "{:>16} {:>6} {:>10} {:>12} {:>12} {:>12}",
        "shape", "zoom", "tiles", "rows_in_rng", "p50_ns", "p99_ns"
    );
    for (label, zoom, bbox) in shapes {
        let (tiles, rows) = true_rows_in_ranges(&bundle, "s0", zoom, bbox);
        let mut ns: Vec<u64> = Vec::with_capacity(reps);
        for _ in 0..reps {
            let out = engine
                .viewport(&session, ViewportRequest::new("s0", zoom, bbox, 30))
                .expect("viewport");
            ns.push(out.timings.total_ns);
        }
        ns.sort_unstable();
        let p50 = ns[ns.len() / 2];
        let p99 = ns[(ns.len() * 99) / 100];
        println!("{label:>16} {zoom:>6} {tiles:>10} {rows:>12} {p50:>12} {p99:>12}");
    }
}
