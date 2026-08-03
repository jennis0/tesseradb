//! The overlay snapshot: what rotation must carry forward before it deletes anything.
//!
//! The overlay's only durable home is the WAL, so a rotation that reclaimed a file holding
//! `Change` records without first re-stating them would silently un-deny. These tests pin the
//! three things that make the snapshot a faithful re-statement rather than an approximation of
//! one: that it is keyed by entity rather than by external id, that it carries raw descriptors
//! rather than resolved ids, and that it reproduces the overlay exactly — including entries whose
//! facts are all currently inactive.

use tempfile::TempDir;

use tessera_authz::Dict;
use tessera_lifecycle::overlay::{replay, PredicateChange};
use tessera_lifecycle::wal::{ChangeOp, Wal, WalRecord};
use tessera_lifecycle::{DescriptorResolver, Overlay};
use tessera_types::{EntityId, TermId};

fn empty_dict() -> (Dict, TempDir) {
    let temp = TempDir::new().unwrap();
    let paths = tessera_authz::DictWriter::new(temp.path())
        .finish()
        .unwrap();
    (Dict::load(&paths).unwrap(), temp)
}

fn dict_with(descriptors: &[&[u8]]) -> (Dict, TempDir) {
    let temp = TempDir::new().unwrap();
    let mut writer = tessera_authz::DictWriter::new(temp.path());
    for d in descriptors {
        writer.intern(d);
    }
    let paths = writer.finish().unwrap();
    (Dict::load(&paths).unwrap(), temp)
}

fn round_trip(overlay: &Overlay, dict: &Dict) -> Overlay {
    let entries = overlay.snapshot();
    let mut restored = Overlay::new();
    restored.apply_snapshot(&entries, &mut DescriptorResolver::new(dict));
    restored
}

/// **Keyed by entity, never by external id.** An entity deleted before it was ever flushed has no
/// row and may have no external-id extent entry, so a `Change`-shaped snapshot would answer
/// `UnknownExternalId` at replay and the node would refuse to open — a benign rotation turned into
/// a permanently unopenable node.
#[test]
fn a_deleted_entity_with_no_row_round_trips_the_snapshot() {
    let (dict, _temp) = empty_dict();
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(99), ChangeOp::Delete, None);

    let entries = overlay.snapshot();
    assert_eq!(entries[0].entity_id, EntityId::new(99));

    let restored = round_trip(&overlay, &dict);
    assert!(
        restored.get(EntityId::new(99)).unwrap().deleted,
        "a deletion must survive a rotation with nothing but the snapshot to carry it"
    );
}

/// **Raw descriptors, never `TermId`s.** Extension ids are assigned in replay order, and rotation
/// changes replay order — so a persisted extension id would dangle, pointing at whatever descriptor
/// interns next.
#[test]
fn an_evaluate_entry_snapshots_raw_descriptors_never_term_ids() {
    let (dict, _temp) = empty_dict();
    let mut resolver = DescriptorResolver::new(&dict);
    let mut overlay = Overlay::new();
    overlay.apply(
        EntityId::new(1),
        ChangeOp::Predicate,
        Some(tessera_lifecycle::overlay::resolve(
            &[b"novel".to_vec()],
            &mut resolver,
        )),
    );

    let entries = overlay.snapshot();
    assert_eq!(
        entries[0].descriptors.as_deref(),
        Some(&[b"novel".to_vec()][..]),
    );
}

/// The hazard the previous test's shape exists to prevent, demonstrated end to end.
///
/// The descriptor is novel, so it resolves into the extension range — and the *second* resolver
/// interns an unrelated descriptor first, exactly as a rotation reordering replay would. A snapshot
/// carrying the id would restore `u32::MAX` and the entity would evaluate against `"unrelated"`.
/// Carrying the descriptor, it re-resolves to the id this process now uses for it.
#[test]
fn a_re_resolved_extension_id_follows_the_descriptor_not_the_ordinal() {
    let (dict, _temp) = empty_dict();
    let mut overlay = Overlay::new();
    overlay.apply(
        EntityId::new(1),
        ChangeOp::Predicate,
        Some(tessera_lifecycle::overlay::resolve(
            &[b"novel".to_vec()],
            &mut DescriptorResolver::new(&dict),
        )),
    );
    assert_eq!(
        overlay.get(EntityId::new(1)).unwrap().evaluate_terms(),
        Some(&[TermId::new(u32::MAX)][..]),
    );

    let mut later = DescriptorResolver::new(&dict);
    assert_eq!(later.resolve(b"unrelated"), TermId::new(u32::MAX));
    let mut restored = Overlay::new();
    restored.apply_snapshot(&overlay.snapshot(), &mut later);

    assert_eq!(
        restored.get(EntityId::new(1)).unwrap().evaluate_terms(),
        Some(&[TermId::new(u32::MAX - 1)][..]),
        "the restored entry must name whatever id \"novel\" holds now, not the one it held then"
    );
}

/// A descriptor the dictionary *does* know keeps its durable ordinal across the round trip — the
/// snapshot must not push a real term into the extension range either.
#[test]
fn a_dictionary_term_keeps_its_durable_ordinal_across_the_snapshot() {
    let (dict, _temp) = dict_with(&[b"dept:eng", b"clearance:secret"]);
    let mut overlay = Overlay::new();
    overlay.apply(
        EntityId::new(5),
        ChangeOp::Predicate,
        Some(tessera_lifecycle::overlay::resolve(
            &[b"clearance:secret".to_vec()],
            &mut DescriptorResolver::new(&dict),
        )),
    );

    let restored = round_trip(&overlay, &dict);
    assert_eq!(
        restored.get(EntityId::new(5)).unwrap().evaluate_terms(),
        Some(&[TermId::new(1)][..]),
    );
}

/// **A present-but-neutral entry is not the same as no entry.** `compose`'s verdict rule gives any
/// overlay entry precedence over the ingest buffer, so an entry lost to a snapshot would let a
/// still-buffered entity's own terms start deciding its visibility — fail-open, and discoverable
/// only after a rotation.
#[test]
fn a_neutral_entry_survives_the_snapshot() {
    let (dict, _temp) = empty_dict();
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(3), ChangeOp::Suppress, None);
    overlay.apply(EntityId::new(3), ChangeOp::Unsuppress, None);

    let restored = round_trip(&overlay, &dict);
    let entry = restored
        .get(EntityId::new(3))
        .expect("the entry itself must survive, even with no fact in force");
    assert!(!entry.deleted && !entry.suppressed && entry.evaluate.is_none());
}

/// The three facts are independent, so a snapshot has to carry all of them — a `delete` folded into
/// a single "current disposition" would drop the suppression, and `suppress → unsuppress` on a
/// deleted entity would then re-expose it.
#[test]
fn all_three_facts_survive_together() {
    let (dict, _temp) = empty_dict();
    let mut resolver = DescriptorResolver::new(&dict);
    let mut overlay = Overlay::new();
    let e = EntityId::new(11);
    overlay.apply(e, ChangeOp::Delete, None);
    overlay.apply(e, ChangeOp::Suppress, None);
    overlay.apply(
        e,
        ChangeOp::Predicate,
        Some(tessera_lifecycle::overlay::resolve(
            &[b"a".to_vec(), b"b".to_vec()],
            &mut resolver,
        )),
    );

    let restored = round_trip(&overlay, &dict);
    let entry = restored.get(e).unwrap();
    assert!(entry.deleted);
    assert!(entry.suppressed);
    assert_eq!(entry.evaluate.as_ref().unwrap().descriptors.len(), 2);
}

/// The same overlay must encode to the same bytes, whatever order its hash map happens to iterate
/// in — a record whose bytes depend on allocation history cannot be compared or re-encoded by
/// `Wal::retry_durability` with any confidence.
#[test]
fn a_snapshot_is_ordered_by_entity_id() {
    let mut overlay = Overlay::new();
    for id in [900u64, 3, 47, 1, 12] {
        overlay.apply(EntityId::new(id), ChangeOp::Suppress, None);
    }
    let ids: Vec<u64> = overlay
        .snapshot()
        .iter()
        .map(|e| e.entity_id.raw())
        .collect();
    assert_eq!(ids, vec![1, 3, 12, 47, 900]);
}

/// The record survives the log, and — the part rotation actually depends on — replay applies it
/// **at the position it occupies**, so a `Change` earlier in the same file still lands first.
///
/// Here that ordering is what keeps the deletion: the snapshot was taken before entity 8 was
/// deleted, so a recovery that treated the snapshot as its starting state and resumed *after* it
/// would be correct, while one that used it as a starting state and skipped what precedes it would
/// silently un-delete. The `Change` below the snapshot is the case that distinguishes them.
#[test]
fn a_snapshot_replays_in_position_and_never_displaces_what_precedes_it() {
    let (dict, _temp) = empty_dict();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal.log");

    let mut before = Overlay::new();
    before.apply(EntityId::new(7), ChangeOp::Suppress, None);

    {
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&WalRecord::Change {
            external_id: b"ext-8".to_vec(),
            op: ChangeOp::Delete,
            descriptors: None,
        })
        .unwrap();
        wal.append(&WalRecord::OverlaySnapshot {
            entries: before.snapshot(),
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let (_wal, records) = Wal::open(&path).unwrap();
    let (overlay, _buffer, _established, _resolver) = replay(&records, &dict, |ext| {
        Ok::<_, std::convert::Infallible>(if ext == b"ext-8" {
            Some(EntityId::new(8))
        } else {
            None
        })
    })
    .unwrap();

    assert!(
        overlay.get(EntityId::new(7)).unwrap().suppressed,
        "the snapshot's own entries must apply"
    );
    assert!(
        overlay.get(EntityId::new(8)).unwrap().deleted,
        "a Change below the snapshot must not be displaced by it"
    );
}

/// A snapshot folds into an overlay that already holds state, rather than replacing it: replay is
/// one left-to-right pass and the snapshot is a record in it.
#[test]
fn a_snapshot_unions_with_state_already_applied() {
    let (dict, _temp) = empty_dict();
    let mut snapshotted = Overlay::new();
    snapshotted.apply(EntityId::new(1), ChangeOp::Delete, None);

    let mut live = Overlay::new();
    live.apply(EntityId::new(2), ChangeOp::Suppress, None);
    live.apply_snapshot(&snapshotted.snapshot(), &mut DescriptorResolver::new(&dict));

    assert!(live.get(EntityId::new(1)).unwrap().deleted);
    assert!(
        live.get(EntityId::new(2)).unwrap().suppressed,
        "applying a snapshot must not discard what was already there"
    );
}

/// A descriptor-less `Predicate` (`access` is optional on `/control/changes`) survives as the
/// empty, always-unsatisfiable term set it was folded to — never as "no predicate at all", which
/// would fall back to the fragment's original verdict and could re-expose the entity.
#[test]
fn a_descriptor_less_predicate_stays_fail_closed_across_the_snapshot() {
    let (dict, _temp) = empty_dict();
    let mut overlay = Overlay::new();
    overlay.apply(EntityId::new(4), ChangeOp::Predicate, None);
    assert_eq!(
        overlay.get(EntityId::new(4)).unwrap().evaluate,
        Some(PredicateChange::default()),
    );

    let restored = round_trip(&overlay, &dict);
    assert_eq!(
        restored.get(EntityId::new(4)).unwrap().evaluate_terms(),
        Some(&[][..]),
        "an empty term set intersects nothing; an absent one falls back to the fragment"
    );
}
