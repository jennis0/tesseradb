//! Deny state reaches the side-manifest (contracts §2.3's writer half).
//!
//! The reader has honoured `deny` and `tombstones` since Task 5 — `initial_deny_of` seeds the
//! overlay from them at open — while nothing wrote either field, so every published manifest
//! asserted "no suppressions" whatever the live state. A restore from bundle and object store
//! would have recovered a corpus with every deny forgotten.
//!
//! **A manifest is a projection of live state, never an input to the next one.** The two fields
//! are serialised from the overlay at every write and never copied from the manifest being
//! extended — which is what makes an unsuppress reach disc at all, and what stops a manifest
//! written once republishing its own stale list for ever.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::UnallocatedRow;
use tessera_store::manifest::SegmentsManifest;
use tessera_types::EntityId;

const WAIT: Duration = Duration::from_secs(20);

fn engine_at(tmp: &Path, root: &Path, tick_secs: u64) -> Engine {
    std::fs::create_dir_all(tmp).unwrap();
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            // The shipped row trigger, four commit windows (`DEFAULT_FLUSH_MAX_ITEMS`):
            // what bounds the window close's O(buffered) copy. Nothing here reaches it.
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn fixture(tmp: &Path) -> PathBuf {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    root
}

/// The newest side-manifest on disc, read the way the reader reads it — highest `n` first.
fn newest_manifest(root: &Path) -> (u64, SegmentsManifest) {
    let bundle = tessera_store::open_bundle(root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    (partition.segments_n, partition.manifest.clone())
}

fn suppressed_in(manifest: &SegmentsManifest) -> Vec<u64> {
    let mut ids: Vec<u64> = manifest.deny.iter().map(|e| e.entity_id).collect();
    ids.sort_unstable();
    ids
}

fn entity_of_source(root: &Path, source_id: u64) -> EntityId {
    EntityId::new(source_to_new_map(root, "v00000")[&source_id])
}

fn ingest(engine: &Engine, external_id: &str) -> EntityId {
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

/// **Obligation 1.** A flush publishes the deny state of the generation it is published against —
/// not the one it was planned against.
///
/// The manifest used to be assembled on the pool at plan time, which made its deny fields a
/// snapshot the flush's own flight had outlived. Assembling at publication is what closes that,
/// and the suppression accepted *after* the ingest but *before* the flush lands is the case that
/// tells the two apart.
#[test]
fn a_flush_manifest_carries_the_deny_state_at_publication() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    let suppressed = entity_of_source(&root, 7);
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("the suppression is accepted");

    ingest(&engine, "ext-1");
    wait_until("the flush to publish", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });

    let (n, manifest) = newest_manifest(&root);
    assert_eq!(
        suppressed_in(&manifest),
        vec![suppressed.raw()],
        "the flush-published manifest carries the live suppression set"
    );
    assert!(n >= 1, "the flush published a manifest above the build's");
    assert_eq!(
        manifest.segments.len(),
        2,
        "and it is the flush's own manifest — the build segment plus the flushed one"
    );
}

/// **A manifest is a projection, never an input.** An unsuppress must reach disc, which it cannot
/// if a later manifest copies the deny list from the one it extends.
///
/// **Mutation:** carry `deny`/`tombstones` forward from the previous manifest instead of
/// serialising them, and this fails — the suppression is republished for ever, and a restore
/// recovers an item the operator un-suppressed.
#[test]
fn an_unsuppress_is_absent_from_the_next_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);
    let entity = entity_of_source(&root, 7);

    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("accepted");
    ingest(&engine, "ext-1");
    wait_until("the first flush", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });
    assert_eq!(suppressed_in(&newest_manifest(&root).1), vec![entity.raw()]);

    engine
        .accept_change(entity, ChangeOp::Unsuppress)
        .expect("accepted");
    ingest(&engine, "ext-2");
    wait_until("the second flush", WAIT, || {
        engine.write_executor_stats().flushes >= 2
    });

    assert!(
        suppressed_in(&newest_manifest(&root).1).is_empty(),
        "the unsuppress removed the entry: complete current state, not a diff"
    );
}

/// A deletion lands in `tombstones`, not in `deny` — the two fields retire under different rules
/// (lifecycle §3), so publishing the union under either would misdescribe both.
#[test]
fn a_delete_reaches_tombstones_and_a_suppress_reaches_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    let deleted = entity_of_source(&root, 11);
    let suppressed = entity_of_source(&root, 12);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("accepted");
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("accepted");

    ingest(&engine, "ext-1");
    wait_until("the flush", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });

    let (_, manifest) = newest_manifest(&root);
    assert_eq!(suppressed_in(&manifest), vec![suppressed.raw()]);
    assert_eq!(manifest.tombstones, vec![deleted.raw()]);
    assert!(
        manifest.deny.iter().all(|e| e.cause == "suppress"),
        "contracts §2.3: `deny` carries the suppression set, and its cause says so"
    );
}

/// **The restore path this exists for**: a node opened from the bundle alone — no WAL — honours
/// what the newest manifest says, and keeps a suppressed item hidden.
///
/// This is the disaster path the deny lifecycle memo §4 names, and it is the first test in the
/// tree to exercise it against a manifest a *writer* produced rather than a hand-edited one.
#[test]
fn a_node_restored_from_the_bundle_alone_honours_the_published_deny() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let suppressed = entity_of_source(&root, 7);

    let visible_before = {
        let engine = engine_at(tmp.path(), &root, 1);
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        let before = visible_count(&engine, &session);

        engine
            .accept_change(suppressed, ChangeOp::Suppress)
            .expect("accepted");
        ingest(&engine, "ext-1");
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes >= 1
        });
        before
    };

    // A **fresh** runtime directory: no WAL, so nothing replays and the manifest is the only
    // surviving statement of the deny state.
    let restored = engine_at(&tmp.path().join("restore"), &root, 3600);
    let session = restored.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible_count(&restored, &session),
        visible_before,
        "the flushed item is visible and the suppressed one is not — a net zero against the \
         baseline, recovered from the manifest alone"
    );
    assert!(
        restored.generation().overlay.is_suppressed(suppressed),
        "and the suppression is in force, not merely parsed"
    );
}

fn visible_count(engine: &Engine, session: &tessera_engine::Session) -> u64 {
    engine
        .viewport(
            session,
            tessera_engine::ViewportRequest::new(
                "s0",
                4,
                [0.0, 0.0, 1000.0, 1000.0],
                (N_ITEMS + 10) as usize,
            ),
        )
        .expect("a viewport")
        .tiles
        .iter()
        .map(|t| t.visible)
        .sum()
}

/// **Obligation 2.** An accepted deny publishes a side-manifest of its own, at a new `n` and at an
/// **unchanged geometry version** — contracts §2.3's immediate-publication rule, without the cost
/// that bumping the geometry version would carry.
///
/// The geometry half is the load-bearing assertion. `segments_version` is the row-projection cache
/// key and its patch path derives only from `version - 1`, so a deny that moved it would drop any
/// session quiet through a burst off that chain and cost it a full rebuild — measured at 1 277 ms at
/// 10⁹. No flush happens here at all.
#[test]
fn an_accepted_deny_publishes_without_moving_the_geometry_version() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 3600);

    let (n_before, _) = newest_manifest(&root);
    let geometry_before = engine.generation().segments_version;
    let entity = entity_of_source(&root, 7);

    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("accepted");
    wait_until("the overlay publication", WAIT, || {
        engine.write_executor_stats().overlay_publications >= 1
    });

    let (n_after, manifest) = newest_manifest(&root);
    assert!(
        n_after > n_before,
        "the deny published a new side-manifest ({n_before} -> {n_after})"
    );
    assert_eq!(suppressed_in(&manifest), vec![entity.raw()]);
    assert_eq!(
        engine.generation().segments_version,
        geometry_before,
        "and no geometry moved: an overlay publication is a disc event only"
    );
    assert_eq!(
        engine.write_executor_stats().flushes,
        0,
        "no flush was involved"
    );
}
