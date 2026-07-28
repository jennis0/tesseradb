//! WAL round-trip and the positional CRC rule (task-9 brief, Step 1; plus the fail-open holes
//! found in review — C1/C2/I1 below).

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::tempdir;

use tessera_lifecycle::wal::{ChangeOp, Wal, WalError, WalRecord, HEADER_LEN};

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

/// Overwrites the 4-byte little-endian length prefix at `offset` with an implausibly large
/// value, without touching anything else in the file.
fn corrupt_length_prefix(path: &Path, offset: u64, value: u32) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&value.to_le_bytes()).unwrap();
}

fn expect_corruption(result: tessera_lifecycle::wal::Result<(Wal, Vec<WalRecord>)>) {
    match result {
        Err(WalError::WalCorruption) => {}
        Err(other) => panic!("expected WalCorruption, got a different error: {other}"),
        Ok(_) => panic!("expected WalCorruption, got Ok"),
    }
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
fn corruption_before_sync_point_fails_closed() {
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

    // Corrupt a byte inside record 1's body: HEADER_LEN (file header) + 4 (record 1's own
    // length prefix) is its first body byte — well below the sync point.
    corrupt_byte(&path, HEADER_LEN + 4);

    expect_corruption(Wal::open(&path));
}

// --- C1: a WAL that is simply shorter than the recorded sync point must fail closed, even with
// no corrupted record at all (e.g. a restored stale copy, or lost filesystem blocks). ---

#[test]
fn wal_shorter_than_sync_point_fails_closed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        let after_record1 = fs::metadata(&path).unwrap().len();
        wal.append(&sample_record(1)).unwrap();
        wal.append(&sample_record(2)).unwrap();
        // The sidecar now claims all three records are durable...
        wal.fsync().unwrap();
        // ...but the file on disk loses its tail without the sidecar being told, e.g. a
        // filesystem-level truncation or a stale restored copy.
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(after_record1).unwrap();
    }

    expect_corruption(Wal::open(&path));
}

// --- C2: a missing or malformed sidecar must default to "assume everything present is acked",
// not "assume nothing is acked" — the latter is fail-open, silently downgrading real, previously
// fsynced records to discardable tail noise. ---

fn setup_two_synced_records(path: &Path) {
    let (mut wal, _) = Wal::open(path).unwrap();
    wal.append(&sample_record(0)).unwrap();
    wal.append(&sample_record(1)).unwrap();
    wal.fsync().unwrap();
}

#[test]
fn missing_sidecar_still_fails_closed_on_corruption() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");
    let sync_path = dir.path().join("wal.sync");

    setup_two_synced_records(&path);
    fs::remove_file(&sync_path).unwrap();
    corrupt_byte(&path, HEADER_LEN + 4);

    expect_corruption(Wal::open(&path));
}

#[test]
fn zero_byte_sidecar_still_fails_closed_on_corruption() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");
    let sync_path = dir.path().join("wal.sync");

    setup_two_synced_records(&path);
    fs::OpenOptions::new()
        .write(true)
        .open(&sync_path)
        .unwrap()
        .set_len(0)
        .unwrap();
    corrupt_byte(&path, HEADER_LEN + 4);

    expect_corruption(Wal::open(&path));
}

#[test]
fn four_byte_sidecar_still_fails_closed_on_corruption() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");
    let sync_path = dir.path().join("wal.sync");

    setup_two_synced_records(&path);
    fs::OpenOptions::new()
        .write(true)
        .open(&sync_path)
        .unwrap()
        .set_len(4)
        .unwrap();
    corrupt_byte(&path, HEADER_LEN + 4);

    expect_corruption(Wal::open(&path));
}

// --- I1: a corrupted length prefix must be bounded against the file's actual remaining bytes
// before allocating for it, and then routed through the same positional CRC rule as any other
// framing failure. ---

#[test]
fn huge_length_prefix_past_sync_point_truncates_silently() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    let record3_start;
    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        wal.append(&sample_record(1)).unwrap();
        wal.fsync().unwrap();
        record3_start = fs::metadata(&path).unwrap().len();
        // Never fsynced: record 3 is discardable.
        wal.append(&sample_record(2)).unwrap();
    }

    corrupt_length_prefix(&path, record3_start, 0xFFFF_FFFF);

    let (_wal, records) = Wal::open(&path).unwrap();
    assert_eq!(records.len(), 2);
}

#[test]
fn huge_length_prefix_before_sync_point_fails_closed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        wal.append(&sample_record(1)).unwrap();
        wal.append(&sample_record(2)).unwrap();
        wal.fsync().unwrap();
    }

    // Record 1's own length prefix, right after the header.
    corrupt_length_prefix(&path, HEADER_LEN, 0xFFFF_FFFF);

    expect_corruption(Wal::open(&path));
}

// --- I3: a poisoned handle is a distinct, matchable error callers can check for. Deterministic
// fault injection into a mid-write I/O failure isn't portable in a plain unit test; this checks
// the observable contract instead. ---

#[test]
fn poisoned_error_is_distinct_and_reports_itself() {
    let err = WalError::Poisoned;
    assert_eq!(
        err.to_string(),
        "wal handle poisoned by a previous write failure — reopen from disk"
    );
}

// --- I5: a file with a bad/missing header must not be treated as a valid, empty WAL. ---

#[test]
fn bad_header_is_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");
    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&sample_record(0)).unwrap();
        wal.fsync().unwrap();
    }
    corrupt_byte(&path, 0);

    match Wal::open(&path) {
        Err(WalError::BadHeader) => {}
        Err(other) => panic!("expected BadHeader, got a different error: {other}"),
        Ok(_) => panic!("expected BadHeader, got Ok"),
    }
}
