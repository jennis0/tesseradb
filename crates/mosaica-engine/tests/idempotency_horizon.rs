//! What survives a rotation: a unique value names its item while the item lives. It is asserted
//! after a restart, where nothing rebuilt from the surviving log covers what a flush published.

mod common;

use std::time::Duration;

use common::*;
use mosaica_lifecycle::UnallocatedRow;
use mosaica_types::EntityId;

const WAIT: Duration = Duration::from_secs(20);

/// Flush, then rotate away the member that carried the ingest, then reopen. The returned id is the
/// flushed entity, which holds the `id` of `key`, and the member that carried its `IngestBatch`
/// record is reclaimed.
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
