//! The overlay: per-entity deny/evaluate state accumulated from `/control/changes` (lifecycle
//! §3.1's three retirement rules).
//!
//! `OverlayEntry` carries **three independent facts**, never one overwritable disposition:
//! `deleted`, `suppressed` and `evaluate_terms` retire on entirely different triggers. Deletion
//! denies retire via the stamp ledger; suppressions retire only on `Unsuppress`; predicate changes
//! retire at their compaction fold (lifecycle §3).
//! **⊘ Partially implemented:** only the `Unsuppress` rule exists. There is no stamp ledger and no
//! compaction fold, so nothing retires a deletion or a predicate change — safe today precisely
//! because nothing retires at all, and fail-open the moment either is built without its own rule.
//!
//! Collapsing the three into a single enum ("last write wins") is fail-open, and has been caught
//! twice: the sequence `delete → suppress → unsuppress` must not re-expose a deleted item. Only
//! three independent booleans/options — each cleared by nothing but its own opposite operation, or
//! for `deleted` by nothing at all — make that structurally impossible rather than merely
//! tested-against.

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_types::{EntityId, TermId};

use crate::buffer::{DescriptorResolver, IngestBuffer};
use crate::wal::{ChangeOp, OverlaySnapshotEntry, WalRecord};

/// A predicate change's term set, carried in **both** the form composition needs and the form
/// durability needs.
///
/// The two are one value rather than two fields because they must never drift: `terms` is
/// process-local (a novel descriptor resolves to an extension id that exists only in this
/// process's [`DescriptorResolver`]), so it is unwritable, while `descriptors` is what
/// [`Overlay::snapshot`] must emit and is meaningless to composition. An overlay that stored only
/// the resolved ids could not be snapshotted at all; one that stored them separately could be set
/// with a mismatched pair. See [`OverlaySnapshotEntry`] for why a persisted `TermId` dangles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PredicateChange {
    /// Raw term descriptors, exactly as `/control/changes` supplied them.
    pub descriptors: Vec<Vec<u8>>,
    /// Those descriptors resolved against this process's dictionary plus extension.
    pub terms: Vec<TermId>,
}

/// One entity's accumulated disposition. Default (all `false`/`None`) is "untouched" — never
/// constructed as a stand-in for "not deleted", only ever the actual absence of any change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayEntry {
    /// Set by `Delete`. **Terminal: nothing clears it**, because the stamp ledger that would
    /// retire a deletion deny does not exist (⊘).
    pub deleted: bool,
    /// Set by `Suppress`; cleared **only** by `Unsuppress`. Never touched by `Delete` or
    /// `Predicate`.
    pub suppressed: bool,
    /// Set by `Predicate`. Replaces the fragment's verdict for this entity in both directions
    /// once present; never cleared by `Delete`/`Suppress`/`Unsuppress`.
    pub evaluate: Option<PredicateChange>,
}

impl OverlayEntry {
    /// The resolved terms of this entity's predicate change, if it has one.
    pub fn evaluate_terms(&self) -> Option<&[TermId]> {
        self.evaluate.as_ref().map(|p| p.terms.as_slice())
    }

    /// Whether any of the three facts is currently in force. A **neutral** entry (all three
    /// inactive, e.g. after `suppress → unsuppress`) is not the same as no entry at all — see
    /// [`Overlay::snapshot`].
    fn is_neutral(&self) -> bool {
        !self.deleted && !self.suppressed && self.evaluate.is_none()
    }
}

/// Accumulated overlay state, keyed by (internal) `EntityId`. Never keyed by external id —
/// external-id resolution happens once, at replay/accept time (see [`replay`]), so the hot
/// composition path (`tessera_engine::compose`) never has to resolve identity.
/// `Clone` because the live `/control/changes` acceptance path builds the
/// next generation's overlay by cloning the current one and applying the newly-accepted change —
/// see [`IngestBuffer`](crate::IngestBuffer)'s doc for why a clone-and-replace, not an in-place
/// mutation, is what the `ArcSwap`-snapshot design requires.
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    entries: FxHashMap<EntityId, OverlayEntry>,
}

impl Overlay {
    pub fn new() -> Self {
        Overlay {
            entries: FxHashMap::default(),
        }
    }

    /// The entry for `entity`, if any change has ever touched it.
    pub fn get(&self, entity: EntityId) -> Option<&OverlayEntry> {
        self.entries.get(&entity)
    }

    /// Iterate every entity this overlay has an opinion on (a superset of entities with an
    /// *active* deny — an entry can be present but currently neutral, e.g. after
    /// `suppress → unsuppress`).
    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &OverlayEntry)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Apply one disposition change to `entity`. The three facts are updated independently —
    /// see this module's doc. `predicate` is used only for `ChangeOp::Predicate`; ignored (should
    /// be `None`) for the other three ops.
    ///
    /// **`Predicate` always *sets* `evaluate_terms` to `Some(_)`, never `None`** — `access` is
    /// optional on `/control/changes` (contracts §3.4), so a `predicate` change with no
    /// descriptors is a representable, reachable request; treating it as "leave `evaluate_terms` unset" would be
    /// fail-open in exactly the dangerous direction: a prior `predicate` that excluded this
    /// entity (an unsatisfied term set) would be silently undone by a later, descriptor-less
    /// `predicate`, falling back to the fragment's original verdict and potentially re-exposing
    /// it. `terms: None` here is therefore folded to `Some(Vec::new())` — a term set that can
    /// never intersect any `satisfied` set, i.e. the entity stays excluded, matching "sets
    /// `evaluate_terms`", never "unsets" it — a case the contract does not define, and for which
    /// this method therefore refuses to invent a permissive answer.
    pub fn apply(&mut self, entity: EntityId, op: ChangeOp, predicate: Option<PredicateChange>) {
        let entry = self.entries.entry(entity).or_default();
        match op {
            ChangeOp::Delete => entry.deleted = true,
            ChangeOp::Suppress => entry.suppressed = true,
            ChangeOp::Unsuppress => entry.suppressed = false,
            ChangeOp::Predicate => entry.evaluate = Some(predicate.unwrap_or_default()),
        }
    }

    /// Re-state this overlay as WAL records, so that the `Change` records it was accumulated from
    /// can be deleted (lifecycle §4's rotation; [`WalRecord::OverlaySnapshot`]).
    ///
    /// **The snapshot reproduces the overlay exactly, not merely its active denies.** An entity
    /// with an entry that is currently *neutral* — `suppress → unsuppress`, nothing else — still
    /// gets one entry, an `Unsuppress`, because a present-but-neutral entry is not the same as no
    /// entry: `tessera_engine::compose`'s verdict rule gives any overlay entry precedence over the
    /// ingest buffer, so dropping a neutral one would change the answer for an entity that is still
    /// buffered. That is a fail-*open* difference in the case it arises — the buffered row's own
    /// terms would start deciding — and it would arise only after a rotation, which is the worst
    /// possible place to discover it.
    ///
    /// **Entries are ordered by entity id**, so the same overlay always encodes to the same bytes.
    /// A `FxHashMap`'s iteration order is not stable across processes, and a record whose bytes
    /// depend on allocation history is one that cannot be compared, re-derived, or re-encoded by
    /// [`crate::Wal::retry_durability`] with any confidence.
    ///
    /// Within one entity the three facts are emitted `Delete`, `Suppress`, `Predicate`. The order
    /// is immaterial — that is the point of three independent facts — but a fixed one is what makes
    /// the bytes a function of the state alone.
    pub fn snapshot(&self) -> Vec<OverlaySnapshotEntry> {
        let mut entities: Vec<&EntityId> = self.entries.keys().collect();
        entities.sort_unstable();

        let mut out = Vec::with_capacity(entities.len());
        for entity_id in entities {
            let entry = &self.entries[entity_id];
            let mut push = |op, descriptors| {
                out.push(OverlaySnapshotEntry {
                    entity_id: *entity_id,
                    op,
                    descriptors,
                })
            };
            if entry.is_neutral() {
                // The one op whose effect on a default entry is to establish it and leave every
                // fact inactive — see this method's doc for why the entry must survive at all.
                push(ChangeOp::Unsuppress, None);
                continue;
            }
            if entry.deleted {
                push(ChangeOp::Delete, None);
            }
            if entry.suppressed {
                push(ChangeOp::Suppress, None);
            }
            if let Some(predicate) = &entry.evaluate {
                push(ChangeOp::Predicate, Some(predicate.descriptors.clone()));
            }
        }
        out
    }

    /// Fold a snapshot back in, resolving each predicate's descriptors through `resolver` exactly
    /// as a live `Change` would.
    ///
    /// Applied, never assigned: the snapshot is a record in a position, and a `Change` earlier in
    /// the same file has already been applied when this runs. Replacing the map instead would
    /// discard those.
    pub fn apply_snapshot(
        &mut self,
        entries: &[OverlaySnapshotEntry],
        resolver: &mut DescriptorResolver<'_>,
    ) {
        for entry in entries {
            self.apply(
                entry.entity_id,
                entry.op,
                entry.descriptors.as_ref().map(|ds| resolve(ds, resolver)),
            );
        }
    }
}

/// Resolve a change's raw descriptors, keeping both forms together — see [`PredicateChange`].
pub fn resolve(descriptors: &[Vec<u8>], resolver: &mut DescriptorResolver<'_>) -> PredicateChange {
    PredicateChange {
        terms: descriptors.iter().map(|d| resolver.resolve(d)).collect(),
        descriptors: descriptors.to_vec(),
    }
}

/// Replay failures. Fail-closed: a change naming an external id this
/// replay has never seen — neither via an `IngestBatch` row replayed so far, nor via
/// `resolve_from_bundle` (the bundle's `entities/external-ids-0.arrow` extent, wired in by the
/// caller) — must not be silently dropped or silently applied to the wrong entity.
///
/// Generic over `E`, the caller's `resolve_from_bundle` error type.
/// `tessera-lifecycle` does not depend on `tessera-store`, so this cannot name
/// `StoreError` directly — `tessera-engine`'s caller instantiates `E = StoreError` and gets a
/// real propagated error instead of the closure panicking on a corrupt sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError<E> {
    /// `404 unknown`: a `Change` record named an external id no known
    /// entity (bundle or WAL-established) has ever claimed.
    UnknownExternalId(Vec<u8>),
    /// `resolve_from_bundle` itself failed — a real error, not "not found". Fail-closed: WAL
    /// replay (and any live resolution reusing the same closure) must propagate this rather than
    /// read it as `UnknownExternalId`, which would let a WAL-resident suppression silently fail
    /// to apply.
    ResolveFailed(E),
}

impl<E: std::fmt::Display> std::fmt::Display for OverlayError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::UnknownExternalId(id) => {
                write!(f, "unknown external id: {id:02x?}")
            }
            OverlayError::ResolveFailed(e) => {
                write!(f, "resolve_from_bundle failed: {e}")
            }
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for OverlayError<E> {}

/// Replay a WAL's records into an `Overlay` and an `IngestBuffer`.
///
/// Single left-to-right pass over `records`, in on-disk order (the order changes and ingests
/// were accepted in — WAL append order is causal order):
/// - `IngestBatch` rows populate `IngestBuffer` (term descriptors resolved via `dict` plus a
///   deterministic, replay-order in-memory extension — see [`DescriptorResolver`]) and register
///   `external_id → entity_id` in this replay's own external-id map, so a later `Change` record
///   in the *same* WAL naming that external id resolves without touching the bundle.
/// - `Change` records resolve their `external_id` first against this replay's own map, then
///   against `resolve_from_bundle` (entities established before this WAL — i.e. present in the
///   bundle's `entities/external-ids-0.arrow` extent, contracts §2.1); an id found in neither is
///   `OverlayError::UnknownExternalId` (`404 unknown`) — fail-closed, never silently ignored.
///   `Predicate`'s descriptors are resolved through the same `DescriptorResolver` as ingest rows.
/// - `OverlaySnapshot` records are applied **at the position they occupy**, never used as a
///   starting state the walk then resumes from. Those are different algorithms: recovery walks
///   every surviving file in sequence order, and a file older than the one holding the snapshot may
///   still carry `Change` records above the point the snapshot was taken at. Starting *at* the
///   snapshot would skip them — which looks like an optimisation and is a silent un-deny.
/// - `Lease` records carry no overlay/buffer information (I9 allocator bookkeeping only) and are
///   skipped here.
///
/// Returns, alongside the overlay and buffer, the `external_id -> entity_id` map this replay
/// established from `IngestBatch` rows, and the `DescriptorResolver` in its final state — both
/// borrowed from `dict` for exactly as long as this call. `Engine::open` immediately
/// detaches them (`.into_state()`) into owned data it keeps for the process's lifetime, so a live
/// `/control/ingest` or `/control/changes` acceptance can keep resolving external ids and novel
/// descriptors from exactly where replay left off, rather than restarting either sequence (see
/// [`DescriptorResolver::resume`]'s doc for why restarting descriptor extension ids would be
/// fail-open).
#[allow(clippy::type_complexity)]
pub fn replay<'a, E>(
    records: &[WalRecord],
    dict: &'a Dict,
    resolve_from_bundle: impl Fn(&[u8]) -> std::result::Result<Option<EntityId>, E>,
) -> Result<
    (
        Overlay,
        IngestBuffer,
        FxHashMap<Vec<u8>, EntityId>,
        DescriptorResolver<'a>,
    ),
    OverlayError<E>,
> {
    let mut overlay = Overlay::new();
    let mut buffer = IngestBuffer::new();
    let mut resolver = DescriptorResolver::new(dict);
    let mut established: FxHashMap<Vec<u8>, EntityId> = FxHashMap::default();

    for record in records {
        match record {
            WalRecord::IngestBatch { rows, .. } => {
                for row in rows {
                    // Contracts §3.4 r6: no external id means no sidecar entry and nothing to
                    // establish here either — the item is addressable only by its `tessera_id`.
                    if let Some(external_id) = &row.external_id {
                        established.insert(external_id.clone(), row.entity_id);
                    }
                    buffer.insert_row(row, &mut resolver);
                }
            }
            WalRecord::Change {
                external_id,
                op,
                descriptors,
            } => {
                let entity = match established.get(external_id.as_slice()).copied() {
                    Some(entity) => entity,
                    None => match resolve_from_bundle(external_id)
                        .map_err(OverlayError::ResolveFailed)?
                    {
                        Some(entity) => entity,
                        None => return Err(OverlayError::UnknownExternalId(external_id.clone())),
                    },
                };

                let predicate = descriptors.as_ref().map(|ds| resolve(ds, &mut resolver));
                overlay.apply(entity, *op, predicate);
            }
            WalRecord::OverlaySnapshot { entries } => {
                overlay.apply_snapshot(entries, &mut resolver);
            }
            WalRecord::Lease { .. } => {}
        }
    }

    Ok((overlay, buffer, established, resolver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::ChangeOp;

    /// A predicate change stated the way [`Overlay::apply`] requires: descriptors and the terms
    /// they resolve to, together. These tests never resolve, so the correspondence is nominal —
    /// what matters is that a `PredicateChange` cannot be built with one half missing.
    fn predicate(pairs: &[(&[u8], u32)]) -> PredicateChange {
        PredicateChange {
            descriptors: pairs.iter().map(|(d, _)| d.to_vec()).collect(),
            terms: pairs.iter().map(|(_, t)| TermId::new(*t)).collect(),
        }
    }

    #[test]
    fn three_facts_are_independent() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(1);

        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Suppress, None);
        overlay.apply(e, ChangeOp::Unsuppress, None);

        let entry = overlay.get(e).unwrap();
        assert!(
            entry.deleted,
            "delete is terminal; unsuppress must not clear it"
        );
        assert!(!entry.suppressed, "unsuppress clears suppressed only");
    }

    #[test]
    fn unsuppress_never_clears_deleted_reverse_order() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(2);

        overlay.apply(e, ChangeOp::Suppress, None);
        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Unsuppress, None);

        let entry = overlay.get(e).unwrap();
        assert!(entry.deleted);
        assert!(!entry.suppressed);
    }

    #[test]
    fn predicate_never_clears_deny_flags() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(3);

        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Predicate, Some(predicate(&[(b"nine", 9)])));

        let entry = overlay.get(e).unwrap();
        assert!(entry.deleted);
        assert_eq!(entry.evaluate_terms(), Some(&[TermId::new(9)][..]));
    }

    /// A `predicate` change with no descriptors (`access` is optional)
    /// must not silently clear a prior evaluate verdict — that would be fail-open (an entity
    /// excluded by an earlier unsatisfied predicate re-exposed by a later, descriptor-less one).
    #[test]
    fn predicate_with_no_terms_does_not_clear_a_prior_evaluate_verdict() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(4);

        // First predicate: an unsatisfied term set (excludes the entity).
        overlay.apply(
            e,
            ChangeOp::Predicate,
            Some(predicate(&[(b"seventy-seven", 77)])),
        );
        assert_eq!(
            overlay.get(e).unwrap().evaluate_terms(),
            Some(&[TermId::new(77)][..])
        );

        // A later predicate change with no descriptors at all (`terms: None`) must not unset
        // `evaluate_terms` back to `None` — that would fall back to the fragment's original
        // verdict, which may have included this entity.
        overlay.apply(e, ChangeOp::Predicate, None);
        let entry = overlay.get(e).unwrap();
        assert_eq!(
            entry.evaluate_terms(),
            Some(&[][..]),
            "a descriptor-less predicate must still set evaluate_terms, to an empty (always \
             fail-closed) set — never leave it unset"
        );
    }

    #[test]
    fn replay_reports_unknown_external_id_as_404() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let records = vec![WalRecord::Change {
            external_id: b"never-ingested".to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        }];

        let result = replay(&records, &dict, |_external_id| {
            Ok::<_, std::convert::Infallible>(None)
        });
        assert_eq!(
            result.unwrap_err(),
            OverlayError::UnknownExternalId(b"never-ingested".to_vec())
        );
    }

    /// **A disposition is idempotent under replay, and this is where that is established rather
    /// than assumed.**
    ///
    /// The deny lane's durability retry (`tessera-engine`'s `Executor::retry_deny_durability`)
    /// re-writes a window's records in place, so it leaves one copy — but it is safe to re-write
    /// only because replaying a disposition twice is indistinguishable from replaying it once, and
    /// that is a property of *this* function, not of the retry. Any future repair that appends a
    /// second copy instead of rewinding depends on it directly.
    ///
    /// Three pieces of replayed state could have carried the difference, and each is checked here or
    /// named: the **overlay** (below, by comparing against the single-copy replay), the
    /// **external-id map** (below — `Change` records never write to it; only `IngestBatch` rows do),
    /// and the **allocator seed**, which `high_water_from` derives from `IngestBatch` and `Lease`
    /// records alone — pinned by `alloc.rs`'s `high_water_from_takes_the_max_of_rows_and_leases`.
    #[test]
    fn a_disposition_replayed_twice_is_the_same_as_replayed_once() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let entity = EntityId::new(7);
        let suppress = WalRecord::Change {
            external_id: b"twice".to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        };
        let resolve = |external_id: &[u8]| {
            Ok::<_, std::convert::Infallible>(if external_id == b"twice" {
                Some(entity)
            } else {
                None
            })
        };

        let (once, _, established_once, _) =
            replay(std::slice::from_ref(&suppress), &dict, resolve).unwrap();
        let (twice, _, established_twice, _) =
            replay(&[suppress.clone(), suppress], &dict, resolve).unwrap();

        assert_eq!(
            once.get(entity),
            twice.get(entity),
            "a second copy of a disposition record must fold to the same overlay entry — the \
             durability retry's safety net rests on exactly this"
        );
        assert!(
            twice.get(entity).unwrap().suppressed,
            "and the entry must actually be a suppression, or this compares two absences"
        );
        assert_eq!(
            (established_once.len(), established_twice.len()),
            (0, 0),
            "a Change record establishes no external id, so a second copy cannot double-register one"
        );
    }

    #[test]
    fn replay_resolves_change_against_bundle_when_not_established_by_this_wal() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let bundle_entity = EntityId::new(42);
        let records = vec![WalRecord::Change {
            external_id: b"from-a-previous-build".to_vec(),
            op: ChangeOp::Delete,
            descriptors: None,
        }];

        let (overlay, _buffer, _established, _resolver) = replay(&records, &dict, |external_id| {
            Ok::<_, std::convert::Infallible>(if external_id == b"from-a-previous-build" {
                Some(bundle_entity)
            } else {
                None
            })
        })
        .unwrap();

        assert!(overlay.get(bundle_entity).unwrap().deleted);
    }

    /// Contracts §3.4 r6: an ingested item with no external id gets no `established` entry at
    /// all — two such items in the same batch must not collide with each other (`None` is not a
    /// key that can be inserted twice and clobber itself), and both rows must still land in the
    /// buffer.
    #[test]
    fn two_null_external_ids_in_one_batch_do_not_collide() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let records = vec![WalRecord::IngestBatch {
            batch_id: "b".to_string(),
            body_hash: [0u8; 32],
            rows: vec![
                crate::wal::WalRow {
                    external_id: None,
                    entity_id: EntityId::new(100),
                    slice: "default".to_string(),
                    descriptors: Vec::new(),
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                },
                crate::wal::WalRow {
                    external_id: None,
                    entity_id: EntityId::new(101),
                    slice: "default".to_string(),
                    descriptors: Vec::new(),
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                },
            ],
        }];

        let (_overlay, buffer, established, _resolver) = replay(&records, &dict, |_external_id| {
            Ok::<_, std::convert::Infallible>(None)
        })
        .unwrap();

        assert!(
            established.is_empty(),
            "no external id was supplied, so nothing should be established"
        );
        assert_eq!(buffer.len(), 2, "both null-external-id rows still buffer");
    }
}
