//! Rotation: sealing a member, carrying the overlay forward, and reclaiming what a flush has made
//! redundant.
//!
//! Everything here is a property of the WAL *sequence*. The engine-level obligations — that a row
//! acked during a flush survives, that a suppression outlives every checkpoint, that the allocator
//! floor comes from the side-manifest — live in `tessera-engine`'s `rotation_e2e.rs`, because they
//! need a flush to have happened.

use std::path::{Path, PathBuf};

use tempfile::tempdir;

use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{
    ChangeOp, OverlaySnapshotEntry, Wal, WalError, WalRecord, HEADER_LEN,
};
use tessera_lifecycle::Overlay;
use tessera_types::EntityId;

fn member(base: &Path, n: u64) -> PathBuf {
    base.with_file_name(format!("wal-{n:06}.log"))
}

fn sidecar(base: &Path, n: u64) -> PathBuf {
    base.with_file_name(format!("wal-{n:06}.sync"))
}

fn change(tag: u8) -> WalRecord {
    WalRecord::Change {
        external_id: vec![tag; 4],
        op: ChangeOp::Suppress,
        descriptors: None,
    }
}

fn suppression_of(entity: u64) -> OverlaySnapshotEntry {
    OverlaySnapshotEntry {
        entity_id: EntityId::new(entity),
        op: ChangeOp::Suppress,
        descriptors: None,
    }
}

/// A sequence with `files` members, each holding one `Change`, rotated between them and never
/// reclaiming. Returns the handle and the position each member ends at.
fn wal_with_files(base: &Path, files: u64) -> (Wal, Vec<u64>) {
    let (mut wal, _) = Wal::open(base).unwrap();
    let mut ends = Vec::new();
    for i in 0..files {
        wal.append(&change(i as u8)).unwrap();
        wal.fsync().unwrap();
        if i + 1 < files {
            ends.push(wal.position());
            // `reclaim_below = 0`: rotate, keep everything.
            assert!(wal.rotate(&[], 0).unwrap().is_empty());
        }
    }
    (wal, ends)
}

/// The base name is a name for the *sequence*, and members are numbered from one.
#[test]
fn a_fresh_log_is_a_one_member_sequence() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (wal, records) = Wal::open(&base).unwrap();

    assert!(records.is_empty());
    assert_eq!(wal.members(), vec![1]);
    assert!(member(&base, 1).exists());
    assert!(sidecar(&base, 1).exists());
    assert!(!base.exists(), "the base path is never itself a file");
}

/// Positions are sequence-global and count record bytes only, so they stay comparable across a
/// rotation — which is what lets `wal_pos` name a point in a file that has since been deleted.
#[test]
fn positions_continue_across_a_rotation() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, _) = Wal::open(&base).unwrap();

    assert_eq!(wal.position(), 0);
    wal.append(&change(0)).unwrap();
    wal.fsync().unwrap();
    let before = wal.position();
    assert!(before > 0);

    wal.rotate(&[], 0).unwrap();
    assert!(
        wal.position() > before,
        "the new member continues the sequence's positions rather than restarting at zero"
    );
    assert_eq!(wal.members(), vec![1, 2]);
}

/// **Reclamation deletes oldest first.** A crash midway through an unordered deletion leaves a gap
/// in the numbering, and a gap fails closed — turning a benign crash into a permanently unopenable
/// node.
#[test]
fn reclamation_deletes_oldest_first() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, ends) = wal_with_files(&base, 4);

    // Everything below member 3's start: members 1 and 2 lie wholly below it, member 3 does not.
    let deleted = wal.rotate(&[], ends[1]).unwrap();

    assert_eq!(deleted, vec![1, 2]);
    assert!(!member(&base, 1).exists());
    assert!(!member(&base, 2).exists());
    assert!(!sidecar(&base, 1).exists());
    assert!(member(&base, 3).exists());
    assert_eq!(wal.members(), vec![3, 4, 5]);
}

/// A member that is not *wholly* below the reclamation point survives — its tail is still live, and
/// deleting it would take acked records with it. Steady-state retention is two members for exactly
/// this reason.
#[test]
fn a_member_still_live_above_the_reclamation_point_survives() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, ends) = wal_with_files(&base, 3);

    // One byte short of member 1's end.
    let deleted = wal.rotate(&[], ends[0] - 1).unwrap();
    assert!(deleted.is_empty());
    assert_eq!(wal.members(), vec![1, 2, 3, 4]);
}

/// The active member is never a reclamation candidate, however high `reclaim_below` goes.
#[test]
fn the_active_member_is_never_reclaimed() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, _) = wal_with_files(&base, 2);

    let deleted = wal.rotate(&[], u64::MAX).unwrap();
    assert_eq!(deleted, vec![1, 2], "everything sealed goes");
    assert_eq!(wal.members(), vec![3], "and the active member stays");
    assert!(member(&base, 3).exists());
}

/// **The snapshot is durable before anything is deleted**, so a suppression whose `Change` record
/// was reclaimed is still in force after a restart. This is the property the whole ordering exists
/// for: the overlay's only durable home is the WAL.
#[test]
fn a_suppression_survives_the_reclamation_of_the_record_that_carried_it() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let temp = tempdir().unwrap();
    let dict = tessera_authz::Dict::load(
        &tessera_authz::DictWriter::new(temp.path())
            .finish()
            .unwrap(),
    )
    .unwrap();

    let entity = EntityId::new(42);
    {
        let (mut wal, _) = Wal::open(&base).unwrap();
        wal.append(&WalRecord::Change {
            external_id: b"ext-42".to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        })
        .unwrap();
        wal.fsync().unwrap();

        let mut overlay = Overlay::new();
        overlay.apply(entity, ChangeOp::Suppress, None);
        // Rotate with everything below the current position reclaimable: the member holding the
        // `Change` goes, and only the snapshot carries the suppression forward.
        let deleted = wal.rotate(&overlay.snapshot(), wal.position()).unwrap();
        assert_eq!(deleted, vec![1], "the record's own member must actually go");
    }

    let (_wal, records) = Wal::open(&base).unwrap();
    let (overlay, _buffer, _established, _resolver) =
        replay(&records, &dict, |_| Ok::<_, std::convert::Infallible>(None)).unwrap();
    assert!(
        overlay.is_suppressed(entity),
        "a suppression retires only on unsuppress — reclaiming its record must not retire it"
    );
}

/// Recovery walks **every** surviving member in order, applying the snapshot at the position it
/// occupies. Starting *at* the newest snapshot would skip the older member's records above the
/// point it was taken at — which looks like an optimisation and is a silent un-deny.
#[test]
fn recovery_walks_every_surviving_member_in_order() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    {
        let (mut wal, _) = Wal::open(&base).unwrap();
        wal.append(&change(0)).unwrap();
        wal.fsync().unwrap();
        // Nothing reclaimed, so member 1 survives with its record above nothing at all...
        wal.rotate(&[suppression_of(7)], 0).unwrap();
        wal.append(&change(1)).unwrap();
        wal.fsync().unwrap();
    }

    let (wal, records) = Wal::open(&base).unwrap();
    assert_eq!(wal.members(), vec![1, 2]);
    assert_eq!(
        records,
        vec![
            change(0),
            WalRecord::OverlaySnapshot {
                entries: vec![suppression_of(7)]
            },
            change(1),
        ],
        "every member's records, in sequence order, snapshot included and in position"
    );
}

/// A **gap** in the numbering fails closed: a member deleted out of order, or lost, takes acked
/// records with it and leaves nothing to mark their absence.
#[test]
fn a_gap_in_the_sequence_fails_closed() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    drop(wal_with_files(&base, 3));

    std::fs::remove_file(member(&base, 2)).unwrap();

    assert!(matches!(Wal::open(&base), Err(WalError::WalCorruption)));
}

/// A member whose header base position does not continue its predecessor's durable end is a stale
/// or foreign file that has taken a member's place. Every position derived from it afterwards would
/// name the wrong bytes, so it fails closed rather than being trusted.
#[test]
fn a_broken_position_chain_fails_closed() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    drop(wal_with_files(&base, 2));

    // Rewrite member 2's base position to a value member 1 does not end at.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(member(&base, 2))
        .unwrap();
    f.seek(SeekFrom::Start(14)).unwrap();
    f.write_all(&9999u64.to_le_bytes()).unwrap();
    f.sync_all().unwrap();

    assert!(matches!(Wal::open(&base), Err(WalError::WalCorruption)));
}

/// A member carrying another member's number in its header is not a continuation: it would take its
/// neighbour's place in the walk with nothing to say so.
#[test]
fn a_member_whose_header_number_disagrees_with_its_name_fails_closed() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    drop(wal_with_files(&base, 2));

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(member(&base, 2))
        .unwrap();
    f.seek(SeekFrom::Start(6)).unwrap();
    f.write_all(&7u64.to_le_bytes()).unwrap();
    f.sync_all().unwrap();

    assert!(matches!(Wal::open(&base), Err(WalError::BadHeader)));
}

/// A crash between creating a member and writing its header leaves a zero-length file. Refusing it
/// would make a benign crash a permanently unopenable node, and it is exactly recoverable: the
/// number comes from the name and the base position from the chain.
#[test]
fn a_zero_length_newest_member_is_re_headered_rather_than_refused() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (wal, _ends) = wal_with_files(&base, 2);
    let position = wal.position();
    drop(wal);

    std::fs::write(member(&base, 3), b"").unwrap();

    let (wal, records) = Wal::open(&base).unwrap();
    assert_eq!(wal.members(), vec![1, 2, 3]);
    assert_eq!(
        records.len(),
        3,
        "the two changes and the rotation's own snapshot are all unaffected"
    );
    assert_eq!(
        wal.position(),
        position,
        "and the re-headered member continues the sequence rather than restarting it"
    );
}

/// A rotation onto a poisoned handle does nothing: nothing above the last durable offset may be
/// built on, and the caller has a repair to attempt first.
#[test]
fn a_poisoned_handle_rotates_nothing() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // The read-only-directory trick does not bind root.
    if unsafe { libc_geteuid() } == 0 {
        eprintln!("skipped: running as root, where a read-only directory is not read-only");
        return;
    }

    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, _) = Wal::open(&base).unwrap();
    wal.append(&change(0)).unwrap();
    wal.fsync().unwrap();

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    wal.append(&change(1)).unwrap();
    assert!(matches!(wal.fsync(), Err(WalError::Io(_))));

    assert!(matches!(wal.rotate(&[], 0), Err(WalError::Poisoned)));

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(wal.members(), vec![1], "and no member was created");
}

unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

/// Records appended but never fsynced are discarded by the rotation's own sync, exactly as a
/// restart would discard them — a rotation must not sweep unacked bytes into the durable prefix.
#[test]
fn the_first_member_starts_its_records_at_the_header() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("wal.log");
    let (mut wal, _) = Wal::open(&base).unwrap();
    wal.append(&change(0)).unwrap();
    let end = wal.fsync().unwrap();
    assert_eq!(
        end - HEADER_LEN,
        wal.position(),
        "a member's file offset and the sequence position differ by exactly its header"
    );
}
