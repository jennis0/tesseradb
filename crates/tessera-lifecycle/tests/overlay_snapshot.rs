//! The overlay snapshot: what rotation must carry forward before it deletes anything.
//!
//! The overlay's only durable home is the WAL, so a rotation that reclaimed a file holding change
//! records without first re-stating them would silently un-deny. These tests pin the things that
//! make the snapshot a faithful re-statement rather than an approximation of one: that it is keyed
//! by entity rather than by external id, that it reproduces the overlay exactly, and that it
//! replays in the position it occupies.

use tempfile::TempDir;

use tessera_authz::Dict;
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{ChangeOp, Wal, WalRecord};
use tessera_lifecycle::Overlay;
use tessera_types::EntityId;

fn empty_dict() -> (Dict, TempDir) {
    let temp = TempDir::new().unwrap();
    let paths = tessera_authz::DictWriter::new(temp.path())
        .finish()
        .unwrap();
    (Dict::load(&paths).unwrap(), temp)
}

fn round_trip(overlay: &Overlay) -> Overlay {
    let entries = overlay.snapshot();
    let mut restored = Overlay::new();
    restored.apply_snapshot(&entries);
    restored
}

/// **Keyed by entity, never by external id.** An entity deleted before it was ever flushed has no
/// row and may have no external-id extent entry, so an external-id-keyed snapshot could not be
/// resolved at replay and the node would refuse to open — a benign rotation turned into a
/// permanently unopenable node.
#[test]
fn a_deleted_entity_with_no_row_round_trips_the_snapshot() {
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(99), ChangeOp::Delete);

    let entries = overlay.snapshot();
    assert_eq!(entries[0].entity_id, EntityId::new(99));

    let restored = round_trip(&overlay);
    assert!(
        restored.is_deleted(EntityId::new(99)),
        "a deletion must survive a rotation with nothing but the snapshot to carry it"
    );
}

/// **An unsuppressed entity is untouched, and the snapshot says nothing about it.**
///
/// Under a single map, `suppress → unsuppress` left a husk whose every fact was inactive, and the
/// snapshot had to preserve it because `compose`'s verdict rule gave any entry precedence over the
/// ingest buffer. Separate stores make the husk unrepresentable: an unsuppress removes the id from
/// the suppression bitmap and nothing else ever held it. This is what lifecycle §3.1 always said —
/// "unsuppress removes the entry" — and what the previous representation did not do.
#[test]
fn an_unsuppressed_entity_is_untouched_and_absent_from_the_snapshot() {
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(3), ChangeOp::Suppress);
    overlay.apply(EntityId::new(3), ChangeOp::Unsuppress);

    assert!(!overlay.touches(EntityId::new(3)));
    assert!(
        overlay.snapshot().is_empty(),
        "nothing is in force, so there is nothing to carry forward"
    );

    let restored = round_trip(&overlay);
    assert!(!restored.touches(EntityId::new(3)));
    assert!(!restored.is_suppressed(EntityId::new(3)));
}

/// The two facts are independent, so a snapshot has to carry both — a `delete` folded into a single
/// "current disposition" would drop the suppression, and `suppress → unsuppress` on a deleted
/// entity would then re-expose it.
#[test]
fn both_facts_survive_together() {
    let mut overlay = Overlay::new();
    let e = EntityId::new(11);
    overlay.apply(e, ChangeOp::Delete);
    overlay.apply(e, ChangeOp::Suppress);

    let restored = round_trip(&overlay);
    assert!(restored.is_deleted(e));
    assert!(restored.is_suppressed(e));
}

/// The same overlay must encode to the same bytes, whatever order its backing store happens to
/// iterate in — a record whose bytes depend on allocation history cannot be compared or re-encoded
/// by `Wal::retry_durability` with any confidence.
#[test]
fn a_snapshot_is_ordered_by_entity_id() {
    let mut overlay = Overlay::new();
    for id in [900u64, 3, 47, 1, 12] {
        overlay.apply(EntityId::new(id), ChangeOp::Suppress);
    }
    let ids: Vec<u64> = overlay
        .snapshot()
        .iter()
        .map(|e| e.entity_id.raw())
        .collect();
    assert_eq!(ids, vec![1, 3, 12, 47, 900]);
}

/// The record survives the log, and — the part rotation actually depends on — replay applies it
/// **at the position it occupies**, so a change record earlier in the same file still lands first.
///
/// Here that ordering is what keeps the deletion: the snapshot was taken before entity 8 was
/// deleted, so a recovery that treated the snapshot as its starting state and resumed *after* it
/// would be correct, while one that used it as a starting state and skipped what precedes it would
/// silently un-delete. The change record below the snapshot is the case that distinguishes them.
#[test]
fn a_snapshot_replays_in_position_and_never_displaces_what_precedes_it() {
    let (dict, _temp) = empty_dict();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal.log");

    let mut before = Overlay::new();
    before.apply(EntityId::new(7), ChangeOp::Suppress);

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&WalRecord::ChangeByEntity {
            entity_id: EntityId::new(8),
            op: ChangeOp::Delete,
        })
        .unwrap();
        wal.append(&WalRecord::OverlaySnapshot {
            entries: before.snapshot(),
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let (_wal, records) = Wal::open(&path).unwrap();
    let (overlay, _buffer, _established, _resolver) = replay(&records, &dict, Overlay::new());

    assert!(
        overlay.is_suppressed(EntityId::new(7)),
        "the snapshot's own entries must apply"
    );
    assert!(
        overlay.is_deleted(EntityId::new(8)),
        "a change record below the snapshot must not be displaced by it"
    );
}

/// A snapshot folds into an overlay that already holds state, rather than replacing it: replay is
/// one left-to-right pass and the snapshot is a record in it.
#[test]
fn a_snapshot_unions_with_state_already_applied() {
    let mut snapshotted = Overlay::new();
    snapshotted.apply(EntityId::new(1), ChangeOp::Delete);

    let mut live = Overlay::new();
    live.apply(EntityId::new(2), ChangeOp::Suppress);
    live.apply_snapshot(&snapshotted.snapshot());

    assert!(live.is_deleted(EntityId::new(1)));
    assert!(
        live.is_suppressed(EntityId::new(2)),
        "applying a snapshot must not discard what was already there"
    );
}
