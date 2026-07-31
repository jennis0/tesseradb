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

// --- Phase 2 stage 2.1, Task 3a: poisoning, for real and injected. ---
//
// `poisoned_error_is_distinct_and_reports_itself` above asserts a Display string and nothing else,
// which is why the Phase 1 ledger carries "deny-op WAL-failure path inspection-only" as a
// deferral. These two close it, and they exist as a **pair**: the first pins what a real I/O
// failure does, the second pins that the injected fault does the same thing. Task 3a's
// `deny_append_failure_still_applies` and `a_poisoned_wal_trips_the_not_ready_posture` both run on
// injection, so if the two ever disagree those tests are measuring the harness rather than the
// engine.

/// A **genuine** I/O failure — not an injected one — poisons the handle, and the sequence is
/// `Io` first, `Poisoned` after.
///
/// The failure is provoked by making the WAL's directory read-only. `Wal::fsync` calls
/// `sync_data()` on an already-open fd (unaffected by the mode change) and then does the sidecar's
/// write-tmp-then-rename, whose `open` needs write permission on the *directory* — so it fails
/// with `EACCES` and lands in the arm that sets `poisoned`. That is the sidecar branch
/// specifically, not the `sync_data` branch; the distinction is recorded because Task 8's crash
/// test depends on which of the two a fault represents.
///
/// Skipped under uid 0: `chmod` does not bind root, so on a root CI runner this would silently
/// assert nothing rather than fail.
#[test]
fn a_real_fsync_failure_poisons_the_handle() {
    if unsafe { geteuid() } == 0 {
        eprintln!("skipped: running as root, where a read-only directory is not read-only");
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let path = dir.path().join("wal.log");
    let (mut wal, _) = Wal::open(&path).unwrap();
    wal.append(&sample_record(1)).unwrap();
    wal.fsync().unwrap();
    assert!(!wal.is_poisoned(), "a healthy handle is not poisoned");

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    wal.append(&sample_record(2)).unwrap();

    let first = wal.fsync();
    assert!(
        matches!(first, Err(WalError::Io(_))),
        "the FAILING call must report the I/O error itself, not Poisoned — it is the variant the \
         500 mapping and the deny-op alarm both branch on; got {first:?}"
    );
    assert!(wal.is_poisoned(), "the failing call must poison the handle");

    // The name claims the handle, so assert BOTH operations refuse, not just the one that failed.
    assert!(matches!(wal.fsync(), Err(WalError::Poisoned)));
    assert!(matches!(
        wal.append(&sample_record(3)),
        Err(WalError::Poisoned)
    ));

    // Restore before `TempDir` drops, or the directory leaks — this box runs near a full disk.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
}

/// The injected fault reproduces that sequence exactly: `Io` on the failing call, `Poisoned` on
/// every call after, and `is_poisoned()` true throughout.
///
/// This is the assertion that lets Task 3a's WAL-failure tests mean anything. Without it, a
/// switchboard that returned `Poisoned` on the *first* call would pass every one of them while
/// describing a failure mode the real WAL never produces.
#[cfg(feature = "fault-injection")]
#[test]
fn an_injected_failure_is_indistinguishable_from_a_real_one() {
    use std::sync::Arc;
    use tessera_lifecycle::faults::{FaultSwitchboard, WalMeter};
    use tessera_lifecycle::wal::ExecutorWal;

    let dir = tempdir().unwrap();
    let (wal, _) = Wal::open(dir.path().join("wal.log")).unwrap();
    let faults = Arc::new(FaultSwitchboard::new());
    let meter = Arc::new(WalMeter::new());
    let mut wal = ExecutorWal::new(wal, Arc::clone(&meter)).with_faults(Arc::clone(&faults));

    wal.append(&sample_record(1)).unwrap();
    wal.fsync().unwrap();
    assert_eq!((meter.appends(), meter.fsyncs()), (1, 1));

    faults.fail_next_fsyncs(1);
    wal.append(&sample_record(2)).unwrap();
    let first = wal.fsync();
    assert!(
        matches!(first, Err(WalError::Io(_))),
        "an injected failure must report Io on the failing call, exactly as a real one does; \
         got {first:?}"
    );
    assert!(wal.is_poisoned());
    assert!(matches!(wal.fsync(), Err(WalError::Poisoned)));
    assert!(matches!(
        wal.append(&sample_record(3)),
        Err(WalError::Poisoned)
    ));

    // A failed operation is not counted: the meter measures durability actually achieved, which is
    // what Task 7a's `one_fsync_per_window` is an assertion about.
    assert_eq!(
        (meter.appends(), meter.fsyncs()),
        (2, 1),
        "the failed fsync must not be counted"
    );
}

/// The **append** injection arm, which nothing else exercises.
///
/// Every WAL-failure test in the tree reaches for `fail_next_fsyncs`, so without this
/// `fail_next_appends` was an unverified arm of a harness whose entire value is fidelity — the one
/// thing a fault switchboard may not have. The real sequence it must match is `Wal::append`'s error
/// arm: `Io` on the failing call (the partial `write_all` that left `self.len` unable to name a
/// record boundary), `Poisoned` on everything after, and a poisoned handle throughout.
///
/// A real append failure is harder to provoke than a real fsync failure — a read-only *directory*
/// does not stop writes to an already-open fd, which is precisely why the fsync test above works —
/// so this pins the injected arm against the source of truth in `wal.rs` rather than against a
/// second provoked failure.
#[cfg(feature = "fault-injection")]
#[test]
fn an_injected_append_failure_follows_the_real_sequence() {
    use std::sync::Arc;
    use tessera_lifecycle::faults::{FaultSwitchboard, WalMeter};
    use tessera_lifecycle::wal::ExecutorWal;

    let dir = tempdir().unwrap();
    let (wal, _) = Wal::open(dir.path().join("wal.log")).unwrap();
    let faults = Arc::new(FaultSwitchboard::new());
    let meter = Arc::new(WalMeter::new());
    let mut wal = ExecutorWal::new(wal, Arc::clone(&meter)).with_faults(Arc::clone(&faults));

    wal.append(&sample_record(1)).unwrap();
    wal.fsync().unwrap();

    faults.fail_next_appends(1);
    let first = wal.append(&sample_record(2));
    assert!(
        matches!(first, Err(WalError::Io(_))),
        "an injected append failure must report Io on the failing call, exactly as `Wal::append`'s \
         own error arm does; got {first:?}"
    );
    assert!(
        wal.is_poisoned(),
        "and it must poison the handle — a partial append cannot name a record boundary"
    );

    // BOTH operations refuse afterwards, not just the one that failed: the poison is the handle's.
    assert!(matches!(
        wal.append(&sample_record(3)),
        Err(WalError::Poisoned)
    ));
    assert!(matches!(wal.fsync(), Err(WalError::Poisoned)));

    assert_eq!(
        (meter.appends(), meter.fsyncs()),
        (1, 1),
        "the failed append must not be counted"
    );
}

extern "C" {
    #[link_name = "geteuid"]
    fn geteuid() -> u32;
}
