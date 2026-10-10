//! The entity-space coalesce, end to end (decision 0044's D2).
//!
//! What is asserted here is the pair of claims the pass exists for and the pair that makes it safe:
//! the tier, key-run and dictionary-extent counts come **down** while every item stays visible and
//! every unique value still names the same entity; and geometry does not move — no
//! `segments_version` bump, so no projection is invalidated and no session pays anything.
//!
//! The selection rules themselves are unit-tested beside the code; what needs a whole engine is
//! that a published coalesce is *live* — the generation's tier list and unique indexes both
//! swapped, rather than a manifest edit the running process keeps ignoring until its next restart.

mod common;

use std::time::{Duration, Instant};

use common::*;
use mosaica_engine::{Engine, EngineConfig, ViewportRequest};
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_lifecycle::UnallocatedRow;
use mosaica_types::EntityId;

const WAIT: Duration = Duration::from_secs(30);

/// The coalesce policy's width. Every axis but the runs needs this many entries before anything
/// is selected.
const WIDTH: usize = 8;

/// The width of the unique key-run axis.
const RUN_WIDTH: usize = 4;

/// The live key runs of the fixture's unique `id`.
fn live_runs(manifest: &mosaica_store::manifest::SegmentsManifest) -> &[String] {
    &manifest
        .unique_indexes
        .iter()
        .find(|i| i.attribute == "id")
        .expect("`id` is unique")
        .live
}

/// Drive ticks until the tiers are one and the key runs are fewer than a run window, which is
/// where every axis a coalesce takes has come to rest.
fn settle_coalesce(engine: &Engine) {
    tick_until(engine, "the coalesce to settle", WAIT, || {
        let generation = engine.generation();
        let manifest = &generation.bundle.partitions["default"].manifest;
        generation.delta_postings.len() == 1 && live_runs(manifest).len() < RUN_WIDTH
    });
}

/// The key runs after a coalesce: the base runs unchanged, and fewer live runs than a run window.
fn assert_runs_coalesced(
    after: &mosaica_store::manifest::SegmentsManifest,
    before: &mosaica_store::manifest::SegmentsManifest,
) {
    let base = |m: &mosaica_store::manifest::SegmentsManifest| {
        let index = m
            .unique_indexes
            .iter()
            .find(|i| i.attribute == "id")
            .unwrap();
        index
            .base
            .iter()
            .map(|run| run.path.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(base(after), base(before));
    assert!(live_runs(after).len() < RUN_WIDTH);
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
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
            compaction: mosaica_engine::CompactionSchedule::off(),
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
/// dictionary extent — which is what puts the third axis in play. It holds the `id` of `ext-{i}`.
fn ingest_novel(engine: &Engine, i: usize) -> EntityId {
    let descriptor = format!("novel-{i}").into_bytes();
    let row = UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![descriptor.clone()],
        x: 5.0,
        y: 5.0,
        scalars: keyed(&format!("ext-{i}")),
        terms: engine.resolve_terms(&[descriptor]),
        scoped: Vec::new(),
    };
    engine
        .ingest_rows(vec![row], format!("batch-{i}"), [i as u8; 32])
        .expect("ingest is accepted")[0]
}

fn manifest_of(root: &std::path::Path) -> mosaica_store::manifest::SegmentsManifest {
    let bundle = mosaica_store::open_bundle(root).expect("the bundle opens");
    bundle.partitions.values().next().unwrap().manifest.clone()
}

/// **The pass bounds all three axes, and moves no geometry doing it.**
///
/// Without it the three counts grow by one per tick for the life of the deployment, and each is a
/// term in a steady-state cost: a fragment build probes every tier, a unique lookup reads every
/// live run, and `Engine::open` reads every dictionary extent.
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

    let mut ingested: Vec<(EntityId, String)> = Vec::new();
    for i in 0..WIDTH {
        let entity = ingest_novel(&engine, i);
        ingested.push((entity, format!("ext-{i}")));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    let before = manifest_of(&root);
    assert_eq!(before.deltas.len(), WIDTH, "one tier per flush");
    let geometry_before = engine.generation().segments_version;

    settle_coalesce(&engine);

    let after = manifest_of(&root);
    assert_eq!(
        after.deltas.len(),
        1,
        "{WIDTH} tiers became one: {:?}",
        after.deltas
    );
    assert_runs_coalesced(&after, &before);
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

    // **Every value still names its item, live** — through the swapped index, not the old one.
    for (entity, key) in &ingested {
        assert_eq!(
            item_of_key(&engine, key),
            Some(*entity),
            "{key} lost its item to the coalesce"
        );
    }
}

/// A merge publishes over segments whose key runs a coalesce has already taken, and every value
/// still names its item, live and after a restart.
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
    let ingested: Vec<(EntityId, String)> = {
        let engine = engine_at(tmp.path(), &root);
        let mut ingested = Vec::new();
        for i in 0..WIDTH {
            let entity = ingest_novel(&engine, i);
            ingested.push((entity, format!("ext-{i}")));
            publish_buffered(&engine);
        }
        settle_coalesce(&engine);
        let coalesced = manifest_of(&root);
        let segments = coalesced.segments.len();

        engine.set_merge_for_test(true);
        wait_until("a merge to publish", WAIT, || {
            engine.request_flush();
            engine.write_executor_stats().merges >= 1
        });
        let merged = manifest_of(&root);
        assert!(merged.segments.len() < segments);
        assert_eq!(merged.unique_indexes, coalesced.unique_indexes);
        for (entity, key) in &ingested {
            assert_eq!(item_of_key(&engine, key), Some(*entity));
        }
        ingested
    };

    let reopened = engine_at(tmp.path(), &root);
    for (entity, key) in &ingested {
        assert_eq!(item_of_key(&reopened, key), Some(*entity));
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

    let ingested: Vec<(EntityId, String)> = {
        let engine = engine_at(tmp.path(), &root);
        let mut ingested = Vec::new();
        for i in 0..WIDTH {
            let entity = ingest_novel(&engine, i);
            ingested.push((entity, format!("ext-{i}")));
            let flushes = engine.write_executor_stats().flushes;
            engine.request_flush();
            wait_until("the flush to publish", WAIT, || {
                engine.write_executor_stats().flushes > flushes
            });
        }
        settle_coalesce(&engine);
        ingested
    };

    let reopened = engine_at(tmp.path(), &root);
    let generation = reopened.generation();
    assert_eq!(
        generation.delta_postings.len(),
        1,
        "the reopened bundle holds exactly the tiers its manifest names"
    );
    for (entity, key) in &ingested {
        assert_eq!(
            item_of_key(&reopened, key),
            Some(*entity),
            "a value did not survive the restart"
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

    let mut ingested: Vec<(EntityId, String)> = Vec::new();
    for i in 0..2 {
        let entity = ingest_novel(&engine, i);
        ingested.push((entity, format!("ext-{i}")));
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
    for (entity, key) in &ingested {
        assert_eq!(
            item_of_key(&engine, key),
            Some(*entity),
            "{key} lost its item to the coalesce"
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
                let id = engine.mosaica_id_of(*entity).expect("an opaque id");
                engine
                    .item(&session, id)
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

    settle_coalesce(&engine);

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
        .accept_change(doomed, mosaica_lifecycle::wal::ChangeOp::Delete)
        .expect("the delete is accepted");

    settle_coalesce(&engine);
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

/// One item at (5, 5) holding `id`, carrying `descriptors`.
fn ingest_with(engine: &Engine, id: u64, descriptors: &[&[u8]], batch: &str) -> EntityId {
    let descriptors: Vec<Vec<u8>> = descriptors.iter().map(|d| d.to_vec()).collect();
    let row = UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        x: 5.0,
        y: 5.0,
        scalars: vec![WalScalar::U64(id)],
        terms: engine.resolve_terms(&descriptors),
        descriptors,
        scoped: Vec::new(),
    };
    engine
        .ingest_rows(vec![row], batch.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

/// A served item's labels, or `None` when it is not served.
type Served = Option<Vec<String>>;

/// What the engine tells a viewer and the admin plane about `bindings`: the item each `id` names,
/// and, under a credential for each fixture term, the visible count per tile and each entity's
/// drill-down.
struct Answers {
    entities: Vec<Option<EntityId>>,
    tiles: Vec<Vec<(u64, u64)>>,
    items: Vec<Vec<Served>>,
}

fn assert_same(got: &Answers, expected: &Answers, when: &str) {
    assert_eq!(
        got.entities, expected.entities,
        "an id's item changed at the {when}"
    );
    assert_eq!(
        got.tiles, expected.tiles,
        "a masked count changed at the {when}"
    );
    assert_eq!(
        got.items, expected.items,
        "a drill-down changed at the {when}"
    );
}

fn answers(engine: &Engine, bindings: &[(EntityId, u64)]) -> Answers {
    let sessions: Vec<_> = [full_coverage_credential(), subset_credential()]
        .iter()
        .map(|credential| {
            engine
                .authorise(credential)
                .expect("the session authorises")
        })
        .collect();
    Answers {
        entities: bindings
            .iter()
            .map(|(_, id)| item_of_id(engine, *id).expect("the index reads"))
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
                        let id = engine.mosaica_id_of(*entity).expect("an opaque id");
                        engine
                            .item(session, id)
                            .expect("the drill-down answers")
                            .map(|item| item.labels)
                    })
                    .collect()
            })
            .collect(),
    }
}

/// A fold carries forward the tiers and key runs that flushes published during its flight, and
/// their digests land in the new `MANIFEST.json`. A coalesce then takes them, and every value,
/// count, drill-down and deny is what it was before, live and after a restart. A later fold still
/// retires what was deleted, and each retired value can be ingested again.
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
    let mut bindings: Vec<(EntityId, u64)> = base
        .iter()
        .map(|(source, entity)| (EntityId::new(*entity), *source))
        .collect();

    let engine = engine_at(tmp.path(), &root);
    // Off until the answers after the fold are recorded, so the coalesce cannot run first.
    engine.set_coalesce_for_test(false);
    let baseline: u64 = answers(&engine, &bindings).tiles[0]
        .iter()
        .map(|t| t.1)
        .sum();

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
    let mut carried: Vec<(EntityId, u64)> = Vec::new();
    for i in 0..WIDTH {
        let id = key_id(&format!("carried-{i}"));
        let descriptors: &[&[u8]] = if i % 2 == 0 { &[b"0", b"1"] } else { &[b"0"] };
        let entity = ingest_with(&engine, id, descriptors, &format!("carried-{i}"));
        carried.push((entity, id));
        publish_buffered(&engine);
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
        assert_eq!(
            now.fold_failures, before_fold.fold_failures,
            "the fold was discarded"
        );
        now.folds > before_fold.folds
    });

    // Every carried entry is digested in the new prefix's MANIFEST.json.
    let folded = manifest_of(&root);
    assert_eq!(folded.deltas.len(), WIDTH, "one carried tier per flush");
    assert_eq!(
        live_runs(&folded).len(),
        WIDTH,
        "one carried key run per flush"
    );
    let digested = mosaica_store::open_bundle(&root)
        .expect("the bundle opens")
        .manifest
        .files;
    assert!(folded.deltas.iter().all(|tier| digested.contains_key(tier)));
    assert!(live_runs(&folded)
        .iter()
        .all(|run| digested.contains_key(run)));

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
    settle_coalesce(&engine);
    let coalesced = manifest_of(&root);
    assert_eq!(coalesced.deltas.len(), 1, "the carried tiers became one");
    assert_runs_coalesced(&coalesced, &folded);
    assert_same(&answers(&engine, &bindings), &expected, "coalesce");

    drop(engine);
    let engine = engine_at(tmp.path(), &root);
    assert_same(&answers(&engine, &bindings), &expected, "restart");

    // A second fold retires the deletions the first could not, including one whose run the
    // coalesce merged. Every deleted value, the one the first fold retired among them, then names
    // nothing and can be ingested again as a new item: one that takes a retired entity's number
    // holds it at a higher tenancy.
    fold(&engine);
    let retired = [
        (deleted_before, 4),
        (base_deleted_during, 9),
        (deleted_during, carried[1].1),
    ];
    for (i, (entity, id)) in retired.iter().enumerate() {
        assert_eq!(
            item_of_id(&engine, *id).expect("the index reads"),
            None,
            "a retired entity's value names nothing"
        );
        let reborn = ingest_with(&engine, *id, &[b"0"], &format!("reborn-{i}"));
        let (high, _) = test_key()
            .invert(engine.mosaica_id_of(reborn).unwrap())
            .unwrap();
        assert!(
            reborn != *entity || high.tenancy.raw() > 0,
            "a re-ingest is a new item, under a mosaica_id the deleted one never had"
        );
        assert_eq!(
            item_of_id(&engine, *id).expect("the index reads"),
            Some(reborn)
        );
    }
}

fn declare(engine: &Engine, name: &str, ty: &str, index: bool) {
    engine
        .declare_attribute(mosaica_engine::AttributeRequest {
            name: name.to_string(),
            title: None,
            ty: ty.to_string(),
            vocabulary: None,
            analyser: None,
            index,
            render: false,
            scope: mosaica_types::layer::LayerScope::Entity,
            unique: false,
        })
        .unwrap_or_else(|e| panic!("column '{name}' declares: {e}"));
}

/// Each item's drill-down fields, and the items each filter matches in each view.
#[derive(Debug, PartialEq)]
struct Interleaved {
    fields: Vec<Vec<(String, mosaica_engine::ScalarOut)>>,
    matches: Vec<Vec<u64>>,
}

fn interleaved_answers(engine: &Engine, entities: &[EntityId]) -> Interleaved {
    use mosaica_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
    let session = engine
        .authorise(&full_coverage_credential())
        .expect("the session authorises");
    let fields = entities
        .iter()
        .map(|entity| {
            let id = engine.mosaica_id_of(*entity).expect("an opaque id");
            let item = engine.item(&session, id).expect("the drill-down answers");
            let mut fields: Vec<_> = item
                .expect("the item is served")
                .fields
                .into_iter()
                .map(|f| (f.name, f.value))
                .collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            fields
        })
        .collect();
    let leaf = |column: &str, operand| FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    };
    let filters = [
        leaf("tag", FilterOperand::TextEquals("tag-1".to_string())),
        leaf(
            "weight",
            FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Float(6.0),
                    inclusive: true,
                }),
                hi: None,
            },
        ),
        leaf(
            "prose",
            FilterOperand::Match {
                query: "shared".to_string(),
                minimum: None,
            },
        ),
        leaf(
            "prose",
            FilterOperand::Match {
                query: "word5".to_string(),
                minimum: None,
            },
        ),
    ];
    let mut matches = Vec::new();
    for filter in filters {
        for view in ["s0", "s1"] {
            let mut request = ViewportRequest::new(view, 0, WHOLE_MAP, N_ITEMS as usize);
            request.filter = Some(filter.clone());
            let mut ids: Vec<u64> = engine
                .viewport(&session, request)
                .expect("the viewport answers")
                .points
                .iter()
                .map(|(id, _)| id.raw())
                .collect();
            ids.sort_unstable();
            matches.push(ids);
        }
    }
    Interleaved { fields, matches }
}

/// **Extents from two views whose entities interleave coalesce, and every entity answers the
/// same.** New items for two views in one commit window take interleaved entity ids, so the two
/// views' flushes write entity-scoped attribute, record, text and entity-term extents whose entity
/// sets interleave. The coalesce merges them by entity: every filter and drill-down answers as it
/// did before, live and after a restart.
#[test]
fn interleaved_extents_from_two_views_coalesce_and_every_entity_answers_the_same() {
    use mosaica_lifecycle::wal::WalScalar;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);
    engine.set_coalesce_for_test(false);
    declare(&engine, "note", "keyword", false);
    declare(&engine, "tag", "keyword", true);
    declare(&engine, "prose", "text", true);
    declare(&engine, "weight", "f32", true);
    engine
        .create_plain_view(mosaica_engine::PlainViewDeclaration {
            name: "s1".to_string(),
            title: None,
            projection: "none".to_string(),
            frame: mosaica_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the view is created");

    // Each window's batch alternates views, so each view's flush holds every other id. Two
    // flushes per window, so WIDTH / 2 windows give every axis a full window.
    let mut entities: Vec<EntityId> = Vec::new();
    let mut s0: Vec<u64> = Vec::new();
    let mut s1: Vec<u64> = Vec::new();
    for window in 0..WIDTH / 2 {
        let rows: Vec<UnallocatedRow> = (0..4)
            .map(|j| {
                let i = window * 4 + j;
                UnallocatedRow {
                    view: if j % 2 == 0 { "s0" } else { "s1" }.to_string(),
                    join: None,
                    descriptors: vec![b"0".to_vec()],
                    x: 5.0 + i as f64,
                    y: 5.0,
                    // The fixture's `id` first, which these items do not hold.
                    scalars: vec![
                        WalScalar::Null,
                        WalScalar::Utf8(format!("note-{i}")),
                        WalScalar::Utf8(format!("tag-{}", i % 3)),
                        WalScalar::Utf8(format!("word{i} shared")),
                        WalScalar::F32(i as f32),
                    ],
                    terms: engine.resolve_terms(&[b"0".to_vec()]),
                    scoped: Vec::new(),
                }
            })
            .collect();
        // A batch names one view, so each row is its own batch; sent in turn, their ids alternate
        // between the two views.
        let allocated: Vec<EntityId> = rows
            .into_iter()
            .enumerate()
            .map(|(j, row)| {
                engine
                    .ingest_rows(vec![row], format!("mixed-{window}-{j}"), [window as u8; 32])
                    .expect("the batch is accepted")[0]
            })
            .collect();
        for (j, entity) in allocated.iter().enumerate() {
            if j % 2 == 0 { &mut s0 } else { &mut s1 }.push(entity.raw());
        }
        entities.extend(allocated);
        publish_buffered(&engine);
    }
    assert!(
        s0.iter().min() < s1.iter().max() && s1.iter().min() < s0.iter().max(),
        "the two views' entities interleave: {s0:?} and {s1:?}"
    );

    let manifest = manifest_of(&root);
    let column_extents = |m: &mosaica_store::manifest::SegmentsManifest, column: &str| {
        m.attr_extents.iter().filter(|e| e.column == column).count()
            + m.text_extents.iter().filter(|e| e.column == column).count()
    };
    let lists = |m: &mosaica_store::manifest::SegmentsManifest| {
        [
            column_extents(m, "tag"),
            column_extents(m, "prose"),
            column_extents(m, "weight"),
            m.record_extents.len(),
            m.entity_terms_extents.len(),
        ]
    };
    assert_eq!(
        lists(&manifest),
        [WIDTH; 5],
        "one extent per flush on every list"
    );
    let before = interleaved_answers(&engine, &entities);
    for fields in &before.fields {
        for name in ["note", "tag", "prose", "weight"] {
            assert!(
                fields.iter().any(|f| f.0 == name),
                "'{name}' is served: {fields:?}"
            );
        }
    }

    engine.set_coalesce_for_test(true);
    let stats = engine.write_executor_stats();
    tick_until(&engine, "the coalesce to publish", WAIT, || {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.coalesce_failures, stats.coalesce_failures,
            "a coalesce failed"
        );
        now.coalesces > stats.coalesces
    });
    let after = manifest_of(&root);
    assert_eq!(lists(&after), [1; 5], "every list collapsed to one extent");
    assert_eq!(
        interleaved_answers(&engine, &entities),
        before,
        "live, after the coalesce"
    );

    drop(engine);
    let engine = engine_at(tmp.path(), &root);
    assert_eq!(
        interleaved_answers(&engine, &entities),
        before,
        "after a restart"
    );
}
