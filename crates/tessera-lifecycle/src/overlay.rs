//! The overlay: per-entity deny/evaluate state accumulated from `/control/changes` (lifecycle
//! §3.1's three retirement rules).
//!
//! The overlay is **three independent stores**, never one overwritable disposition: deletions,
//! suppressions and predicate changes retire on entirely different triggers. Deletion denies retire
//! via the stamp ledger; suppressions retire only on `Unsuppress`; predicate changes retire at
//! their compaction fold (lifecycle §3).
//! **⊘ Partially implemented:** only the `Unsuppress` rule exists. There is no stamp ledger and no
//! compaction fold, so nothing retires a deletion or a predicate change — safe today precisely
//! because nothing retires at all, and fail-open the moment either is built without its own rule.
//!
//! Collapsing them into a single enum ("last write wins") is fail-open, and has been caught twice:
//! the sequence `delete → suppress → unsuppress` must not re-expose a deleted item. Three separate
//! containers, each written by exactly one op, make that impossible for any refactor that still
//! type-checks — where three fields in one struct left it a rule a reader had to keep in mind.

use croaring::Bitmap;
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

/// Per-entity deny/evaluate state, held as **three independent stores, one per retirement rule**.
///
/// The three facts retire on entirely different triggers — deletion denies by the stamp ledger,
/// suppressions *only* on unsuppress, predicate changes at their compaction fold (lifecycle §3.1) —
/// and the rejected single rule (r1: one retirement stamp for every deny) is fail-open precisely
/// for suppression, because any stamp eventually retires the entry and re-exposes the item.
///
/// **Three stores rather than three fields in one struct, and the difference is not cosmetic.**
/// Collapsing the facts into one last-write-wins disposition was caught fail-open twice in review;
/// the counterexample is `delete → suppress → unsuppress`. Three fields made that a rule a refactor
/// could still break while compiling. Three containers of three different types, mutated in three
/// places, cannot be collapsed by any refactor that still type-checks. It also makes r1
/// *unexpressible* rather than merely rejected: [`Overlay::suppressed`] is a bitmap of entity ids
/// and carries no stamp field for a retirement rule to act on at all. Its only removal path is an
/// `Unsuppress`.
///
/// The suppression and deletion sets are Roaring bitmaps because that is what they are — sets of
/// entity ids, in entity space, where a set is the whole content. Predicate changes carry a term
/// set per entity and stay a map.
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    /// Set by `Delete`. **Terminal: nothing removes from it**, because the stamp ledger that would
    /// retire a deletion deny does not exist (⊘).
    deleted: Bitmap,
    /// Set by `Suppress`, cleared **only** by `Unsuppress`. Never touched by `Delete` or
    /// `Predicate`, which is now true by construction rather than by discipline.
    suppressed: Bitmap,
    /// Set by `Predicate`. Replaces the fragment's verdict for this entity in both directions once
    /// present; never cleared by the other three ops (⊘ — the compaction fold that would retire it
    /// does not exist).
    evaluate: FxHashMap<EntityId, PredicateChange>,
}

impl Overlay {
    pub fn new() -> Self {
        Overlay::default()
    }

    pub fn is_deleted(&self, entity: EntityId) -> bool {
        self.deleted.contains(as_u32(entity))
    }

    pub fn is_suppressed(&self, entity: EntityId) -> bool {
        self.suppressed.contains(as_u32(entity))
    }

    pub fn evaluate_of(&self, entity: EntityId) -> Option<&PredicateChange> {
        self.evaluate.get(&entity)
    }

    /// Whether any of the three stores holds an opinion about `entity`.
    ///
    /// **There is no "present but neutral" state any more.** Under a single map, `suppress →
    /// unsuppress` left an entry whose every fact was inactive, and its mere *presence* outranked
    /// the ingest buffer in `compose`'s verdict rule. An unsuppress now removes the id from the
    /// suppression bitmap, so the entity is untouched again and the buffer decides — which is what
    /// lifecycle §3.1 always said ("unsuppress removes the entry") and what the code did not do.
    pub fn touches(&self, entity: EntityId) -> bool {
        self.is_deleted(entity) || self.is_suppressed(entity) || self.evaluate.contains_key(&entity)
    }

    /// `deleted ∪ suppressed` — every entity denied outright, in entity space.
    ///
    /// **The one input to the row-space deny mask** (`Generation::denied`), and the reason it is a
    /// union rather than two accessors: the mask's derivation rule turns on the union, so an
    /// unsuppress may not subtract a row while `deleted` still holds the entity. Handing callers
    /// the union makes the rule the only expressible thing.
    ///
    /// The two predicate-change stores are deliberately absent: an `evaluate` entry is not a deny,
    /// it replaces a verdict in both directions, and it stays in `compose`'s per-entity walk.
    pub fn denied(&self) -> Bitmap {
        self.deleted.or(&self.suppressed)
    }

    /// The suppression set, ascending — `SEGMENTS-<n>.json`'s `deny` field.
    ///
    /// Separate from [`Self::deleted_entities`] because the two manifest fields mean different
    /// things and retire under different rules (lifecycle §3): a suppression leaves only by its
    /// unsuppress, a tombstone only at the fold that executes it. [`Self::denied`] deliberately
    /// hands out only the union, which is right for the row mask and wrong here — a writer that
    /// published the union under one field would make every deletion look retirable by an
    /// unsuppress.
    pub fn suppressed_entities(&self) -> Vec<u64> {
        self.suppressed.iter().map(u64::from).collect()
    }

    /// The deleted set, ascending — `SEGMENTS-<n>.json`'s `tombstones` field. See
    /// [`Self::suppressed_entities`].
    pub fn deleted_entities(&self) -> Vec<u64> {
        self.deleted.iter().map(u64::from).collect()
    }

    /// The entities carrying a predicate change — with the buffer, the whole of what `compose`
    /// still walks per request once the deny mask covers the rest.
    pub fn evaluate_keys(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.evaluate.keys().copied()
    }

    /// Every entity any store has an opinion on, ascending, without duplicates.
    pub fn touched(&self) -> Vec<EntityId> {
        let mut ids = self.deleted.or(&self.suppressed);
        for entity in self.evaluate.keys() {
            ids.add(as_u32(*entity));
        }
        ids.iter().map(|id| EntityId::new(id as u64)).collect()
    }

    /// How many entities this overlay has an opinion on — the depth the soft-limit alarm gauges.
    ///
    /// **This can now go down.** An unsuppress genuinely removes an id, so the one disposition with
    /// a retirement rule that exists is the one the gauge can reflect. The other two never shrink
    /// (⊘), so the depth's floor is the deletion and predicate sets.
    pub fn len(&self) -> usize {
        let mut ids = self.deleted.or(&self.suppressed);
        for entity in self.evaluate.keys() {
            ids.add(as_u32(*entity));
        }
        ids.cardinality() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty() && self.suppressed.is_empty() && self.evaluate.is_empty()
    }

    /// Apply one disposition change to `entity`. Each op touches exactly one store — see this
    /// type's doc for why that is the whole safety argument.
    ///
    /// **`Predicate` always *sets* a term set, never unsets one.** `access` is optional on
    /// `/control/changes` (contracts §3.4), so a predicate change with no descriptors is a
    /// representable, reachable request; treating it as "leave the entry alone" would be fail-open
    /// in the dangerous direction — a prior predicate that excluded this entity would be silently
    /// undone, falling back to the fragment's original verdict. `None` therefore folds to an empty
    /// term set, which intersects no session's `satisfied` and so keeps the entity excluded.
    pub fn apply(&mut self, entity: EntityId, op: ChangeOp, predicate: Option<PredicateChange>) {
        match op {
            ChangeOp::Delete => {
                self.deleted.add(as_u32(entity));
            }
            ChangeOp::Suppress => {
                self.suppressed.add(as_u32(entity));
            }
            ChangeOp::Unsuppress => {
                self.suppressed.remove(as_u32(entity));
            }
            ChangeOp::Predicate => {
                self.evaluate.insert(entity, predicate.unwrap_or_default());
            }
        }
    }

    /// Re-state this overlay as WAL records, so the `Change` records it was accumulated from can be
    /// deleted (lifecycle §4's rotation; [`WalRecord::OverlaySnapshot`]).
    ///
    /// **Entries are ordered by entity id**, so the same overlay always encodes to the same bytes.
    /// A record whose bytes depend on a hash map's iteration order is one that cannot be compared,
    /// re-derived, or re-encoded by [`crate::Wal::retry_durability`] with any confidence.
    ///
    /// There is no neutral-entry rule here any more, and its absence is the point: under one map a
    /// `suppress → unsuppress` left a husk whose presence was load-bearing, so the snapshot had to
    /// emit an `Unsuppress` to preserve it. Three stores make the husk unrepresentable.
    pub fn snapshot(&self) -> Vec<OverlaySnapshotEntry> {
        let mut out = Vec::with_capacity(self.len());
        for entity in self.touched() {
            if self.is_deleted(entity) {
                out.push(OverlaySnapshotEntry {
                    entity_id: entity,
                    op: ChangeOp::Delete,
                    descriptors: None,
                });
            }
            if self.is_suppressed(entity) {
                out.push(OverlaySnapshotEntry {
                    entity_id: entity,
                    op: ChangeOp::Suppress,
                    descriptors: None,
                });
            }
            if let Some(predicate) = self.evaluate_of(entity) {
                out.push(OverlaySnapshotEntry {
                    entity_id: entity,
                    op: ChangeOp::Predicate,
                    descriptors: Some(predicate.descriptors.clone()),
                });
            }
        }
        out
    }

    /// Fold a snapshot back in, resolving each predicate's descriptors through `resolver` exactly
    /// as a live `Change` would.
    ///
    /// Applied, never assigned: the snapshot is a record in a position, and a `Change` earlier in
    /// the same file has already been applied when this runs.
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

/// Entity ids are capped at `u32::MAX` by the I9 allocator (contracts §2.6 r6), which is what lets
/// the deny sets be Roaring bitmaps at all.
fn as_u32(entity: EntityId) -> u32 {
    u32::try_from(entity.raw())
        .expect("entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)")
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
/// - `Lease` and `Flush` records carry no overlay/buffer information — I9 allocator bookkeeping
///   and a reclamation authority respectively — and are skipped here.
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
    seed: Overlay,
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
    // **The seed is the starting state, and replay runs over it — that order is load-bearing.**
    // `seed` is what the partition manifests carry (`initial_deny_of`); every WAL record postdates
    // it, because a manifest is only ever written above WAL durability and a member is only
    // reclaimed after a manifest reflecting it is durable at a higher `n`. So a later record must
    // win, and the one op that needs it to is `Unsuppress`: publication is deliberately off the ack
    // path, so there is always a gap in which the newest manifest predates a durable, acked
    // unsuppress. Seeding *after* replay would re-apply the retired suppression on every restart in
    // that gap, and the next manifest write would make the reversion permanent — an acked
    // disposition silently reverted. Fail-closed in direction (an item hidden, never leaked), but a
    // violation of what the 200 asserts.
    //
    // Deletes are indifferent: nothing un-sets them, so either order gives the same answer. The
    // idempotency the previous ordering rested on is untouched — applying a disposition twice still
    // folds to the same state.
    let mut overlay = seed;
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
            // Neither carries overlay or buffer information: `Lease` is I9 allocator bookkeeping
            // and `Flush` is a reclamation authority read by the WAL sequence, not by this walk.
            WalRecord::Lease { .. } | WalRecord::Flush { .. } => {}
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

        assert!(
            overlay.is_deleted(e),
            "delete is terminal; unsuppress must not clear it"
        );
        assert!(
            !overlay.is_suppressed(e),
            "unsuppress clears the suppression and nothing else — it cannot reach the other two \
             stores at all"
        );
    }

    #[test]
    fn unsuppress_never_clears_deleted_reverse_order() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(2);

        overlay.apply(e, ChangeOp::Suppress, None);
        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Unsuppress, None);

        assert!(overlay.is_deleted(e));
        assert!(!overlay.is_suppressed(e));
    }

    #[test]
    fn predicate_never_clears_deny_flags() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(3);

        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Predicate, Some(predicate(&[(b"nine", 9)])));

        assert!(overlay.is_deleted(e));
        assert_eq!(
            overlay.evaluate_of(e).map(|p| p.terms.as_slice()),
            Some(&[TermId::new(9)][..])
        );
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
            overlay.evaluate_of(e).map(|p| p.terms.as_slice()),
            Some(&[TermId::new(77)][..])
        );

        // A later predicate change with no descriptors at all (`terms: None`) must not unset
        // `evaluate_terms` back to `None` — that would fall back to the fragment's original
        // verdict, which may have included this entity.
        overlay.apply(e, ChangeOp::Predicate, None);
        assert_eq!(
            overlay.evaluate_of(e).map(|p| p.terms.as_slice()),
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

        let result = replay(&records, &dict, Overlay::new(), |_external_id| {
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

        let (once, _, established_once, _) = replay(
            std::slice::from_ref(&suppress),
            &dict,
            Overlay::new(),
            resolve,
        )
        .unwrap();
        let (twice, _, established_twice, _) = replay(
            &[suppress.clone(), suppress],
            &dict,
            Overlay::new(),
            resolve,
        )
        .unwrap();

        assert_eq!(
            once.is_suppressed(entity),
            twice.is_suppressed(entity),
            "a second copy of a disposition record must fold to the same overlay entry — the \
             durability retry's safety net rests on exactly this"
        );
        assert!(
            twice.is_suppressed(entity),
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

        let (overlay, _buffer, _established, _resolver) =
            replay(&records, &dict, Overlay::new(), |external_id| {
                Ok::<_, std::convert::Infallible>(if external_id == b"from-a-previous-build" {
                    Some(bundle_entity)
                } else {
                    None
                })
            })
            .unwrap();

        assert!(overlay.is_deleted(bundle_entity));
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

        let (_overlay, buffer, established, _resolver) =
            replay(&records, &dict, Overlay::new(), |_external_id| {
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
