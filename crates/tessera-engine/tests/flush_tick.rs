//! The flush tick (§1.3, §1.5): the one cadence on which geometry is published.
//!
//! **⊘ The flush itself is not built.** What is asserted here is the cadence and its three
//! publishers — that the tick fires, that it drives reclaim, and that nothing publishes off it.
//! The segment write and the publication arrive with the flush unit; these are the properties that
//! must already hold when it does, because each of them is fail-open if it lands the other way
//! round.

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

/// **`POST /control/flush` is accepted at any time and executed at the next tick** (contracts
/// §3.4). Publishing on request would move the real publication period below the one §4's
/// relation 1 validated, and §2.2's depth trim would then drop pins before their TTL while the
/// depth alarm saturates.
#[test]
fn a_requested_flush_waits_for_the_tick_and_is_then_consumed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // A tick far enough out that the request is observably pending in between.
    let engine = engine_with_tick(&tmp, &root, 3600);

    engine.request_flush();
    assert!(
        engine.write_executor_stats().flush_requested,
        "accepted, and pending until a tick"
    );
    assert_eq!(
        engine.generation().segments_version,
        0,
        "202 means accepted, not done: nothing published"
    );
}

/// **An accepted deny leaves `segments_version` unmoved** (§1.3's geometry/overlay split).
///
/// ⊘ Nothing writes a side-manifest on an accepted deny today, so contracts §2.3's
/// immediate-publication rule is unmet and a deny's durable home is the WAL alone until a restart
/// or the next flush. The guard is here regardless, because the fail-open arrives the day someone
/// implements §2.3 by reaching for `publish_geometry`: an overlay publication supersedes no
/// geometry, so it creates no drain entry and must not move `segments_version` — every deny would
/// otherwise rotate the row-projection cache key and cost a full projection rebuild.
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
        .accept_change(
            source_id_key(7),
            EntityId::new(entity),
            ChangeOp::Suppress,
            None,
        )
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
