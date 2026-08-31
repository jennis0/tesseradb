//! The WAL's on-disk row shape, and the version gate that protects it.
//!
//! Flush freezes this layout: once a segment's entity range is published, the rows that produced
//! it must be re-derivable from the log byte-for-byte. The field this file pins is the one §2.1's
//! contiguity arithmetic assumes and the format did not carry.

use tessera_lifecycle::wal::{Wal, WalError, WalRecord, WalRow};
use tessera_types::EntityId;

/// The view a row belongs to is durable, because a flush segment's entity range is
/// contiguous only within one view (§2.1) and the WAL is append-only.
#[test]
fn a_wal_row_round_trips_its_view() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    let row = WalRow {
        external_id: Some(b"ext-1".to_vec()),
        entity_id: EntityId::new(7),
        view: "default".to_string(),
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

/// A well-formed header at some other version: `WAL_MAGIC`, the version, the member number and the
/// base position, all present and all right except the one field under test.
///
/// A *short* header is refused on its length before the version is ever compared, so a fixture
/// that writes six bytes passes against no version check at all — which is why every case below
/// writes the whole thing.
fn header_at_version(version: u16, number: u64, base_pos: u64) -> Vec<u8> {
    let mut header = b"TWAL".to_vec();
    header.extend_from_slice(&version.to_le_bytes());
    header.extend_from_slice(&number.to_le_bytes());
    header.extend_from_slice(&base_pos.to_le_bytes());
    header
}

/// An older-version WAL is refused, not silently misread: postcard decodes a missing field as
/// whatever follows it in the buffer.
///
/// Written under a member's real name, because the base path is a name for the *sequence* and a
/// file sitting there is not part of it — `Wal::open` would ignore it and start a fresh log, which
/// is the one answer a version check must never give.
#[test]
fn an_older_version_wal_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    std::fs::write(
        dir.path().join("wal-000001.log"),
        header_at_version(1, 1, 0),
    )
    .unwrap();
    assert!(matches!(Wal::open(&path), Err(WalError::BadHeader)));
}

/// A coordinate the log has to carry at full width, round-tripped exactly.
///
/// The whole coordinate path is `f64` (`projections.md` §6), and this record is the middle of it:
/// the wire reads the caller's value, the log stores it and the flush quantises it. The value here
/// is deliberately one `f32` does not hold — it is chosen so the narrowed form is a *different*
/// position, not a rounder one — so a record that lost four bytes of it would fail here rather
/// than in a cell count somewhere downstream.
#[test]
fn a_wal_row_round_trips_a_coordinate_no_f32_holds() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    let (x, y) = (65_508.501_f64, 65_512.003_f64);
    assert_ne!(
        f64::from(x as f32),
        x,
        "the fixture coordinate must be one f32 cannot hold, or this test discriminates nothing"
    );
    assert_ne!(f64::from(y as f32), y);

    let row = WalRow {
        external_id: Some(b"ext-1".to_vec()),
        entity_id: EntityId::new(7),
        view: "default".to_string(),
        descriptors: vec![b"dept:eng".to_vec()],
        x,
        y,
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
        WalRecord::IngestBatch { rows, .. } => {
            assert_eq!(rows[0].x, x);
            assert_eq!(rows[0].y, y);
        }
        other => panic!("expected an IngestBatch, got {other:?}"),
    }
}

/// **A log at version 15 — the version before the coordinates widened — is refused.**
///
/// Kept beside [`an_older_version_wal_is_refused`] rather than folded into it, because the two
/// guard different failures. That one rules out a header from an arbitrary past. This one rules
/// out the *immediately preceding* version, which is the only one a stale local log is likely to
/// be at, and whose records a version-16 reader would otherwise find entirely plausible: a field's
/// type changed rather than a variant being added, so the record length is right, the CRC is right,
/// and four of the eight bytes a coordinate now takes come out of whatever field follows it. The
/// row would land somewhere on the grid instead of failing.
#[test]
fn a_log_at_the_version_before_the_coordinates_widened_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    std::fs::write(
        dir.path().join("wal-000001.log"),
        header_at_version(15, 1, 0),
    )
    .unwrap();
    assert!(matches!(Wal::open(&path), Err(WalError::BadHeader)));

    // The same header at the current version is *accepted*, which is what makes the refusal above
    // a statement about the version rather than about the rest of the header.
    let other = dir.path().join("current");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(
        other.join("wal-000001.log"),
        header_at_version(16, 1, 0),
    )
    .unwrap();
    assert!(Wal::open(other.join("wal.log")).is_ok());
}

/// A create and a drop round-trip, and the drop replays whatever else the log carries
/// (`views.md` §3.2): the roster's WAL half is what puts a created view back between a
/// publication and a restart.
#[test]
fn view_create_and_drop_round_trip() {
    use tessera_types::view::{CreatedView, TombstonedView, ViewMetadataValue};
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wal.log");
    let created = CreatedView {
        group: "quarter".to_string(),
        key: "2026-Q5".to_string(),
        ordinal: 4,
        visibility: None,
        metadata: [(
            "label".to_string(),
            ViewMetadataValue::Text("Q5 2026".to_string()),
        )]
        .into_iter()
        .collect(),
    };
    let dropped = TombstonedView {
        group: "quarter".to_string(),
        key: "2026-Q4".to_string(),
        ordinal: 3,
    };
    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&WalRecord::ViewCreate {
            view: created.clone(),
        })
        .unwrap();
        wal.append(&WalRecord::ViewDrop {
            view: dropped.clone(),
        })
        .unwrap();
        wal.fsync().unwrap();
    }
    let (_wal, records) = Wal::open(&path).unwrap();
    assert_eq!(records.len(), 2);
    match (&records[0], &records[1]) {
        (WalRecord::ViewCreate { view }, WalRecord::ViewDrop { view: stone }) => {
            assert_eq!(view, &created);
            assert_eq!(stone, &dropped);
        }
        other => panic!("expected a create then a drop, got {other:?}"),
    }
}
