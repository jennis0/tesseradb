//! The WAL's on-disk row shape, and the version gate that protects it.
//!
//! Flush freezes this layout: once a segment's entity range is published, the rows that produced
//! it must be re-derivable from the log byte-for-byte. The field this file pins is the one §2.1's
//! contiguity arithmetic assumes and the format did not carry.

use tessera_lifecycle::wal::{Wal, WalError, WalRecord, WalRow};
use tessera_types::EntityId;

/// The slice a row belongs to is durable, because a flush segment's entity range is
/// contiguous only within one slice (§2.1) and the WAL is append-only.
#[test]
fn a_wal_row_round_trips_its_slice() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    let row = WalRow {
        external_id: Some(b"ext-1".to_vec()),
        entity_id: EntityId::new(7),
        slice: "default".to_string(),
        descriptors: vec![b"dept:eng".to_vec()],
        x: 1.0,
        y: 2.0,
        scalars: vec![],
    };
    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&WalRecord::IngestBatch {
            batch_id: "b1".into(),
            body_hash: [0u8; 32],
            rows: vec![row.clone()],
        })
        .unwrap();
        wal.fsync().unwrap();
    }
    let (_wal, records) = Wal::open(&path).unwrap();
    match &records[0] {
        WalRecord::IngestBatch { rows, .. } => assert_eq!(rows[0], row),
        other => panic!("expected an IngestBatch, got {other:?}"),
    }
}

/// A version-1 WAL is refused, not silently misread: postcard decodes a missing field as
/// whatever follows it in the buffer.
#[test]
fn a_version_1_wal_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    std::fs::write(&path, [b'T', b'W', b'A', b'L', 1, 0]).unwrap();
    assert!(matches!(Wal::open(&path), Err(WalError::BadHeader)));
}
