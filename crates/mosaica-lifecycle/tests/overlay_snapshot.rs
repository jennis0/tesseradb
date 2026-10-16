//! The overlay snapshot: what rotation must carry forward before it deletes anything.
//!
//! The overlay's only durable home is the WAL, so a rotation that reclaimed a file holding change
//! records without first re-stating them would silently un-deny. These tests pin the things that
//! make the snapshot a faithful re-statement rather than an approximation of one: that it is keyed
//! by target, that it reproduces each overlay exactly, and that it replays in the position it
//! occupies.

use tempfile::TempDir;

use mosaica_authz::Dict;
use mosaica_lifecycle::overlay::Replay;
use mosaica_lifecycle::wal::{ChangeOp, Wal, WalRecord};
use mosaica_lifecycle::{Overlay, Overlays};
use mosaica_types::{EntityId, LayerSpace, ShardId, Target};

fn empty_dict() -> (Dict, TempDir) {
    let temp = TempDir::new().unwrap();
    let paths = mosaica_authz::DictWriter::new(temp.path())
        .finish()
        .unwrap();
    (Dict::load(&paths).unwrap(), temp)
}

fn round_trip(overlay: &Overlay) -> Overlay {
    let entries = overlay.snapshot(item);
    let mut restored = Overlays::default();
    restored.apply_snapshot(&entries);
    shard_zero(restored)
}

/// Shard 0's items of `overlays`.
fn shard_zero(mut overlays: Overlays) -> Overlay {
    overlays.items.remove(&ShardId::ZERO).unwrap_or_default()
}

/// **Keyed by entity.** An entity deleted before it was ever flushed has no row and may hold no
/// value, so a snapshot keyed by anything else could not be resolved at replay and the node would
/// refuse to open.
#[test]
fn a_deleted_entity_with_no_row_round_trips_the_snapshot() {
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(99), ChangeOp::Delete);

    let entries = overlay.snapshot(item);
    assert_eq!(entries[0].target, item(EntityId::new(99)));

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
        overlay.snapshot(item).is_empty(),
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
        .snapshot(item)
        .iter()
        .map(|e| e.target.entity().raw())
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
        let mut wal = Wal::open(&path).unwrap();
        wal.append(&WalRecord::ChangeBatch {
            changes: vec![(item(EntityId::new(8)), ChangeOp::Delete)],
        })
        .unwrap();
        wal.append(&WalRecord::OverlaySnapshot {
            entries: before.snapshot(item),
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let wal = Wal::open(&path).unwrap();
    let mut replay = Replay::new(
        &dict,
        Overlays::default(),
        &mosaica_lifecycle::owner_id_only,
    );
    for record in wal.records() {
        let (position, record) = record.unwrap();
        replay.apply(&record, position, |_, _, _| false);
    }
    let overlay = shard_zero(replay.finish().0);

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

    let mut live = Overlays::default();
    live.apply(item(EntityId::new(2)), ChangeOp::Suppress);
    live.apply_snapshot(&snapshotted.snapshot(item));
    let live = shard_zero(live);

    assert!(live.is_deleted(EntityId::new(1)));
    assert!(
        live.is_suppressed(EntityId::new(2)),
        "applying a snapshot must not discard what was already there"
    );
}

/// An artifact's deny is restored into its layer space's overlay and never the items', so an
/// entity number an item and an artifact share is denied only where it was.
#[test]
fn a_snapshot_restores_each_deny_into_the_overlay_its_target_names() {
    let space = LayerSpace::new(4).unwrap();
    let mut overlays = Overlays::default();
    overlays.apply(Target::Artifact(space, EntityId::new(5)), ChangeOp::Suppress);
    overlays.apply(item(EntityId::new(6)), ChangeOp::Delete);

    let one = ShardId::new(1).unwrap();
    overlays.apply(Target::Item(one, EntityId::new(7)), ChangeOp::Suppress);

    let mut restored = Overlays::default();
    restored.apply_snapshot(&mosaica_lifecycle::overlay::snapshot(
        overlays
            .items
            .iter()
            .map(|(shard, overlay)| (*shard, overlay)),
        overlays
            .spaces
            .iter()
            .map(|(space, overlay)| (*space, overlay)),
    ));
    assert!(restored.spaces[&space].is_suppressed(EntityId::new(5)));
    assert!(restored.items[&one].is_suppressed(EntityId::new(7)));
    assert!(!restored.items[&ShardId::ZERO].touches(EntityId::new(7)));
    let items = shard_zero(restored.clone());
    assert!(!items.touches(EntityId::new(5)));
    assert!(items.is_deleted(EntityId::new(6)));
    assert!(!restored.spaces[&space].touches(EntityId::new(6)));
}

fn item(entity: EntityId) -> Target {
    Target::Item(ShardId::ZERO, entity)
}
