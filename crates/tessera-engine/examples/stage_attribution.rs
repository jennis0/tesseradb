//! Stage attribution for the tile-loop fan-out's c=1 overhead: a measured `threads=default` c=1
//! p50 of 1.118 ms against 0.376 ms at `threads=1` — 2.97x *slower* with more threads.
//!
//! The number on its own does not say where the time goes. This attributes it, using the
//! `bench-timing` feature's per-request [`tessera_engine::StageTimings`] rather than inference.
//!
//! **Method.** `StageTimings`' serial-prefix fields (`generation_resolve_ns` through
//! `tile_ranges_ns`, plus `theta_anchor_ns`) keep their wall-clock meaning at every
//! `compute_threads` (`timing.rs`'s doc) — none of that work runs inside `pool.install`. Their
//! sum is therefore directly comparable between the two thread configurations. What is NOT
//! separately timed is the parallel section itself (`Probe::skip()` deliberately discards its
//! wall time rather than misattributing it — see `Engine::viewport`'s call site): so this example
//! reports `total_ns - serial_prefix_sum` as "parallel section wall" — the pool.install call plus
//! the trailing serial fold, dominated by the former at this fixture's per-tile cost. Comparing
//! that quantity across thread configs isolates whether the overhead is scheduling/pool-entry
//! (would show as `parallel section wall` growing with no matching growth in the summed per-tile
//! stage fields) or genuine per-tile work (would show as the per-tile sums growing too).
//!
//! Run: `cargo run --release --example stage_attribution -p tessera-engine --features bench-timing`
//!
//! Without `--features bench-timing` every field is zero (the feature's whole zero-cost-when-off
//! discipline) and this example prints that observation rather than a meaningless zero table.

use std::path::{Path, PathBuf};

use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const ITEM_LIMIT: u64 = 2_422_486;
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const ZOOM: u8 = 8;
/// Iterations averaged per thread config, after one discarded warm-up (the row-projection cache
/// fill, measured at 8.8 s for a 69M mask — excluded from every harness in this repo for the same
/// reason).
const REPS: usize = 300;

fn extent() -> Bounds {
    Bounds {
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

/// Same fixture and convention as `benches/viewport.rs::ensure_bundle` — `/tmp/tessera-2m4`,
/// built by `tessera-engine/tests/viewport.rs`'s ignored `latency_sanity_at_2_4m_p99_under_50ms`
/// test, `scripts/bench_build_fixtures.sh` (which symlinks this exact path to its own
/// `$FIXTURES/2422486/categories-subclass` convention), or rebuilt here if both are missing.
fn ensure_bundle() -> PathBuf {
    let bundle_root = PathBuf::from("/tmp/tessera-2m4");
    if !bundle_root.join("CURRENT").exists() {
        let root = workspace_root();
        let args = BuildArgs {
            point_fields: Default::default(),
            corpus_fields: Default::default(),
            points: root.join("data/scaled/geometry.parquet"),
            corpus: Some(root.join("data/scaled/geometry.parquet")),
            access: tessera_build::config::AccessInput::relation(root.join("data/scaled/pairs/categories-subclass.pairs.parquet")),
            out: bundle_root.clone(),
            extent: extent(),
            view_id: "s0".to_string(),
            limit: Some(ITEM_LIMIT),
            identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
            identity_key_hex: TEST_KEY_HEX.to_string(),
            idset: 1,
            shard_id: 0,
            layers: Vec::new(),
            layer_inputs: Vec::new(),
            mint_external_ids: false,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: Default::default(),
            emit_oracle_pairs: false,
        };
        build(&args).expect("2.4M fixture build should succeed");
    }
    bundle_root
}

/// Every dictionary descriptor, in `TermId` order — a length-prefixed sequence of `u32 LE len ||
/// utf8 bytes` records.
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

/// `w` descriptors spread evenly across the dictionary — a w=10 grant matching the measured
/// condition (w=10, k=30, zoom=8) rather than a single term, which
/// selects a far denser mask than the condition actually measured —
/// see this file's stage-attribution finding. Spread rather than the first `w` because the
/// dictionary's term order is not known to be frequency-independent.
fn spread_descriptors(all: &[String], w: usize) -> Vec<String> {
    let step = (all.len() / w).max(1);
    (0..w)
        .map(|i| all[(i * step) % all.len()].clone())
        .collect()
}

/// Copy of `tessera-bench::corpus::gen_viewports`'s algorithm (identical RNG, identical formula),
/// so `viewports(1, ..., seed, &[ZOOM])[0]` is bit-for-bit the same first viewport
/// `bench_concurrency.py`'s c=1 cell hits (its `load` arm draws `gen_viewports(256, 65536.0,
/// seed, &[zoom])` and worker 0 starts at index 0) — not duplicated logic, the same draw.
/// `tessera-engine` cannot depend on `tessera-bench` (`scripts/check-layers.sh`: nothing may
/// depend on the harness crate), so this is copied rather than imported.
fn viewports(n: usize, extent: f64, seed: u64, zoom: u8) -> Vec<[f64; 4]> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let span = extent / 2f64.powi(zoom.saturating_sub(4).max(1) as i32);
    (0..n)
        .map(|_| {
            let x0 = rng.gen_range(0.0..(extent - span).max(f64::MIN_POSITIVE));
            let y0 = rng.gen_range(0.0..(extent - span).max(f64::MIN_POSITIVE));
            [x0, y0, x0 + span, y0 + span]
        })
        .collect()
}

#[derive(Default, Clone, Copy)]
struct Sums {
    generation_resolve_ns: u128,
    stamp_compare_ns: u128,
    view_lookup_ns: u128,
    row_projection_ns: u128,
    compose_ns: u128,
    theta_anchor_ns: u128,
    tiles_for_bbox_ns: u128,
    tile_ranges_ns: u128,
    total_ns: u128,
    // Per-tile fields: cross-worker CPU-time sums at threads > 1, genuine wall at threads = 1
    // (timing.rs's doc) — reported for reference, not as a wall-clock partition.
    count_ns: u128,
    select_ns: u128,
    gather_ns: u128,
    tiles_nonempty: u128,
    sigma_visible: u128,
}

fn main() {
    if !cfg!(feature = "bench-timing") {
        println!(
            "built without `bench-timing` — every StageTimings field is zero by design; \
             re-run with `--features bench-timing` to see the breakdown."
        );
        return;
    }

    let bundle_root = ensure_bundle();
    let all = all_descriptors(&bundle_root);
    let terms = spread_descriptors(&all, 10);
    // The measured condition: seed 0, zoom 8, the FIRST viewport `bench_concurrency.py`'s c=1
    // cell draws (see `viewports`' doc).
    let bbox = viewports(1, 65536.0, 0, ZOOM)[0];

    println!(
        "2.42M categories-subclass, w={} (spread grant), zoom {ZOOM}, bbox {bbox:?}, {REPS} reps \
         averaged (1 warm-up discarded)\n",
        terms.len()
    );

    for (label, compute_threads) in [
        ("threads=1", 1),
        ("threads=default", tessera_engine::default_compute_threads()),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            &bundle_root,
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
            Passthrough::new(),
            EngineConfig {
                // `tessera-server`'s own defaults (`config.rs`'s `DEFAULT_*` constants) — this
                // must measure the deployment the server actually runs, not an arbitrary config
                // (`tessera-bench/src/arms/viewport.rs`'s own doc makes the same point).
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
            },
        )
        .expect("engine should open the 2.4M bundle");

        let terms_json = terms
            .iter()
            .map(|t| format!("{t:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let auth = format!(r#"{{"terms": [{terms_json}]}}"#);
        let session = engine.authorise(auth.as_bytes()).expect("authorise");

        // Warm-up: pays the row-projection cache fill, excluded from every figure below.
        let _ = engine
            .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, 30))
            .expect("warm-up viewport");

        let mut sums = Sums::default();
        let mut totals = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let out = engine
                .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, 30))
                .expect("viewport");
            let t = out.timings;
            sums.generation_resolve_ns += t.generation_resolve_ns as u128;
            sums.stamp_compare_ns += t.stamp_compare_ns as u128;
            sums.view_lookup_ns += t.view_lookup_ns as u128;
            sums.row_projection_ns += t.row_projection_ns as u128;
            sums.compose_ns += t.compose_ns as u128;
            sums.theta_anchor_ns += t.theta_anchor_ns as u128;
            sums.tiles_for_bbox_ns += t.tiles_for_bbox_ns as u128;
            sums.tile_ranges_ns += t.tile_ranges_ns as u128;
            sums.total_ns += t.total_ns as u128;
            sums.count_ns += t.count_ns as u128;
            sums.select_ns += t.select_ns as u128;
            sums.gather_ns += t.gather_ns as u128;
            sums.tiles_nonempty += t.tiles_nonempty as u128;
            sums.sigma_visible += t.sigma_visible as u128;
            totals.push(t.total_ns);
        }
        totals.sort_unstable();
        let n = REPS as u128;
        let avg = |x: u128| x / n;
        let serial_prefix = avg(sums.generation_resolve_ns)
            + avg(sums.stamp_compare_ns)
            + avg(sums.view_lookup_ns)
            + avg(sums.row_projection_ns)
            + avg(sums.compose_ns)
            + avg(sums.theta_anchor_ns)
            + avg(sums.tiles_for_bbox_ns)
            + avg(sums.tile_ranges_ns);
        let avg_total = avg(sums.total_ns);
        let parallel_section = avg_total.saturating_sub(serial_prefix);
        let p50 = totals[totals.len() / 2];
        let p99 = totals[(totals.len() * 99) / 100];

        println!("== {label} (compute_threads = {compute_threads}) ==");
        println!("  total_ns            avg={avg_total:>8}  p50={p50:>8}  p99={p99:>8}");
        println!("  serial prefix sum   avg={serial_prefix:>8}");
        println!(
            "    generation_resolve_ns  {:>8}",
            avg(sums.generation_resolve_ns)
        );
        println!(
            "    stamp_compare_ns         {:>8}",
            avg(sums.stamp_compare_ns)
        );
        println!(
            "    view_lookup_ns        {:>8}",
            avg(sums.view_lookup_ns)
        );
        println!(
            "    row_projection_ns      {:>8}",
            avg(sums.row_projection_ns)
        );
        println!("    compose_ns             {:>8}", avg(sums.compose_ns));
        println!(
            "    theta_anchor_ns        {:>8}",
            avg(sums.theta_anchor_ns)
        );
        println!(
            "    tiles_for_bbox_ns      {:>8}",
            avg(sums.tiles_for_bbox_ns)
        );
        println!("    tile_ranges_ns         {:>8}", avg(sums.tile_ranges_ns));
        println!(
            "  parallel section (pool.install + fold), = total - serial prefix   avg={parallel_section:>8}"
        );
        println!(
            "    [reference, cross-worker sum at threads>1] count_ns={:>7} select_ns={:>7} \
             gather_ns={:>7}",
            avg(sums.count_ns),
            avg(sums.select_ns),
            avg(sums.gather_ns)
        );
        println!(
            "    tiles_nonempty={} sigma_visible={}\n",
            avg(sums.tiles_nonempty),
            avg(sums.sigma_visible)
        );
    }
}
