//! What rotation costs the caller: one identity path that must keep working once the WAL records
//! behind it are gone, and one that stops.
//!
//! Both are consequences of the WAL retention window becoming finite. They are asserted after a
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

/// **The idempotency window equals the WAL retention window**, and that is a client-visible
/// weakening of contracts §3.4's replay rule rather than a silent regression.
///
/// `accepted_batches` is WAL-replay-derived. Once the member carrying a batch's `IngestBatch`
/// record is reclaimed, a restart no longer recognises that batch id, so a byte-identical retry is
/// not answered as a duplicate. Rows carrying a unique value are still caught by the check above;
/// rows carrying none would be ingested twice. A client retrying across the window must therefore
/// send a unique value.
#[test]
fn a_batch_older_than_the_retained_wal_is_no_longer_recognised_as_a_duplicate() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    flushed_then_rotated(tmp.path(), &root, "ext-1");

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(
        reopened.accepted_batch("ext-1").is_none(),
        "the batch's record was reclaimed, so the horizon has passed it — asserted so the \
         weakening is an observable rather than something a client discovers"
    );
}

/// **The running process forgets a batch id at the rotation that deletes its record**, so the
/// horizon is the retained log whether or not a restart happened (#154).
///
/// The index is a cache of the WAL. Left untrimmed it answered a retry as a replay while the
/// record behind it was gone, and the same node after a restart would have called that id unknown
/// and re-ingested the rows: one client, one id, two answers, decided by whether the process had
/// been restarted since.
#[test]
fn an_accepted_batch_leaves_the_live_index_when_its_wal_member_is_rotated_away() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);
    ingest(&engine, "ext-1");
    assert!(
        engine.accepted_batch("ext-1").is_some(),
        "the batch was just accepted, so its id is held"
    );
    wait_until("the flush", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });
    wait_until("member 1 to be reclaimed", WAIT, || {
        !wal_members(&tmp.path().join("wal.log")).contains(&"wal-000001.log".to_string())
    });
    wait_until("the index to follow the log", WAIT, || {
        engine.accepted_batch("ext-1").is_none()
    });
}
