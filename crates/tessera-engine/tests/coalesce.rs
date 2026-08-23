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
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::UnallocatedRow;
use tessera_types::EntityId;

/// The coalesce policy's width. Every axis needs this many entries before anything is selected.
const WIDTH: usize = 8;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

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
    // **The row-space merge is held off**, so these assertions are about the entity-space pass
    // alone. A merge coalesces its consumed segments' runs and locator extents too, on the same
    // tick, and the two would race for the same entries — safely, each discarding a plan that no
    // longer rebases, but not deterministically enough to assert list lengths against.
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
        descriptors: vec![descriptor.clone()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[descriptor]),
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
        wait_until("the flush to publish", || {
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
    wait_until("the coalesce to publish", || {
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
            wait_until("the flush to publish", || {
                engine.write_executor_stats().flushes > flushes
            });
        }
        engine.request_flush();
        wait_until("the coalesce to publish", || {
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
            coalesce_width: Some(2),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    // Held off for engine_at's reason: a merge coalesces runs and locator extents of its own.
    engine.set_merge_for_test(false);

    let mut ingested: Vec<(EntityId, Vec<u8>)> = Vec::new();
    for i in 0..2 {
        let entity = ingest_novel(&engine, i);
        ingested.push((entity, format!("ext-{i}").into_bytes()));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    }
    assert_eq!(manifest_of(&root).deltas.len(), 2, "one tier per flush");

    engine.request_flush();
    wait_until("the width-2 coalesce to publish", || {
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
