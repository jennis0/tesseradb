//! The entity-space coalesce, end to end (decision 0044's D2).
//!
//! What is asserted here is the pair of claims the pass exists for and the pair that makes it safe:
//! the tier, run and dictionary-extent counts come **down** while every item stays visible and
//! every external id still resolves to the same entity; and geometry does not move — no
//! `segments_version` bump, so no projection is invalidated and no session pays anything.
//!
//! The selection rules themselves are unit-tested beside the code (`crates/tessera-engine/src/coalesce.rs`); what needs a
//! whole engine is that a published coalesce is *live* — the generation's tier list and the
//! process's external-id sidecar both swapped, rather than a manifest edit the running process
//! keeps ignoring until its next restart.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::UnallocatedRow;
use tessera_types::EntityId;

const WAIT: Duration = Duration::from_secs(30);

/// The coalesce policy's width. Every axis needs this many entries before anything is selected.
const WIDTH: usize = 8;

fn engine_at(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            // Long, so every flush in this test is one an operator asked for: the coalesce is
            // selected on the tick either way, and a background tick landing mid-assertion would
            // make the counts depend on wall-clock.
            flush_max_age_secs: 3600,
            // **The row trigger off.** This cell drives publication itself — it pins `B`
            // by flushing and waiting, so a trigger that published on its own would
            // measure a different buffer depth than the one the sweep set.
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    // The row-space merge is held off so the geometry version moves only if a coalesce moves it.
    engine.set_merge_for_test(false);
    engine
}

/// One ingest carrying a **novel** descriptor, so the flush that takes it promotes and publishes a
/// dictionary extent — which is what puts the third axis in play.
fn ingest_novel(engine: &Engine, i: usize) -> EntityId {
    let descriptor = format!("novel-{i}").into_bytes();
    let row = UnallocatedRow {
        external_id: Some(format!("ext-{i}").into_bytes()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![descriptor.clone()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[descriptor]),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], format!("batch-{i}"), [i as u8; 32])
        .expect("ingest is accepted")[0]
}

fn manifest_of(root: &std::path::Path) -> tessera_store::manifest::SegmentsManifest {
    let bundle = tessera_store::open_bundle(root).expect("the bundle opens");
    bundle.partitions.values().next().unwrap().manifest.clone()
}

/// **The pass bounds all three axes, and moves no geometry doing it.**
///
/// Without it the three counts grow by one per tick for the life of the deployment, and each is a
/// term in a steady-state cost: a fragment build probes every tier, the ingest duplicate check
/// scans every run, and `Engine::open` reads every dictionary extent.
///
/// **Mutation:** bump `segments_version` in `publish_coalesce` and the geometry assertion fails —
/// which is the whole of decision 0044's D2, since that bump is what would cost every live session
/// a projection rebuild for a pass that moved no row.
#[test]
fn a_coalesce_bounds_the_three_entity_space_axes_without_moving_geometry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);

    let mut ingested: Vec<(EntityId, Vec<u8>)> = Vec::new();
    for i in 0..WIDTH {
        let entity = ingest_novel(&engine, i);
        ingested.push((entity, format!("ext-{i}").into_bytes()));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    let before = manifest_of(&root);
    assert_eq!(before.deltas.len(), WIDTH, "one tier per flush");
    assert_eq!(
        before.locator_extents.len(),
        WIDTH,
        "one locator extent per flush"
    );
    let geometry_before = engine.generation().segments_version;

    // The coalesce is selected on the tick, and the tick is what a requested flush drives.
    engine.request_flush();
    wait_until("the coalesce to publish", WAIT, || {
        engine.write_executor_stats().coalesces >= 1
    });

    let after = manifest_of(&root);
    assert_eq!(
        after.deltas.len(),
        1,
        "{WIDTH} tiers became one: {:?}",
        after.deltas
    );
    assert_eq!(after.locator_extents.len(), 1);
    assert_eq!(
        after.external_id_runs.len(),
        2,
        "the build's run plus the coalesced one: {:?}",
        after.external_id_runs
    );
    assert_eq!(
        after.external_id_runs[0], before.external_id_runs[0],
        "the build's run stays listed first — the base locator's ordinals resolve inside it"
    );
    assert_eq!(
        after.dict_extents.len(),
        2,
        "the build's dictionary extent plus the coalesced one: {:?}",
        after.dict_extents
    );
    assert_eq!(
        after.dict_extents[0].path, before.dict_extents[0].path,
        "the build's dictionary extent keeps ordinal 0"
    );

    // **Geometry did not move.** This is decision 0044's D2 in one assertion: nothing a coalesce
    // rewrites addresses a row, so no projection is stale and no cache key may rotate.
    assert_eq!(
        engine.generation().segments_version,
        geometry_before,
        "an entity-space coalesce must not bump segments_version"
    );
    assert_eq!(
        engine.generation().delta_postings.len(),
        1,
        "the live tier list is swapped too, or the bound is only realised at the next restart"
    );

    // **Every binding still resolves, live** — through the swapped sidecar, not the old one.
    for (entity, external_id) in &ingested {
        assert_eq!(
            engine.resolve_external_id(external_id).expect("resolvable"),
            Some(*entity),
            "external id {} lost its binding to the coalesce",
            String::from_utf8_lossy(external_id)
        );
    }
}

/// A merge publishes over segments whose external-id runs and locator extents a coalesce has
/// already taken, and every binding still answers, live and after a restart.
#[test]
fn a_merge_publishes_over_segments_whose_runs_a_coalesce_took() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let ingested: Vec<(EntityId, Vec<u8>)> = {
        let engine = engine_at(tmp.path(), &root);
        let mut ingested = Vec::new();
        for i in 0..WIDTH {
            let entity = ingest_novel(&engine, i);
            ingested.push((entity, format!("ext-{i}").into_bytes()));
            flush(&engine);
        }
        engine.request_flush();
        wait_until("the coalesce to publish", WAIT, || {
            engine.write_executor_stats().coalesces >= 1
        });
        let coalesced = manifest_of(&root);
        assert_eq!(coalesced.locator_extents.len(), 1);
        let segments = coalesced.segments.len();

        engine.set_merge_for_test(true);
        wait_until("a merge to publish", WAIT, || {
            engine.request_flush();
            engine.write_executor_stats().merges >= 1
        });
        let merged = manifest_of(&root);
        assert!(merged.segments.len() < segments);
        assert_eq!(merged.external_id_runs, coalesced.external_id_runs);
        assert_eq!(merged.locator_extents.len(), 1);
        for (entity, external_id) in &ingested {
            assert_eq!(engine.resolve_external_id(external_id).unwrap(), Some(*entity));
            assert_eq!(
                engine.external_id_of(*entity).unwrap().as_deref(),
                Some(external_id.as_slice())
            );
        }
        ingested
    };

    let reopened = engine_at(tmp.path(), &root);
    for (entity, external_id) in &ingested {
        assert_eq!(reopened.resolve_external_id(external_id).unwrap(), Some(*entity));
        assert_eq!(
            reopened.external_id_of(*entity).unwrap().as_deref(),
            Some(external_id.as_slice())
        );
    }
}

/// **A restart opens what the coalesce committed**, which is the other half of the claim: the
/// manifest edit and the live swap must describe the same bundle, or a process that had coalesced
/// would come back holding a different set of tiers than the one it was serving from.
#[test]
fn a_coalesced_manifest_reopens_with_every_item_and_binding_intact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );

    let ingested: Vec<(EntityId, Vec<u8>)> = {
        let engine = engine_at(tmp.path(), &root);
        let mut ingested = Vec::new();
        for i in 0..WIDTH {
            let entity = ingest_novel(&engine, i);
            ingested.push((entity, format!("ext-{i}").into_bytes()));
            let flushes = engine.write_executor_stats().flushes;
            engine.request_flush();
            wait_until("the flush to publish", WAIT, || {
                engine.write_executor_stats().flushes > flushes
            });
        }
        engine.request_flush();
        wait_until("the coalesce to publish", WAIT, || {
            engine.write_executor_stats().coalesces >= 1
        });
        ingested
    };

    let reopened = engine_at(tmp.path(), &root);
    let generation = reopened.generation();
    assert_eq!(
        generation.delta_postings.len(),
        1,
        "the reopened bundle holds exactly the tiers its manifest names"
    );
    for (entity, external_id) in &ingested {
        assert_eq!(
            reopened
                .resolve_external_id(external_id)
                .expect("resolvable"),
            Some(*entity),
            "a binding did not survive the restart"
        );
        assert!(
            generation.bundle.partitions["default"].views["s0"]
                .row_space
                .row_of(*entity)
                .is_some(),
            "every flushed entity still has its row — a coalesce moves no row space"
        );
    }
}

/// **A configured `coalesce_width` reaches selection and changes when the pass fires.**
///
/// Until 2026-08-15 the width was a constant (8) with no configuration key at all, so nothing an
/// operator wrote could move it (the correctness suite needs to — correctness-suite §12.3). The
/// fixture is two flushed tiers — a quarter of the built-in width, which can never select a
/// coalesce over them — so the only way this pass can fire is the configured 2 arriving at the
/// policy, and against a regression to the constant this test fails by timeout rather than
/// passing vacuously.
#[test]
fn a_configured_coalesce_width_reaches_selection_and_changes_when_the_pass_fires() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            coalesce_width: Some(2),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");

    let mut ingested: Vec<(EntityId, Vec<u8>)> = Vec::new();
    for i in 0..2 {
        let entity = ingest_novel(&engine, i);
        ingested.push((entity, format!("ext-{i}").into_bytes()));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }
    assert_eq!(manifest_of(&root).deltas.len(), 2, "one tier per flush");

    engine.request_flush();
    wait_until("the width-2 coalesce to publish", WAIT, || {
        engine.write_executor_stats().coalesces >= 1
    });

    assert_eq!(
        manifest_of(&root).deltas.len(),
        1,
        "two tiers became one at the configured width"
    );
    for (entity, external_id) in &ingested {
        assert_eq!(
            engine.resolve_external_id(external_id).expect("resolvable"),
            Some(*entity),
            "external id {} lost its binding to the coalesce",
            String::from_utf8_lossy(external_id)
        );
    }
}

/// **The seventh axis: the entity→term transpose's extents come down to one, and every entity
/// answers what it answered.**
///
/// Without this pass the transpose accumulates one extent per flush until the next fold, and both
/// its readers — the drill-down's `labels` array and the join rule's label arm — pay a file handle
/// and a linear probe per lookup for every tick since the last fold.
///
/// **Mutation:** drop the merge's ascending walk (emit each layer's entities in layer order) and
/// the writer refuses; drop the re-derivation in `publish_coalesce` and the *live* layer count
/// assertion fails while the manifest one still passes, which is the whole difference between
/// bounding the reader and bounding a restart.
#[test]
fn a_coalesce_merges_the_entity_term_extents_and_every_entity_answers_the_same() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);

    let mut ingested: Vec<EntityId> = Vec::new();
    for i in 0..WIDTH {
        ingested.push(ingest_novel(&engine, i));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    let before = manifest_of(&root);
    assert_eq!(
        before.entity_terms_extents.len(),
        WIDTH,
        "one transpose extent per flush"
    );
    // The build's base plus one layer per flush — what the reader probes before the pass.
    assert_eq!(
        engine.generation().filter_columns.entity_terms().layers(),
        WIDTH + 1
    );
    let labels_before: Vec<Option<Vec<_>>> = ingested
        .iter()
        .map(|entity| engine.flushed_terms(*entity))
        .collect();
    assert!(
        labels_before
            .iter()
            .all(|l| l.as_ref().is_some_and(|t| !t.is_empty())),
        "each ingest carried a novel descriptor, so each entity has a label: {labels_before:?}"
    );
    // And the same through the drill-down, which is what the transpose exists to answer: the
    // served `labels` array, intersected with a session holding every novel descriptor.
    let credential = format!(
        r#"{{"terms": [{}]}}"#,
        (0..WIDTH)
            .map(|i| format!("\"novel-{i}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let drill_down = |engine: &Engine| -> Vec<Vec<String>> {
        let session = engine
            .authorise(credential.as_bytes())
            .expect("the session authorises");
        ingested
            .iter()
            .map(|entity| {
                let id = engine.tessera_id_of(*entity).expect("an opaque id");
                engine
                    .item(&session, id, None)
                    .expect("the drill-down answers")
                    .expect("the entity is visible to a session holding its descriptor")
                    .labels
            })
            .collect()
    };
    let served_before = drill_down(&engine);
    assert!(
        served_before.iter().all(|labels| !labels.is_empty()),
        "each entity's own novel descriptor is satisfied, so each drill-down names it"
    );

    engine.request_flush();
    wait_until("the coalesce to publish", WAIT, || {
        engine.write_executor_stats().coalesces >= 1
    });

    let after = manifest_of(&root);
    assert_eq!(
        after.entity_terms_extents.len(),
        1,
        "{WIDTH} transpose extents became one: {:?}",
        after.entity_terms_extents
    );
    assert_eq!(
        engine.generation().filter_columns.entity_terms().layers(),
        2,
        "the live stack is the base plus the coalesced layer, or the bound is only realised at \
         the next restart"
    );
    for (entity, before) in ingested.iter().zip(&labels_before) {
        assert_eq!(
            &engine.flushed_terms(*entity),
            before,
            "an entity's label set changed across the coalesce"
        );
    }
    assert_eq!(
        drill_down(&engine),
        served_before,
        "the served labels changed across a pass that merges what is there, verbatim"
    );

    // And a restart opens what was committed, with the same answers again.
    drop(engine);
    let reopened = engine_at(tmp.path(), &root);
    assert_eq!(
        reopened.generation().filter_columns.entity_terms().layers(),
        2
    );
    for (entity, before) in ingested.iter().zip(&labels_before) {
        assert_eq!(&reopened.flushed_terms(*entity), before);
    }
}

/// **A coalesce retires nothing** (Rule S / Rule F, write-path §5.4). An entity awaiting a
/// deletion keeps its term list across the pass — the merge has no tombstone parameter and no
/// route to one — and it is the read gate that hides the item, not the artefact.
///
/// **Mutation:** filter the merge on the overlay's deleted set and this fails, which is the point:
/// retiring here would put a Rule F retirement on a route that is not the fold's fold.
#[test]
fn a_pending_deletion_keeps_its_terms_across_a_coalesce() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);

    let mut ingested: Vec<EntityId> = Vec::new();
    for i in 0..WIDTH {
        ingested.push(ingest_novel(&engine, i));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    // The first flushed entity is deleted, and the deletion is pending until a fold executes it.
    let doomed = ingested[0];
    let terms_before = engine.flushed_terms(doomed).expect("a flushed label set");
    engine
        .accept_change(doomed, tessera_lifecycle::wal::ChangeOp::Delete)
        .expect("the delete is accepted");

    engine.request_flush();
    wait_until("the coalesce to publish", WAIT, || {
        engine.write_executor_stats().coalesces >= 1
    });
    assert_eq!(manifest_of(&root).entity_terms_extents.len(), 1);

    assert_eq!(
        engine.flushed_terms(doomed),
        Some(terms_before),
        "a coalesce merges what is there, verbatim: the deletion retires at the fold that \
         executes it (Rule F), never here"
    );

    // The fold is what retires it — and pass 4c runs over the *merged* shape, one extent where it
    // used to find `WIDTH`.
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(Instant::now() < deadline, "the fold did not publish");
        std::thread::sleep(Duration::from_millis(10));
    }

    let folded = manifest_of(&root);
    assert!(
        folded.entity_terms_extents.is_empty(),
        "the fold rewrites the base and carries no extent forward: {:?}",
        folded.entity_terms_extents
    );
    assert_eq!(
        engine.flushed_terms(doomed),
        None,
        "the fold retires the deleted entity's list"
    );
    for entity in &ingested[1..] {
        assert!(
            engine.flushed_terms(*entity).is_some_and(|t| !t.is_empty()),
            "a surviving entity lost its labels at the fold after a coalesce"
        );
    }
}

/// One item at (5, 5) under `key`, carrying `descriptors`.
fn ingest_with(engine: &Engine, key: &[u8], descriptors: &[&[u8]], batch: &str) -> EntityId {
    let descriptors: Vec<Vec<u8>> = descriptors.iter().map(|d| d.to_vec()).collect();
    let row = UnallocatedRow {
        external_id: Some(key.to_vec()),
        view: "s0".to_string(),
        join: None,
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        descriptors,
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], batch.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

/// A served item's external id and labels, or `None` when it is not served.
type Served = Option<(Option<Vec<u8>>, Vec<String>)>;

/// What the engine tells a viewer and the admin plane about `bindings`: each key's entity, each
/// entity's key, and, under a credential for each fixture term, the visible count per tile and
/// each entity's drill-down.
struct Answers {
    entities: Vec<Option<EntityId>>,
    keys: Vec<Option<Vec<u8>>>,
    tiles: Vec<Vec<(u64, u64)>>,
    items: Vec<Vec<Served>>,
}

fn assert_same(got: &Answers, expected: &Answers, when: &str) {
    assert_eq!(got.entities, expected.entities, "a key's entity changed at the {when}");
    assert_eq!(got.keys, expected.keys, "an entity's key changed at the {when}");
    assert_eq!(got.tiles, expected.tiles, "a masked count changed at the {when}");
    assert_eq!(got.items, expected.items, "a drill-down changed at the {when}");
}

fn answers(engine: &Engine, bindings: &[(EntityId, Vec<u8>)]) -> Answers {
    let sessions: Vec<_> = [full_coverage_credential(), subset_credential()]
        .iter()
        .map(|credential| engine.authorise(credential).expect("the session authorises"))
        .collect();
    Answers {
        entities: bindings
            .iter()
            .map(|(_, key)| engine.resolve_external_id(key).expect("the sidecar reads"))
            .collect(),
        keys: bindings
            .iter()
            .map(|(entity, _)| engine.external_id_of(*entity).expect("the sidecar reads"))
            .collect(),
        tiles: sessions
            .iter()
            .map(|session| {
                engine
                    .viewport(
                        session,
                        ViewportRequest::new("s0", 2, WHOLE_MAP, N_ITEMS as usize),
                    )
                    .expect("the viewport answers")
                    .tiles
                    .iter()
                    .map(|tile| (tile.tile, tile.visible))
                    .collect()
            })
            .collect(),
        items: sessions
            .iter()
            .map(|session| {
                bindings
                    .iter()
                    .map(|(entity, _)| {
                        let id = engine.tessera_id_of(*entity).expect("an opaque id");
                        engine
                            .item(session, id, None)
                            .expect("the drill-down answers")
                            .map(|item| (item.external_id, item.labels))
                    })
                    .collect()
            })
            .collect(),
    }
}

/// A fold carries forward the tiers, runs and locator extents that flushes published during its
/// flight, and their digests land in the new `MANIFEST.json`. A coalesce then takes them, and
/// every binding, count, drill-down and deny is what it was before, live and after a restart. A
/// later fold still retires what was deleted, and each retired key can be ingested again.
#[test]
fn a_folds_carried_tiers_and_runs_are_coalesced_and_every_answer_holds_through_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let base = source_to_new_map(&root, "v00000");
    let base_entity = |source: u64| EntityId::new(base[&source]);
    let mut bindings: Vec<(EntityId, Vec<u8>)> = base
        .iter()
        .map(|(source, entity)| (EntityId::new(*entity), source_id_key(*source)))
        .collect();

    let engine = engine_at(tmp.path(), &root);
    // Off until the answers after the fold are recorded, so the coalesce cannot run first.
    engine.set_coalesce_for_test(false);
    let baseline: u64 = answers(&engine, &bindings).tiles[0].iter().map(|t| t.1).sum();

    // Denied before the fold: this deletion is the fold's to retire.
    let deleted_before = base_entity(4);
    let suppressed_before = base_entity(8);
    engine
        .accept_change(deleted_before, ChangeOp::Delete)
        .expect("the delete is accepted");
    engine
        .accept_change(suppressed_before, ChangeOp::Suppress)
        .expect("the suppression is accepted");

    // Hold the fold after its passes, and publish WIDTH flushes while it waits.
    engine.set_fold_paused_for_test(true);
    let before_fold = engine.write_executor_stats();
    engine.request_fold();
    wait_until("the fold to reach its hold", WAIT, || {
        engine.fold_is_holding_for_test()
    });
    let mut carried: Vec<(EntityId, Vec<u8>)> = Vec::new();
    for i in 0..WIDTH {
        let key = format!("carried-{i}").into_bytes();
        let descriptors: &[&[u8]] = if i % 2 == 0 { &[b"0", b"1"] } else { &[b"0"] };
        let entity = ingest_with(&engine, &key, descriptors, &format!("carried-{i}"));
        carried.push((entity, key));
        flush(&engine);
    }

    // Denied during the flight, after the fold's snapshot: these stand after the fold.
    let deleted_during = carried[1].0;
    let suppressed_during = carried[2].0;
    let base_deleted_during = base_entity(9);
    for (entity, op) in [
        (deleted_during, ChangeOp::Delete),
        (suppressed_during, ChangeOp::Suppress),
        (base_deleted_during, ChangeOp::Delete),
    ] {
        engine
            .accept_change(entity, op)
            .expect("the deny is accepted during the fold");
    }

    engine.set_fold_paused_for_test(false);
    wait_until("the fold to publish", WAIT, || {
        let now = engine.write_executor_stats();
        assert_eq!(now.fold_failures, before_fold.fold_failures, "the fold was discarded");
        now.folds > before_fold.folds
    });

    // Every carried entry is digested in the new prefix's MANIFEST.json.
    let folded = manifest_of(&root);
    assert_eq!(folded.deltas.len(), WIDTH, "one carried tier per flush");
    assert_eq!(folded.locator_extents.len(), WIDTH, "one carried locator extent per flush");
    let digested = tessera_store::open_bundle(&root)
        .expect("the bundle opens")
        .manifest
        .files;
    assert!(folded.deltas.iter().all(|tier| digested.contains_key(tier)));
    assert!(folded
        .locator_extents
        .iter()
        .all(|extent| extent.files().all(|rel| digested.contains_key(rel))));

    bindings.extend(carried.iter().cloned());
    let expected = answers(&engine, &bindings);
    assert_eq!(
        expected.tiles[0].iter().map(|t| t.1).sum::<u64>(),
        baseline - 3 + WIDTH as u64 - 2,
        "three base items and two carried ones are denied"
    );
    for entity in [
        deleted_before,
        suppressed_before,
        base_deleted_during,
        deleted_during,
        suppressed_during,
    ] {
        let at = bindings.iter().position(|(e, _)| *e == entity).unwrap();
        assert_eq!(expected.items[0][at], None, "a denied item is not served");
    }

    engine.set_coalesce_for_test(true);
    engine.request_flush();
    wait_until("the coalesce to publish", WAIT, || {
        engine.write_executor_stats().coalesces >= 1
    });
    let coalesced = manifest_of(&root);
    assert_eq!(coalesced.deltas.len(), 1, "the carried tiers became one");
    assert_eq!(coalesced.locator_extents.len(), 1, "the carried locator extents became one");
    assert_eq!(
        coalesced.external_id_runs.len(),
        2,
        "the fold's base run, untouched, and the coalesced run"
    );
    assert_eq!(coalesced.external_id_runs[0], folded.external_id_runs[0]);
    assert_same(&answers(&engine, &bindings), &expected, "coalesce");

    drop(engine);
    let engine = engine_at(tmp.path(), &root);
    assert_same(&answers(&engine, &bindings), &expected, "restart");

    // A second fold retires the deletions the first could not, including one whose run the
    // coalesce merged. Every deleted key, the one the first fold retired among them, then resolves
    // to nothing and can be ingested again as a new entity.
    fold(&engine);
    let retired = [
        (deleted_before, source_id_key(4)),
        (base_deleted_during, source_id_key(9)),
        (deleted_during, carried[1].1.clone()),
    ];
    for (i, (entity, key)) in retired.iter().enumerate() {
        assert_eq!(
            engine.resolve_external_id(key).expect("the sidecar reads"),
            None,
            "a retired entity's key resolves to nothing"
        );
        let reborn = ingest_with(&engine, key, &[b"0"], &format!("reborn-{i}"));
        assert_ne!(reborn, *entity, "a re-ingest takes a fresh entity");
        assert_eq!(
            engine.resolve_external_id(key).expect("the sidecar reads"),
            Some(reborn)
        );
    }
}
