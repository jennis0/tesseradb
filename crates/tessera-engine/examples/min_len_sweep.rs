//! `with_min_len` grain sweep for the calibration task — run once per candidate value of
//! `TILE_PAR_MIN_LEN` (hand-edited in `src/viewport.rs`, rebuilt, rerun; there is no runtime knob
//! for this by design — see the calibration report for why one was not added).
//!
//! Restricted to shapes the serial-fallback sweep (`calibration_sweep`) already established are
//! clearly on the PARALLEL side of the threshold (`rows_in_ranges` well above 200,000), since a
//! chunk-size grain only matters once the fan-out actually runs — see the calibration report's
//! sweep table for the full serial-vs-parallel picture this narrows from.
//!
//! Run: `cargo run --release --example min_len_sweep -p tessera-engine --features bench-timing`

use std::path::PathBuf;

use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::Extent;
use tessera_types::IdentityKey;

const ITEM_LIMIT: u64 = 2_422_486;
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const REPS: usize = 60;

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
            mint_external_ids: false,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            emit_oracle_pairs: false,
        };
        build(&args).expect("2.4M fixture build should succeed");
    }
    bundle_root
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

fn main() {
    let bundle_root = ensure_bundle();
    let all = all_descriptors(&bundle_root);
    let terms = spread_descriptors(&all, 10);
    let terms_json = terms
        .iter()
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let auth = format!(r#"{{"terms": [{terms_json}]}}"#);

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
        },
    )
    .expect("engine should open");
    let session = engine.authorise(auth.as_bytes()).expect("authorise");

    // Clearly-parallel shapes from `calibration_sweep`'s own findings: natural z4/z5 (big span,
    // dense) and full-extent z2..z5 (huge total rows, varying tile counts from 16 to 1024) — the
    // range this grain constant actually governs.
    let shapes: Vec<(&str, u8, [f64; 4])> = vec![
        ("natural/z4", 4, [10000.0, 10000.0, 42768.0, 42768.0]),
        ("natural/z5", 5, [5000.0, 5000.0, 21384.0, 21384.0]),
        ("full-extent/z2", 2, [0.0, 0.0, 65536.0, 65536.0]),
        ("full-extent/z3", 3, [0.0, 0.0, 65536.0, 65536.0]),
        ("full-extent/z4", 4, [0.0, 0.0, 65536.0, 65536.0]),
        ("full-extent/z5", 5, [0.0, 0.0, 65536.0, 65536.0]),
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
        let mut ns: Vec<u64> = Vec::with_capacity(REPS);
        let mut tiles = 0u64;
        let mut rows = 0u64;
        for _ in 0..REPS {
            let out = engine
                .viewport(&session, ViewportRequest::new("s0", zoom, bbox, 30))
                .expect("viewport");
            ns.push(out.timings.total_ns);
            tiles = out.timings.tiles_resolved;
            rows = out.timings.rows_in_ranges;
        }
        ns.sort_unstable();
        let p50 = ns[ns.len() / 2];
        let p99 = ns[(ns.len() * 99) / 100];
        println!("{label:>16} {zoom:>6} {tiles:>10} {rows:>12} {p50:>12} {p99:>12}");
    }
}
