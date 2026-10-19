//! What rotation costs the caller: the identity paths that must keep working once the WAL records
//! behind them are gone. A unique value names its item for as long as the item lives, and a batch
//! id is answered as a replay for the horizon the rotation carries it for. Each is asserted after a
//! **restart**, because that is the only state in which nothing rebuilt from the surviving WAL
//! records covers what a flush published.

mod common;

use std::time::Duration;

use common::*;
use mosaica_lifecycle::UnallocatedRow;
use mosaica_types::EntityId;

const WAIT: Duration = Duration::from_secs(20);

/// Flush, then rotate away the member that carried the ingest, then reopen. The returned id is the
/// flushed entity, which holds the `id` of `key`, and its `IngestBatch` record no longer exists
/// anywhere.
fn flushed_then_rotated(tmp: &std::path::Path, root: &std::path::Path, key: &str) -> EntityId {
    let engine = engine_at(tmp, root, 1);
    let id = ingest_keyed(&engine, key);
    wait_until("the flush", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });
    wait_until("member 1 to be reclaimed", WAIT, || {
        !wal_members(&tmp.join("wal.log")).contains(&"wal-000001.log".to_string())
    });
    id
}

/// One item at the fixture's centre holding the `id` of `key`, under the batch id `key`.
fn ingest_keyed(engine: &mosaica_engine::Engine, key: &str) -> EntityId {
    let row = UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: keyed(key),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .ingest_rows(vec![row], key.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

/// **A unique value that lives only in a flushed run still names its item**, after rotation and a
/// restart, and a row sending it again names that item rather than making a copy no deny reaches.
#[test]
fn a_value_held_only_in_a_flushed_run_names_its_item_after_rotation_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    let id = flushed_then_rotated(tmp.path(), &root, "ext-1");

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert_eq!(item_of_key(&reopened, "ext-1"), Some(id));
    let again = {
        let row = UnallocatedRow {
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: 5.0,
            y: 5.0,
            scalars: keyed("ext-1"),
            terms: reopened.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        };
        reopened
            .ingest_rows(vec![row], "again".to_string(), [1u8; 32])
            .expect("the row is accepted")[0]
    };
    assert_eq!(again, id, "the row names the flushed item");
}

/// **A batch is remembered after the rotation that reclaims its record**, live and across a
/// restart: the rotation carries it, for the flush tick after its acceptance.
#[test]
fn a_batch_is_remembered_after_its_record_is_reclaimed_and_across_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    {
        let engine = engine_at(tmp.path(), &root, 3600);
        ingest(&engine, "ext-1");
        engine.request_flush();
        reclaimed(&engine, tmp.path());
        assert!(engine.accepted_batch("", "ext-1").is_some());
    }
    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(reopened.accepted_batch("", "ext-1").is_some());
}

/// **A batch past the flush tick after its acceptance is forgotten at the next rotation**, and a
/// restart does not bring it back.
#[test]
fn a_batch_past_its_horizon_is_forgotten_live_and_after_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    {
        let engine = engine_at(tmp.path(), &root, 1);
        ingest(&engine, "ext-1");
        reclaimed(&engine, tmp.path());
        std::thread::sleep(Duration::from_millis(1_500));
        ingest(&engine, "ext-2");
        wait_until("the index to forget the batch", WAIT, || {
            engine.accepted_batch("", "ext-1").is_none()
        });
    }
    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(reopened.accepted_batch("", "ext-1").is_none());
}

/// Wait for a flush and for the rotation after it to reclaim the log's first member.
fn reclaimed(engine: &mosaica_engine::Engine, tmp: &std::path::Path) {
    wait_until("the flush", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });
    wait_until("member 1 to be reclaimed", WAIT, || {
        !wal_members(&tmp.join("wal.log")).contains(&"wal-000001.log".to_string())
    });
}
