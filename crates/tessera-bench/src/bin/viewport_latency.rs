//! **p99 viewport latency at 2.4M items** — the generous local gate (50 ms) ahead of the real
//! 10 ms gate, which is the exit measurement at 10⁹.
//!
//! 300 random viewports at depths 4..=12 over a seeded RNG, so the shape is the same from run to
//! run, timed against `Engine` directly. The bundle is built from `data/scaled/` at the
//! 2,422,486-item prefix and kept: `/tmp/tessera-2m4` is the path
//! `scripts/bench_build_fixtures.sh` links and `tessera-engine`'s criterion bench opens, so the
//! three share one copy of the bytes.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin viewport_latency
//! cargo run --release -p tessera-bench --bin viewport_latency -- --bundle /tmp/x --p99-ms 10
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

/// The prefix the shared-context fixtures are cut at.
const ITEM_LIMIT: u64 = 2_422_486;

/// The fixed, non-degenerate key the engine's own fixtures use. Arbitrary here — nothing inverts
/// a `tessera_id` — but a real key, since `IdentityKey::from_hex` refuses degenerate ones.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// Server defaults, so this measures a deployment somebody runs.
const MAX_K: usize = 1_000;
const K_MAX_MARKS: usize = 500;
const THETA_TARGET: u64 = 16;

#[derive(Parser)]
#[command(about = "p99 viewport latency over a 2.4M-item bundle, built from data/scaled/ if absent")]
struct Args {
    /// Bundle root (the directory holding `CURRENT`), built here if it does not exist.
    #[arg(long, default_value = "/tmp/tessera-2m4")]
    bundle: PathBuf,
    /// Fail (exit 1) unless the measured p99 is under this many milliseconds.
    #[arg(long, default_value_t = 50)]
    p99_ms: u128,
    /// Viewports to time.
    #[arg(long, default_value_t = 300)]
    samples: usize,
    /// RNG seed for the viewport shapes.
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

/// Identity extent (contracts §2.5 grid): `geometry.parquet` stores Morton codes, not
/// coordinates, and `read_points`'s Morton branch requires this exact extent.
fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

/// The workspace root, resolved from this crate rather than the working directory, which a
/// `cargo run` from a subdirectory does not fix for us.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Build the 2.4M bundle at `bundle_root`, or say what is missing.
fn ensure_bundle(bundle_root: &Path) -> Result<(), String> {
    if bundle_root.join("CURRENT").exists() {
        return Ok(());
    }
    let root = workspace_root();
    let geometry = root.join("data/scaled/geometry.parquet");
    let pairs = root.join("data/scaled/pairs/categories-subclass.pairs.parquet");
    let missing: Vec<String> = [&geometry, &pairs]
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "no bundle at {} and the corpus it would be built from is missing ({}), so run \
             scripts/bench_build_fixtures.sh on a machine that has data/scaled/ or pass --bundle \
             pointing at a bundle that exists.",
            bundle_root.display(),
            missing.join(", ")
        ));
    }

    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: geometry,
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: bundle_root.to_path_buf(),
        limit: Some(ITEM_LIMIT),
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args)
        .map(|_report| ())
        .map_err(|e| format!("the 2.4M fixture build failed: {e}"))
}

/// A real descriptor from the built dictionary — the corpus's own term ids, read rather than
/// guessed.
fn first_dictionary_descriptor(bundle_root: &Path) -> String {
    let dict_path = bundle_root.join("v00000/dictionary/terms-0.dict");
    let data = std::fs::read(dict_path).unwrap();
    let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    String::from_utf8(data[4..4 + len].to_vec()).unwrap()
}

/// `q`th percentile of an already-sorted slice, by nearest rank.
fn percentile(sorted: &[Duration], q: f64) -> Duration {
    let idx = ((sorted.len() as f64) * q) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn measure(args: &Args) -> Result<Vec<Duration>, String> {
    ensure_bundle(&args.bundle)?;

    let tmp = std::env::temp_dir().join(format!("tessera-viewport-latency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let engine = Engine::open(
        &args.bundle,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // Production defaults, deliberately: under a saturated θ the threshold clause never
            // binds and the counting/selecting branch is barely exercised, which would measure a
            // path the server does not take. Keep these in step with `tessera-server`'s DEFAULT_*.
            max_k: MAX_K,
            k_min: 2,
            k_max_marks: K_MAX_MARKS,
            theta_target_marks: THETA_TARGET,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
            flush_max_age_secs: 90,
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )
    .map_err(|e| format!("the engine could not open {}: {e}", args.bundle.display()))?;

    let descriptor = first_dictionary_descriptor(&args.bundle);
    let auth = format!(r#"{{"terms": ["{descriptor}"]}}"#);
    // Warm token: authorise once, outside the timing loop — a session's fragment is built once at
    // authorise time (I2) and reused across every viewport, exactly as a real client would.
    let session = engine
        .authorise(auth.as_bytes())
        .map_err(|e| format!("authorise failed: {e}"))?;

    let mut rng = StdRng::seed_from_u64(args.seed);
    let mut latencies = Vec::with_capacity(args.samples);
    for _ in 0..args.samples {
        let x0: f64 = rng.gen_range(0.0..65000.0);
        let y0: f64 = rng.gen_range(0.0..65000.0);
        let x1 = (x0 + rng.gen_range(1.0..500.0)).min(65536.0);
        let y1 = (y0 + rng.gen_range(1.0..500.0)).min(65536.0);
        let zoom: u8 = rng.gen_range(4..=12);

        let start = Instant::now();
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", zoom, [x0, y0, x1, y1], 30),
            )
            .map_err(|e| format!("viewport failed: {e}"))?;
        latencies.push(start.elapsed());
    }

    drop(engine);
    let _ = std::fs::remove_dir_all(&tmp);
    latencies.sort();
    Ok(latencies)
}

fn main() -> ExitCode {
    let args = Args::parse();
    let latencies = match measure(&args) {
        Ok(latencies) => latencies,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let p99 = percentile(&latencies, 0.99);
    println!(
        "{} viewports at {ITEM_LIMIT} items: p50 {:?}, p90 {:?}, p99 {:?}, max {:?}",
        latencies.len(),
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.90),
        p99,
        latencies[latencies.len() - 1],
    );
    if p99.as_millis() >= args.p99_ms {
        eprintln!("p99 {p99:?} is at or over the {} ms gate", args.p99_ms);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
