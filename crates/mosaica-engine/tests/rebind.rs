//! **A deleted holder of a unique value is forgotten**: our retention of a deleted item's value
//! must never refuse a user's write. The check counts only live holders, a re-ingest gives the
//! value to a new item, and a lookup names that item — while the WAL holds the rows, and through
//! the key runs after rotation has reclaimed them.

mod common;

use std::time::Duration;

use common::*;
use mosaica_engine::{Engine, EngineConfig};
use mosaica_lifecycle::{ChangeOp, UnallocatedRow};

const WAIT: Duration = Duration::from_secs(10);

/// A new item holding the `id` of `key`.
fn row(engine: &Engine, key: &str) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: keyed(key),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    }
}

#[test]
fn delete_then_reingest_gives_the_value_to_the_new_item_across_flush_rotation_and_restart() {
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
            EngineConfig {
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
        e.start_write_executor(8).expect("executor starts");
        e
    };
    let engine = open();

    // First life: ingest doc-1, flush it so its value reaches a key run.
    let first = engine
        .ingest_rows(
            vec![row(&engine, "doc-1")],
            "life-1".to_string(),
            [1u8; 32],
        )
        .expect("first ingest accepted")[0];
    engine.request_flush();
    wait_until("first flush publishes", WAIT, || {
        engine.generation().segments_version >= 1
    });

    // Delete it. Its value is now held by a deleted item and must not block a user's write.
    engine
        .accept_change(first, ChangeOp::Delete)
        .expect("delete accepted");

    // Second life: the same value, accepted — the executor's own backstop check is the one this
    // exercises (no HTTP handler in front of it here).
    let second = engine
        .ingest_rows(
            vec![row(&engine, "doc-1")],
            "life-2".to_string(),
            [2u8; 32],
        )
        .expect("a deleted holder does not block re-ingest")[0];
    assert_ne!(
        first, second,
        "I9: the dead entity's id is burned, never reused"
    );

    engine.request_flush();
    wait_until("second flush publishes", WAIT, || {
        engine.generation().segments_version >= 2
    });

    // Restart onto the rotated log: the buffer was drained by the flushes, so rotation reclaimed
    // the IngestBatch records — the lookup must find the live holder through the key runs, not
    // the reclaimed WAL.
    drop(engine);
    let reopened = open();
    let resolved = item_of_key(&reopened, "doc-1").expect("doc-1 still names something");
    assert_eq!(
        resolved, second,
        "the lookup names the live holder: the deleted first life is forgotten"
    );
    assert!(
        reopened.generation().overlay.is_deleted(first),
        "the first life's deny survives rotation and restart"
    );

    // And the value is fully operable: a suppress through it lands on the second life.
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

/// An engine over the fixture in `dir`, its executor running and no flush or fold of its own.
fn open_engine(dir: &std::path::Path) -> Engine {
    let root = dir.join("bundle");
    build_fixture(&root, &dir.join("points.parquet"), &dir.join("pairs.parquet"));
    let mut engine = Engine::open(
        &root,
        &dir.join("cache"),
        &dir.join("wal.log"),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: mosaica_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine.start_write_executor(8).expect("executor starts");
    engine
}

/// A suppressed item still holds its unique value: the same row sent again names it and changes
/// nothing, rather than creating a copy no deny reaches.
#[test]
fn a_suppressed_holder_is_named_by_a_reingest_and_no_copy_is_made() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(tmp.path());

    let entity = engine
        .ingest_rows(vec![row(&engine, "doc-2")], "s-1".to_string(), [3u8; 32])
        .expect("ingest accepted")[0];
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("suppress accepted");

    let again = engine
        .ingest_rows(vec![row(&engine, "doc-2")], "s-2".to_string(), [4u8; 32])
        .expect("a row naming a suppressed item is accepted");
    assert_eq!(again, vec![entity], "the row names the suppressed item");
    assert_eq!(item_of_key(&engine, "doc-2"), Some(entity));
}
