//! Criterion micro-benches at 2.4M items — the regression gate ahead of the
//! 10⁹ exit measurement (`scripts/bench_p99.py`, run once, out of scope for `cargo bench`).
//!
//! Reuses `/tmp/tessera-2m4` (built by `tessera-engine/tests/viewport.rs`'s ignored
//! `latency_sanity_at_2_4m_p99_under_50ms` test, or rebuilt here if missing — shared-context
//! 2.4M is the "validate" scale; 10⁹ is exit-only).
//!
//! Four groups:
//! 1. `fragment_build` — `tessera_authz::build_fragment` directly against the real postings, for
//!    w ∈ {10², 10⁴} terms (the build path itself, not `Engine::authorise`'s cache wrapper —
//!    `FragmentCache::get_or_build` memoises by (terms, auth hash), which would only measure the
//!    cache hit on iterations after the first).
//! 2. `compose` — `RowProjection::new` + `compose()` against an empty `Overlay`/`IngestBuffer`
//!    (a freshly built bundle has no changes), reusing a `FrozenFragment` built once via
//!    `FragmentCache::get_or_build` outside the timed loop (per the comment on
//!    `RowProjection::new`: never reconstruct the projection on a per-viewport path — one build
//!    is paid up front here, same discipline the real serving path uses).
//! 3. `viewport_tile_sweep_k0` — the tile-count-only path (`k=0`, no rows gathered) over a bbox
//!    touching ~300 tiles.
//! 4. `viewport_gather_k30` — the same sweep with `k=30` (≤300×30 rows gathered), isolating the
//!    row-projection scalar-gather cost added on top of (3).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::TempDir;

use tessera_authz::{build_fragment, FragmentCache, PostingsReader};
use tessera_build::{build, BuildArgs};
use tessera_engine::compose::{compose, RowProjection};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::{IdentityKey, TermId};

const ITEM_LIMIT: u64 = 2_422_486;

/// The same fixed, non-degenerate test key `tests/viewport.rs` and `tessera-build`'s own fixture
/// tests use — arbitrary here (this bench never inverts a `tessera_id`), but a real key all the
/// same, since `IdentityKey::from_hex` refuses degenerate ones.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn workspace_root() -> PathBuf {
    // `cargo bench`'s working directory is not guaranteed to be the workspace root (unlike
    // `cargo test`, which the sibling `tests/viewport.rs` relies on) — resolve relative to this
    // crate's manifest directory instead.
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
            mint_external_ids: true,
            emit_oracle_pairs: true,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: Default::default(),
        };
        build(&args).expect("2.4M fixture build should succeed");
    }
    bundle_root
}

fn first_dictionary_descriptor(bundle_root: &Path) -> String {
    let dict_path = bundle_root.join("v00000/dictionary/terms-0.dict");
    let data = std::fs::read(dict_path).unwrap();
    let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    String::from_utf8(data[4..4 + len].to_vec()).unwrap()
}

fn bench_fragment_build(c: &mut Criterion) {
    let bundle_root = ensure_bundle();
    let postings_path = bundle_root.join("v00000/partitions/default/terms/postings.arrow");
    let postings = PostingsReader::open(&postings_path, true).expect("postings should open");
    let vocab = postings.term_count();

    let mut group = c.benchmark_group("fragment_build");
    for &w in &[100u32, 10_000u32] {
        let w = w.min(vocab);
        let terms: Vec<TermId> = (0..w).map(TermId::new).collect();
        group.bench_with_input(BenchmarkId::from_parameter(w), &terms, |b, terms| {
            b.iter(|| build_fragment(terms, &postings).expect("fragment build should succeed"));
        });
    }
    group.finish();
}

fn bench_compose(c: &mut Criterion) {
    let bundle_root = ensure_bundle();
    let postings_path = bundle_root.join("v00000/partitions/default/terms/postings.arrow");
    let postings = PostingsReader::open(&postings_path, true).expect("postings should open");
    let vocab = postings.term_count();
    let terms: Vec<TermId> = (0..vocab.min(1000)).map(TermId::new).collect();

    let cache_tmp = TempDir::new().unwrap();
    let cache = FragmentCache::new(cache_tmp.path(), [0u8; 32], [1u8; 32]);
    let fragment = cache
        .get_or_build(&terms, [2u8; 32], 0, &postings, &[], ITEM_LIMIT)
        .expect("fragment build should succeed");

    let bundle = open_bundle(&bundle_root).expect("bundle should open");
    let view = &bundle.partitions["default"].views["s0"];
    let base = Arc::new(RowProjection::new(&fragment, &view.row_space));

    let satisfied: rustc_hash::FxHashSet<TermId> = terms.iter().copied().collect();
    let overlay = Overlay::new();
    let buffer = IngestBuffer::new();
    // Derived once, outside the timed loop, exactly as a publication derives it: the deny half of
    // composition is one `andnot` inside the loop whatever the deny depth, which is the point.
    let denied = tessera_engine::denied_rows_of(&overlay, &view.row_space);

    c.bench_function("compose", |b| {
        b.iter(|| {
            compose(
                &satisfied,
                &overlay,
                &buffer,
                Arc::clone(&base),
                &view.row_space,
                &denied,
            )
        });
    });
}

fn bench_viewport(c: &mut Criterion) {
    let bundle_root = ensure_bundle();
    let tmp = TempDir::new().unwrap();
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        // The production defaults, deliberately: this bench exists to measure what the server
        // actually does, so θ is live here rather than saturated the way the correctness tests
        // configure it. Note that recorded baselines from before §7.2's density rule landed are
        // **not** comparable — the selection path now reads `tessera_id` per visible row where the
        // placeholder read nothing, and the number of gathered points is θ-dependent rather than
        // `min(k, visible)`.
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // Raised from 200/128 so `gather_k500` measures a genuine cap of 500 — the
            // deployment operating point (owner directive 2026-07-30). Deliberately NOT a
            // semantics change for the other groups: `cap = min(request_k, k_max_marks)`, so
            // k=0 and k=30 produce the same cap under either limit and those baselines stay
            // comparable across the change.
            max_k: 500,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
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

    let descriptor = first_dictionary_descriptor(&bundle_root);
    let auth = format!(r#"{{"terms": ["{descriptor}"]}}"#);
    let session = engine
        .authorise(auth.as_bytes())
        .expect("authorise should succeed");

    // zoom 8 gives a 256x256 tile grid, and 23,170 of 65,536 units covers tile coordinates 0..=90
    // per axis — **8,281 tiles spanning 242,221 rows**, measured directly with `tiles_for_bbox` +
    // `tile_ranges_all` on this fixture (`docs/evidence/memos/2026-07-31-viewport-bench-regression.md`,
    // reproduced as the `f35/z8` cell of `probes/2026-08-01-two-axis-sweep/`).
    //
    // **Not a ~300-tile shape**, however much it resembles the calibration sweep's z8 cell (289
    // tiles, span 4,096) — this one is 28x that. The distinction is load-bearing: this bench is the
    // only high-tile-count probe in the tree, and mislabelling it as ~300 tiles is how the
    // serial/parallel calibration came to have measured nothing above 1,024 tiles unnoticed. Keep
    // the shape.
    const ZOOM: u8 = 8;
    let bbox = [0.0, 0.0, 23170.0, 23170.0];

    let mut group = c.benchmark_group("viewport");
    group.bench_function("tile_sweep_k0", |b| {
        b.iter(|| {
            engine
                .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, 0))
                .expect("viewport should succeed")
        });
    });
    group.bench_function("gather_k30", |b| {
        b.iter(|| {
            engine
                .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, 30))
                .expect("viewport should succeed")
        });
    });
    // k=500 is the deployment operating point (k defaults to the cap); k=30 is kept above it for
    // comparability with the recorded 2.4M baselines, not because anything still requests 30.
    group.bench_function("gather_k500", |b| {
        b.iter(|| {
            engine
                .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, 500))
                .expect("viewport should succeed")
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench_fragment_build, bench_compose, bench_viewport
}
criterion_main!(benches);
