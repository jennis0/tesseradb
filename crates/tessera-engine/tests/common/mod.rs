//! Shared fixtures for `tessera-engine`'s viewport-side integration tests.
//!
//! The viewport cases and the pin cases live in separate binaries (`tests/viewport.rs` and
//! `tests/pins.rs`). This module holds the fixture corpus, the `EngineConfig`s and the credentials
//! both of them use.
//!
//! Every item carries `ALL_TERM` ("0"); every third item (`source_id % 3 == 0`) additionally
//! carries `SUBSET_TERM` ("1").

// Each integration-test binary compiles this module separately, so a fixture used by only one of
// them is genuinely dead code in the other. Allowing it here is what keeps the binaries from each
// carrying their own copy — which is the drift this module exists to prevent.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, BuildArgs};
use tessera_engine::{default_compute_threads, Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

pub const N_ITEMS: u64 = 10_000;
pub const ALL_TERM: u64 = 0;
pub const SUBSET_TERM: u64 = 1;

/// A fixed, non-degenerate test key — the same canonical vector used across the identity
/// construction's own tests (`tessera_types::identity`'s `CANONICAL_KEY`) and
/// `tessera-build`'s fixture tests, so a mismatch between crates would show up as a vector
/// disagreement rather than an independently-chosen value.
pub const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

pub fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

pub fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// The base config for these tests.
///
/// **`theta_target_marks` is raised above `N_ITEMS` on purpose.** These tests assert *masking* —
/// which items a principal may see — not density. Leaving θ live would make every assertion about a
/// point set depend on the density rule's threshold clause as well, so a masking bug and a θ
/// arithmetic bug would be indistinguishable. Raising the target above the fixture's total visible
/// count saturates θ at every depth, which reduces selection to "serve every visible row up to the
/// cap" and isolates what these tests are for. Density itself is tested in `selection.rs`, against
/// fixtures built for it.
pub fn config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        theta_target_marks: N_ITEMS * 2,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: default_compute_threads(),
        // The server's own defaults for lifecycle §2.2's two pin bounds, deliberately rather than
        // test-specific values: a case that means to exercise the TTL or the cap overrides the one
        // it is testing (`tests/pins.rs` does), so these must never be the reason a test passes.
        // A 300 s TTL means no case here reaches it by elapsing.
        flush_max_age_secs: 90,
        // The built-in default; `tests/scale.rs` is where this knob is exercised.
        max_merged_segment_bytes: None,
        // The built-in merge and coalesce policies (width 4 / floor 16 MiB / width 8);
        // `tests/merge.rs` and `tests/coalesce.rs` are where the configured values are exercised.
        tier_width: None,
        segment_floor_bytes: None,
        coalesce_width: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
    }
}

/// A config whose caps are large enough to never truncate a sample — used by tests asserting
/// membership (counts, exact point sets) rather than a cap itself. θ is saturated here too, for the
/// reason given on [`config`].
pub fn config_uncapped() -> EngineConfig {
    EngineConfig {
        max_k: N_ITEMS as usize,
        k_max_marks: N_ITEMS as usize,
        ..config()
    }
}

/// Every item carries `ALL_TERM`; every third carries `SUBSET_TERM` too.
pub fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![ALL_TERM, SUBSET_TERM]
    } else {
        vec![ALL_TERM]
    }
}

/// Parameterised over item count so the D-G concurrency tests near the end of this file (which
/// need `RowProjection::new` to take long enough to give a race a real window) can ask for a
/// larger synthetic corpus without duplicating the whole writer. [`build_fixture`] is the
/// `N_ITEMS`-sized default every other test in this file uses.
pub fn write_points_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// See [`write_points_n`]'s doc.
pub fn write_pairs_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..n {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Build the fixture bundle at `out` through `tessera_build::build`, over an
/// `n`-item synthetic corpus. See [`write_points_n`]'s doc for why this is parameterised.
pub fn build_fixture_n(out: &Path, points_path: &Path, pairs_path: &Path, n: u64) {
    write_points_n(points_path, n);
    write_pairs_n(pairs_path, n);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
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
    build(&args).expect("fixture build should succeed");
}

/// Build the fixture bundle at `out` through `tessera_build::build`.
pub fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    build_fixture_n(out, points_path, pairs_path, N_ITEMS)
}

/// Build a bundle over inputs the **corpus generator** wrote, with the generator's own schema.
///
/// Separate from [`build_fixture_n`] rather than a parameter on it: that one owns its inputs and
/// its (empty) schema, and the census owns neither — its points carry the generator's five column
/// families and its extent is the generator's. What the two share is `BuildArgs`, which is the
/// point.
pub fn build_corpus_fixture(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &tessera_corpus::Corpus,
) {
    // **Beside the sources, not beside the bundle.** A `source` is a path relative to the
    // document that declares it (`configuration.md` §3), and the generator's declaration names
    // `points.parquet` and `pairs.parquet` — so the document has to sit where they do.
    let config_path = points_path.with_file_name("corpus-config.toml");
    std::fs::create_dir_all(config_path.parent().expect("the sources have a parent")).ok();
    std::fs::write(&config_path, corpus.config_toml()).expect("the generator's config is writable");
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the generator's config parses");
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: corpus.extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &config.schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
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
        schema: config.schema,
    };
    build(&args).expect("the census fixture builds");
}

/// The generator's corpus built **with its layers** — the five artifact arms its declaration
/// carries, materialised beside the points and published into the bundle.
///
/// Separate from [`build_corpus_fixture`] rather than a flag on it: that one exists so a census can
/// register and publish its *own* layer against the generator's memberships, and reads none of the
/// artifact fixtures. This one is for the cases that are about the declaration — a layer built by
/// rule beside the same layer built by list — where what is under test is precisely what
/// `tessera build` does with the generator's own `[[layer]]` blocks.
pub fn build_corpus_fixture_with_layers(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &tessera_corpus::Corpus,
) {
    let dir = points_path.parent().expect("the sources have a parent");
    corpus
        .write_artifact_fixtures(dir)
        .expect("the generator's artifact fixtures are writable");
    let config_path = points_path.with_file_name("corpus-config.toml");
    std::fs::write(&config_path, corpus.config_toml()).expect("the generator's config is writable");
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the generator's config parses");
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: corpus.extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &config.schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    build(&args).expect("the generator's own layers build");
}

/// The generator's points and access relation, under a **caller-supplied** declaration.
///
/// For the cases whose subject is a layer the generator does not declare — a spatial layer with
/// boxes, say, which its boundary arm has a roster for and no geometry. The points, the pairs and
/// the extent are the generator's, so its closed forms are still the oracle; the layers are the
/// case's own.
pub fn build_with_layers(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &tessera_corpus::Corpus,
    config: tessera_build::config::Config,
) {
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: corpus.extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &config.schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    build(&args).expect("the fixture's own declaration builds");
}

/// Read the bundle's external-ids extent into a `source_id -> new entity_id` map — the same
/// ground truth `tessera-build`'s own smoke test cross-checks against.
pub fn source_to_new_map(bundle_root: &Path, prefix: &str) -> BTreeMap<u64, u64> {
    let bundle = open_bundle(bundle_root).unwrap();
    let part = &bundle.partitions["default"];
    let ext_path = bundle_root
        .join(prefix)
        .join(&part.manifest.external_id_runs[0]);
    let file = File::open(&ext_path).unwrap();
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let mut map = BTreeMap::new();
    for batch in reader {
        let batch = batch.unwrap();
        let ext = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        // Contracts r6: the external-id extent's entity column is `UInt32` (entities are capped
        // at `u32::MAX` by the I9 allocator), not the pre-r6 `UInt64`.
        let ent = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
            map.insert(source, ent.value(i) as u64);
        }
    }
    map
}

/// The external id `tessera-build` writes for a source row: the source corpus id, 8 bytes
/// little-endian (see `source_to_new_map`'s decode of the same convention).
pub fn source_id_key(source_id: u64) -> Vec<u8> {
    source_id.to_le_bytes().to_vec()
}

/// An engine with its write executor running.
///
/// **Publication is a submission now** (lifecycle §1.3): `Engine::publish_geometry` hands the
/// swap to the executor thread, so an engine that never started one answers `NoExecutor` rather
/// than publishing. That is the correct posture — there is no honest "published" when there is no
/// publisher — so a test that publishes starts the thread, exactly as one that submits a
/// `/control/changes` entry already had to.
pub fn open_engine_publishing(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    let mut engine = open_engine(bundle_root, cache_dir, wal_path);
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    engine
}

pub fn open_engine(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        Passthrough::new(),
        config(),
    )
    .expect("engine should open against a freshly built bundle")
}

pub fn open_engine_uncapped(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        Passthrough::new(),
        config_uncapped(),
    )
    .expect("engine should open against a freshly built bundle")
}

pub fn full_coverage_credential() -> Vec<u8> {
    br#"{"terms": ["0"]}"#.to_vec()
}

pub fn subset_credential() -> Vec<u8> {
    br#"{"terms": ["1"]}"#.to_vec()
}

pub fn zero_credential() -> Vec<u8> {
    br#"{"terms": []}"#.to_vec()
}
