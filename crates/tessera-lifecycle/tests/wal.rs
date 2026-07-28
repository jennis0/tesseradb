//! WAL round-trip and the positional CRC rule (task-9 brief, Step 1).

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::tempdir;

use tessera_lifecycle::wal::{ChangeOp, Wal, WalError, WalRecord};

fn sample_record(tag: u8) -> WalRecord {
    WalRecord::Change {
        external_id: vec![tag, tag, tag],
        op: ChangeOp::Delete,
        descriptors: None,
    }
}

/// Flips every bit of the byte at `offset`, in place, on disk.
fn corrupt_byte(path: &Path, offset: u64) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[b[0] ^ 0xFF]).unwrap();
}

#[test]
fn round_trip_three_records() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    {
        let (mut wal, initial) = Wal::open(&path).unwrap();
        assert!(initial.is_empty());
        for i in 0..3u8 {
            wal.append(&sample_record(i)).unwrap();
        }
        wal.fsync().unwrap();
    }

    let (_wal, records) = Wal::open(&path).unwrap();
    assert_eq!(records.len(), 3);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r, &sample_record(i as u8));
    }
}

#[test]
fn tail_corruption_past_sync_point_truncates_silently() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        wal.append(&sample_record(1)).unwrap();
        wal.fsync().unwrap();
        // Appended but never fsynced: this record was never acked.
        wal.append(&sample_record(2)).unwrap();
    }

    // Corrupt the last byte of the file — the unsynced third record's trailing CRC byte.
    let len = fs::metadata(&path).unwrap().len();
    corrupt_byte(&path, len - 1);

    let (_wal, records) = Wal::open(&path).unwrap();
    assert_eq!(records.len(), 2);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r, &sample_record(i as u8));
    }
}

#[test]
fn corruption_at_or_before_sync_point_fails_closed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        wal.append(&sample_record(1)).unwrap();
        wal.append(&sample_record(2)).unwrap();
        // All three records are durable: the sync point sits past record 1's start offset.
        wal.fsync().unwrap();
    }

    // Corrupt a byte inside record 1's body (offset 4 is the first body byte, past the 4-byte
    // length prefix) — well below the sync point.
    corrupt_byte(&path, 4);

    match Wal::open(&path) {
        Err(WalError::WalCorruption) => {}
        Err(other) => panic!("expected WalCorruption, got {other}"),
        Ok(_) => panic!("expected WalCorruption, got Ok"),
    }
}
