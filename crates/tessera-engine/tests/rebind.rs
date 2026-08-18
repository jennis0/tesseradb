//! **Edit is delete + re-ingest, and a deleted holder is forgotten** (decision 0047, owner-ruled
//! 2026-08-04): our retention of a dead external-id binding must never refuse a user's write.
//! The duplicate check counts only non-deleted holders, re-ingest re-binds the id, and
//! resolution prefers the newest binding — through the live map while the WAL holds the rows,
//! and through the newest-run-first sidecar walk after rotation has reclaimed them.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn row(engine: &Engine, external_id: &[u8]) -> UnallocatedRow {
    UnallocatedRow {
        external_id: Some(external_id.to_vec()),
        view: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    }
}

#[test]
fn delete_then_reingest_rebinds_the_external_id_across_flush_rotation_and_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let wal_path = tmp.path().join("wal.log");
    let open = || {
        let mut e = Engine::open(
            &root,
            &tmp.path().join("cache"),
            &wal_path,
            tessera_plugin::Passthrough::new(),
            EngineConfig {
                flush_max_age_secs: 3600,
                max_merged_segment_bytes: None,
                // Compaction §9's trigger is off unless a deployment configures one.
                compaction: tessera_engine::CompactionSchedule::off(),
                ..config()
            },
        )
        .expect("engine opens");
        e.start_write_executor(8).expect("executor starts");
        e
    };
    let engine = open();

    // First life: ingest doc-1, flush it so its binding reaches a sidecar run.
    let first = engine
        .accept_ingest(
            vec![row(&engine, b"doc-1")],
            "life-1".to_string(),
            [1u8; 32],
        )
        .expect("first ingest accepted")[0];
    engine.request_flush();
    wait_until("first flush publishes", || {
        engine.generation().segments_version >= 1
    });

    // Delete it. The binding is now a dead one and must not block a user's write.
    engine
        .accept_change(first, ChangeOp::Delete)
        .expect("delete accepted");

    // Second life: same external id, accepted — the executor's own backstop check is the one
    // this exercises (no HTTP handler in front of it here).
    let second = engine
        .accept_ingest(
            vec![row(&engine, b"doc-1")],
            "life-2".to_string(),
            [2u8; 32],
        )
        .expect("a deleted holder does not block re-ingest (decision 0047)")[0];
    assert_ne!(
        first, second,
        "I9: the dead entity's id is burned, never reused"
    );

    engine.request_flush();
    wait_until("second flush publishes", || {
        engine.generation().segments_version >= 2
    });

    // Restart onto the rotated log: the buffer was drained by the flushes, so rotation reclaimed
    // the IngestBatch records and the live map comes back empty — resolution must find the
    // newest binding through the sidecar's newest-run-first walk, not the reclaimed WAL.
    drop(engine);
    let reopened = open();
    let resolved = reopened
        .resolve_external_id(b"doc-1")
        .expect("resolution succeeds")
        .expect("doc-1 still names something");
    assert_eq!(
        resolved, second,
        "resolution must prefer the newest binding: the deleted first life is forgotten"
    );
    assert!(
        reopened.generation().overlay.is_deleted(first),
        "the first life's deny survives rotation and restart"
    );

    // And the live binding is fully operable: a suppress by external id lands on the second life.
    reopened
        .accept_change(resolved, ChangeOp::Suppress)
        .expect("suppress accepted");
    assert!(
        reopened.generation().overlay.is_suppressed(second),
        "the suppression addressed the re-ingested entity, not the forgotten one"
    );
    assert!(
        !reopened.generation().overlay.is_suppressed(first),
        "the forgotten holder accumulates no new state"
    );
}

#[test]
fn a_suppressed_holder_still_blocks_reingest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine.start_write_executor(8).expect("executor starts");

    let entity = engine
        .accept_ingest(vec![row(&engine, b"doc-2")], "s-1".to_string(), [3u8; 32])
        .expect("ingest accepted")[0];
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("suppress accepted");

    // Suppression is temporary hiding, not deletion: re-ingesting a byte-identical copy past it
    // would be the copy-no-deny-can-reach hole. The executor backstop must refuse.
    let err = engine
        .accept_ingest(vec![row(&engine, b"doc-2")], "s-2".to_string(), [4u8; 32])
        .expect_err("a suppressed holder still collides");
    assert!(
        format!("{err}").contains("duplicate") || format!("{err:?}").contains("Duplicate"),
        "expected the duplicate refusal, got {err:?}"
    );
}
