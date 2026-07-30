//! Task 16, Step 1: criterion micro-benches at 2.4M items — the regression gate ahead of the
//! 10⁹ exit measurement (`scripts/bench_p99.py`, run once, out of scope for `cargo bench`).
//!
//! Reuses `/tmp/tessera-2m4` (built by `tessera-engine/tests/viewport.rs`'s ignored
//! `latency_sanity_at_2_4m_p99_under_50ms` test, or rebuilt here if missing — shared-context
//! constraint 7: 2.4M is the "validate" scale, 10⁹ is exit-only).
//!
//! Four groups, matching the brief:
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
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_plugin::Passthrough;
use tessera_spatial::Extent;
use tessera_store::read::open_bundle;
use tessera_types::{IdentityKey, TermId};

const ITEM_LIMIT: u64 = 2_422_486;

/// The same fixed, non-degenerate test key `tests/viewport.rs` and `tessera-build`'s own fixture
/// tests use — arbitrary here (this bench never inverts a `tessera_id`), but a real key all the
/// same, since `IdentityKey::from_hex` refuses degenerate ones.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn extent() -> Extent {
    Extent {
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
        .get_or_build(&terms, [2u8; 32], &postings, ITEM_LIMIT)
        .expect("fragment build should succeed");

    let bundle = open_bundle(&bundle_root).expect("bundle should open");
    let slice = &bundle.partitions["default"].slices["s0"];
    let base = Arc::new(RowProjection::new(&fragment, &slice.permutation));

    let satisfied: rustc_hash::FxHashSet<TermId> = terms.iter().copied().collect();
    let overlay = Overlay::new();
    let buffer = IngestBuffer::new();

    c.bench_function("compose", |b| {
        b.iter(|| {
            compose(
                &fragment,
                &satisfied,
                &overlay,
                &buffer,
                Arc::clone(&base),
                &slice.permutation,
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
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
        },
    )
    .expect("engine should open the 2.4M bundle");

    let descriptor = first_dictionary_descriptor(&bundle_root);
    let auth = format!(r#"{{"terms": ["{descriptor}"]}}"#);
    let session = engine
        .authorise(auth.as_bytes())
        .expect("authorise should succeed");

    // zoom 8 gives a 256x256 tile grid; a bbox covering roughly one eighth of the extent touches
    // on the order of 300 tiles at this zoom, matching the brief's "~300 tiles" sweep.
    const ZOOM: u8 = 8;
    let bbox = [0.0, 0.0, 23170.0, 23170.0];

    let mut group = c.benchmark_group("viewport");
    group.bench_function("tile_sweep_k0", |b| {
        b.iter(|| {
            engine
                .viewport(&session, "s0", ZOOM, bbox, 0, None)
                .expect("viewport should succeed")
        });
    });
    group.bench_function("gather_k30", |b| {
        b.iter(|| {
            engine
                .viewport(&session, "s0", ZOOM, bbox, 30, None)
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
