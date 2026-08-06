//! The flush tick: the one cadence on which geometry is published (write-path §4.1).
//!
//! What is asserted here is the cadence and its two triggers — the tick fires on an idle node,
//! an operator request pulls the deadline forward without bypassing the tick path, and an
//! accepted deny moves no geometry. The flush unit's own behaviour is covered by the flush and
//! promotion suites; these are the properties that must hold around it, because each is
//! fail-open if it lands the other way round.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_lifecycle::ChangeOp;
use tessera_types::EntityId;

/// Poll until `cond` holds, rather than sleeping on a guess about how long a tick takes.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn engine_with_tick(
    tmp: &tempfile::TempDir,
    root: &std::path::Path,
    flush_max_age_secs: u64,
) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    engine
}

use tessera_engine::{Engine, EngineConfig};

/// The tick fires on its own, with no traffic at all. A cadence that only advanced when something
/// else woke the executor would make visibility latency a function of load rather than of
/// `flush_max_age_secs`.
#[test]
fn the_tick_fires_on_an_idle_node() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = engine_with_tick(&tmp, &root, 1);
    wait_until("two ticks on an idle node", || {
        engine.write_executor_stats().ticks >= 2
    });
}

/// **`POST /control/flush` is accepted at any time and executed promptly** (contracts §3.4): the
/// flag pulls the tick's deadline forward and the doorbell wakes an idle executor, so the flush
/// runs at the next loop iteration — through the one tick path, never around it. With nothing
/// buffered the triggered tick plans nothing and the request is consumed; with a buffered row it
/// publishes long before the 3600 s deadline this test sets.
#[test]
fn a_requested_flush_executes_promptly_through_the_tick_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // A deadline far enough out that any publication observed below is the request's doing.
    let engine = engine_with_tick(&tmp, &root, 3600);
    let ticks_at_start = engine.write_executor_stats().ticks;

    // Empty buffer: the triggered tick fires, plans nothing, and consumes the request.
    engine.request_flush();
    wait_until("the requested tick fires on an empty buffer", || {
        engine.write_executor_stats().ticks > ticks_at_start
    });
    wait_until("the request is consumed by the tick it triggered", || {
        !engine.write_executor_stats().flush_requested
    });
    assert_eq!(
        engine.generation().segments_version,
        0,
        "nothing buffered, so the triggered tick published nothing"
    );

    // Buffered row: a second request publishes it without waiting out the deadline.
    let row = tessera_lifecycle::UnallocatedRow {
        external_id: Some(b"prompt-flush".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    engine
        .accept_ingest(vec![row], "prompt-flush-batch".to_string(), [7u8; 32])
        .expect("the row is accepted");
    engine.request_flush();
    wait_until("the buffered row is published by the requested flush", || {
        engine.generation().segments_version > 0
    });
}

/// **An accepted deny leaves `segments_version` unmoved** (§1.3's geometry/overlay split).
///
/// The overlay publication writes a side-manifest at the deny drain's close (Task 27), and the
/// guard here is what keeps that publication from ever being implemented through
/// `publish_geometry`: an overlay publication supersedes no geometry, so it must not move
/// `segments_version` — every deny would otherwise rotate the row-projection cache key and cost
/// a full projection rebuild.
#[test]
fn an_accepted_deny_moves_no_geometry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = engine_with_tick(&tmp, &root, 3600);

    let before = engine.generation();
    let entity = source_to_new_map(&root, &before.prefix)[&7];
    engine
        .accept_change(EntityId::new(entity),
            ChangeOp::Suppress)
        .expect("a suppression is accepted");

    let after = engine.generation();
    assert!(
        after.overlay_version > before.overlay_version,
        "the deny is in force"
    );
    assert_eq!(
        after.segments_version, before.segments_version,
        "an overlay publication supersedes no geometry, so it must not rotate the row-projection \
         cache key"
    );
}

/// **The deny-only regime rotates** (owner-ruled 2026-08-04; write-path §4.5). A node that takes
/// denies but never flushes — a loaded bundle with no live ingest — used to seal nothing and
/// reclaim nothing: an unbounded log on the one lane that cannot be shed. The tick now rotates
/// whenever the log has grown and no flush publication is coming to do it, and the rotation's
/// snapshot is what carries the suppression across the reclaim — asserted by restarting onto the
/// rotated log and finding it still in force.
#[test]
fn a_deny_only_node_rotates_at_the_tick_and_the_suppression_survives_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = engine_with_tick(&tmp, &root, 1);

    let wal_members = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("wal") && !n.ends_with(".sync"))
            .collect();
        names.sort();
        names
    };
    let before = wal_members();
    assert!(!before.is_empty(), "the WAL has at least its first member");

    let entity = source_to_new_map(&root, &engine.generation().prefix)[&3];
    engine
        .accept_change(EntityId::new(entity), ChangeOp::Suppress)
        .expect("a suppression is accepted");

    // The tick fires within a second; growth (the ChangeByEntity record) triggers a rotation,
    // whose reclaim deletes the original member — the buffer is empty, so the whole durable
    // prefix below the snapshot is reclaimable.
    wait_until("the original WAL member is reclaimed by a tick rotation", || {
        let now = wal_members();
        now != before && !now.is_empty()
    });

    // The suppression's only durable home is now the rotation snapshot. A restart must carry it.
    drop(engine);
    let reopened = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            max_merged_segment_bytes: None,
            ..config()
        },
    )
    .expect("the rotated log opens");
    assert!(
        reopened
            .generation()
            .overlay
            .is_suppressed(EntityId::new(entity)),
        "the suppression must survive the rotation it was reclaimed under"
    );
}
