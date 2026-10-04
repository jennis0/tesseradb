//! What `Engine::open` does, and what it refuses: the allocator seed it takes from durable
//! state, the key runs it must not read, and the
//! `EngineConfig` values it will not serve under.

mod common;

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use tessera_engine::{Engine, EngineConfig, EngineError, IngestRequest};
use tessera_lifecycle::{ChangeOp, IngestRow};
use tessera_types::TesseraId;
use tessera_lifecycle::wal::{Wal, WalRecord};

use common::*;

/// `Engine::open` seeds the I9 allocator at `max(manifest high-water, WAL high-water)` — here,
/// with an empty WAL, that is exactly the bundle's `entity_id_high_water` (== `N_ITEMS`, the
/// bootstrap build's dense `0..n` assignment).
#[test]
fn engine_open_seeds_the_allocator_from_the_manifest_high_water() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    assert_eq!(engine.allocator_high_water(), N_ITEMS);
}

/// S6: `Allocator::try_new`'s own doc calls it "the check that belongs at open", and `Engine::open`
/// is open — it must refuse a seed at or above `u32::MAX` before any ingest, rather than let the
/// first allocation surface it as an opaque exhaustion error. The seed comes from durable state
/// this process did not write (MANIFEST's high-water, or a replayed WAL lease), so a hand-edited or
/// corrupt value has to fail closed here.
#[test]
fn engine_open_refuses_an_out_of_range_allocator_seed() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // A replayed row at the top of the u32 space — `high_water_from` folds
    // `entity_id + 1` into the seed, so this is the WAL-side half of the seeding rule, reached
    // without touching MANIFEST's digest. The refusal fires before replay resolves anything, so
    // the row's other fields never matter.
    let wal_path = tmp.path().join("wal.log");
    {
        let mut wal = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::IngestBatch {
            edits: Vec::new(),
            receipt: Vec::new(),
            batch_id: "over-the-top".to_string(),
            body_hash: [0u8; 32],
            rows: vec![tessera_lifecycle::WalRow {
                entity_id: tessera_types::EntityId::new(u32::MAX as u64 - 1),
                view: "s0".to_string(),
                join: false,
                descriptors: Vec::new(),
                x: 0.5,
                y: 0.5,
                scalars: Vec::new(),
                scoped: Vec::new(),
            }],
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let opened = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &wal_path,
        config(),
    );
    let Err(err) = opened else {
        panic!("a seed at u32::MAX must be refused at open, not at the first ingest");
    };
    assert!(
        matches!(err, EngineError::Malformed(ref d) if d.contains("allocator")),
        "expected a typed refusal naming the allocator, got {err:?}"
    );
}

/// Residency proxy: `Engine::open` must never read a unique column's key runs, which open on the
/// first lookup that needs them.
///
/// **The files are removed from disk before the engine opens**, so any read of them, at any layer,
/// is a hard failure of `Engine::open`, which is the property claimed.
#[test]
fn engine_open_does_not_touch_the_key_runs() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // Every key run the build wrote, named from MANIFEST's own `files` map rather than guessed, so
    // this cannot silently check nothing if the layout moves.
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle_root.join("CURRENT")).unwrap()).unwrap();
    let prefix_dir = bundle_root.join(current["prefix"].as_str().unwrap());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(prefix_dir.join("MANIFEST.json")).unwrap()).unwrap();
    let runs: Vec<PathBuf> = manifest["files"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|rel| rel.ends_with(".keys"))
        .map(|rel| prefix_dir.join(rel))
        .collect();
    assert!(!runs.is_empty(), "the fixture's unique `id` writes key runs");
    for path in &runs {
        std::fs::remove_file(path).unwrap();
    }

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    // Deferred, not dropped: the first lookup *does* reach for a run, and fails closed because it
    // is gone.
    assert!(
        item_of_id(&engine, 0).is_err(),
        "the first lookup must reach the (now absent) run and fail closed"
    );
}

fn open(config: EngineConfig) -> Result<Engine, EngineError> {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
}

#[test]
fn a_config_that_draws_nothing_in_a_sparse_tile_does_not_open() {
    let refused = open(EngineConfig { k_min: 0, ..config() });
    assert!(matches!(refused, Err(EngineError::ConfigRefused(_))));
}

#[test]
fn a_config_whose_merge_or_coalesce_could_never_select_does_not_open() {
    for width in [0, 1] {
        let refused = open(EngineConfig { tier_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "tier_width {width}");
        let refused = open(EngineConfig { coalesce_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "coalesce_width {width}");
    }
}

#[test]
fn the_smallest_lawful_values_open() {
    let opened = open(EngineConfig {
        k_min: 1,
        tier_width: Some(2),
        coalesce_width: Some(2),
        ..config()
    });
    assert!(opened.is_ok());
}

/// An engine over the bundle at `root`, its cache and log beside it, its executor running.
fn serving(root: &Path) -> Engine {
    let dir = root.parent().expect("the bundle sits in a directory");
    let mut engine = open_engine(root, &dir.join("cache"), &dir.join("wal.log"));
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

/// Create one item per name in view `s0`, each holding the `id` of its name, as one batch, and
/// answer their `tessera_id`s.
fn create(engine: &Engine, batch: &str, names: &[&str]) -> Vec<TesseraId> {
    let rows = names
        .iter()
        .enumerate()
        .map(|(i, name)| IngestRow {
            tessera_id: None,
            labels: Some(vec![b"0".to_vec()]),
            position: Some((1.0 + i as f64, 1.0)),
            scalars: keyed(name),
            scoped: Vec::new(),
            omitted: Vec::new(),
        })
        .collect();
    engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash: [7u8; 32],
            view: Some("s0".to_string()),
            rows,
            artifacts: Default::default(),
            strict: false,
            tessera_id_column: false,
        })
        .expect("the batch is accepted")
        .tessera_ids
        .into_iter()
        .map(|id| id.expect("an accepted row has a tessera_id"))
        .collect()
}

/// **The buffered rows an open reports are the rows it replayed**, before any write.
#[test]
fn the_buffered_rows_reported_after_a_restart_are_the_rows_replayed() {
    let tmp = TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    let engine = serving(&root);
    create(&engine, "three", &["a", "b", "c"]);
    assert_eq!(engine.buffered_items(), 3);
    drop(engine);
    assert_eq!(serving(&root).buffered_items(), 3);
}

/// **A deleted item's `tessera_id` is never given to another item**, even once a fold has removed
/// it and a restart has read the allocator back.
#[test]
fn a_deleted_items_tessera_id_is_not_issued_again_after_a_fold_and_a_restart() {
    let tmp = TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    let engine = serving(&root);
    let first = create(&engine, "first", &["first"])[0];
    let entity = engine.resolve_tessera_ids(&[first]).unwrap()[0].expect("the item it created");
    engine.accept_change(entity, ChangeOp::Delete).unwrap();
    fold(&engine);
    drop(engine);
    let engine = serving(&root);
    let second = create(&engine, "second", &["second"])[0];
    assert_ne!(second, first, "the new item has a tessera_id of its own");
}
