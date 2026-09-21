//! The overlay: per-entity deny state accumulated from `/control/changes`
//! (`write-path.md` §5.4's two removal rules).
//!
//! The overlay is **two independent stores**, never one overwritable disposition: deletions and
//! suppressions are separate facts that leave on different triggers. **Rule S** — an entry leaves
//! `suppressed` only by its unsuppress. **Rule F** — an entry leaves `deleted` only at the
//! compaction fold that *executes* it.
//!
//! *(Owner-ruled 2026-08-03, replacing lifecycle §3.2's stamp ledger. That document is not yet
//! rewritten — a reader who finds it describing per-deny retirement stamps has found the stale
//! text, not a second mechanism.)*
//!
//! **A deletion retires at the fold that executes it.** [`Overlay::retire`] is Rule F's only route
//! out of `deleted`, and its caller is the fold's own publication, which derives the executed set
//! from what it demonstrably removed (`tessera_engine`'s `compact`, compaction §5). Rule F's safety
//! is the identity match the publication seam builds (`write-path.md` §5.4, compaction §4), not a
//! stamp ordering.
//!
//! Collapsing the two into a single enum ("last write wins") is fail-open, and has been caught
//! twice: the sequence `delete → suppress → unsuppress` must not re-expose a deleted item. Two
//! separate containers, each written by exactly one op, make that impossible for any refactor that
//! still type-checks — where two fields in one struct left it a rule a reader had to keep in mind.
//! There were three stores until decision 0048; deleting the `evaluate` one is not a licence to
//! collapse the two that remain, whose separation carries the whole argument above.

use croaring::Bitmap;
use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_types::EntityId;

use crate::buffer::{DescriptorResolver, IngestBuffer};
use crate::wal::{ChangeOp, OverlaySnapshotEntry, WalRecord};

/// Per-entity deny state, held as **two independent stores**.
///
/// The two facts leave on different triggers — a suppression *only* by its unsuppress (Rule S), a
/// deletion only at the compaction fold that executes it (Rule F, `write-path.md` §5.4) — and the
/// rejected single rule (r1: one retirement stamp for every deny) is fail-open precisely for
/// suppression, because any stamp eventually retires the entry and re-exposes the item.
///
/// **Two stores rather than two fields in one struct, and the difference is not cosmetic.**
/// Collapsing the facts into one last-write-wins disposition was caught fail-open twice in review;
/// the counterexample is `delete → suppress → unsuppress`. Two fields made that a rule a refactor
/// could still break while compiling. Two containers, mutated in two places, cannot be collapsed by
/// any refactor that still type-checks. It also makes r1 *unexpressible* rather than merely
/// rejected: [`Overlay::suppressed`] is a bitmap of entity ids and carries no stamp field for a
/// retirement rule to act on at all. Its only removal path is an `Unsuppress`.
///
/// Both sets are Roaring bitmaps because that is what they are — sets of entity ids, in entity
/// space, where a set is the whole content.
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    /// Set by `Delete`, and removed from **only** by [`Overlay::retire`] — Rule F's single route.
    deleted: Bitmap,
    /// Set by `Suppress`, cleared **only** by `Unsuppress`. Never touched by `Delete`, which is
    /// true by construction rather than by discipline.
    suppressed: Bitmap,
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

    /// Whether either store holds an opinion about `entity`.
    ///
    /// **There is no "present but neutral" state any more.** Under a single map, `suppress →
    /// unsuppress` left an entry whose every fact was inactive, and its mere *presence* outranked
    /// the ingest buffer in `compose`'s verdict rule. An unsuppress now removes the id from the
    /// suppression bitmap, so the entity is untouched again and the buffer decides — which is what
    /// lifecycle §3.1 always said ("unsuppress removes the entry") and what the code did not do.
    pub fn touches(&self, entity: EntityId) -> bool {
        self.is_deleted(entity) || self.is_suppressed(entity)
    }

    /// `deleted ∪ suppressed` — every entity denied outright, in entity space.
    ///
    /// **The one input to the row-space deny mask** (`Generation::denied`), and the reason it is a
    /// union rather than two accessors: the mask's derivation rule turns on the union, so an
    /// unsuppress may not subtract a row while `deleted` still holds the entity. Handing callers
    /// the union makes the rule the only expressible thing.
    pub fn denied(&self) -> Bitmap {
        self.deleted.or(&self.suppressed)
    }

    /// The suppression set — `SEGMENTS-<n>.json`'s `deny` field, which a publication serialises
    /// as it stands rather than enumerating.
    ///
    /// Separate from [`Self::deleted_set`] because the two manifest fields mean different
    /// things and leave under different rules (`write-path.md` §5.4): a suppression only by its
    /// unsuppress (Rule S), a tombstone only at the fold that executes it (Rule F).
    /// [`Self::denied`] deliberately
    /// hands out only the union, which is right for the row mask and wrong here — a writer that
    /// published the union under one field would make every deletion look retirable by an
    /// unsuppress.
    pub fn suppressed_set(&self) -> &Bitmap {
        &self.suppressed
    }

    /// The deleted set — `SEGMENTS-<n>.json`'s `tombstones` field. See [`Self::suppressed_set`].
    pub fn deleted_set(&self) -> &Bitmap {
        &self.deleted
    }

    /// An overlay holding exactly these two sets, the form a manifest's deny fields seed at open.
    /// The two are given separately because the manifest keeps them apart, and a seed that unioned
    /// them would make every deletion retirable by an unsuppress.
    pub fn seeded(deleted: &Bitmap, suppressed: &Bitmap) -> Self {
        Overlay {
            deleted: deleted.clone(),
            suppressed: suppressed.clone(),
        }
    }

    /// Every entity either store has an opinion on, ascending, without duplicates.
    ///
    /// The same set as [`Self::denied`], in the form [`Self::snapshot`] iterates: with the evaluate
    /// store gone (decision 0048), every entity the overlay touches is one it denies. The two are
    /// kept apart because they answer different questions — `denied` is the row mask's input and is
    /// documented as a union rule, this is an enumeration — and a future non-deny disposition would
    /// separate them again.
    pub fn touched(&self) -> Vec<EntityId> {
        self.denied()
            .iter()
            .map(|id| EntityId::new(id as u64))
            .collect()
    }

    /// How many deletions stand — **the gauge a compaction trigger keys on**, and not the same
    /// number as [`Self::len`].
    ///
    /// `len` is `|deleted ∪ suppressed|`, and Rule S says a suppression never retires, so a
    /// deployment holding 500,000 standing suppressions is permanently over any limit expressed in
    /// those terms — and a trigger reading it would dispatch a **full no-op fold every interval,
    /// for ever**, rewriting the corpus to retire nothing (compaction §9; r3, memory F5). A
    /// trigger keys on what a fold can actually reduce; the *alarm* stays on total depth, which is
    /// the right thing for an operator to see.
    pub fn deleted_len(&self) -> u64 {
        self.deleted.cardinality()
    }

    /// How many entities this overlay has an opinion on — the depth the soft-limit alarm gauges.
    ///
    /// **This can now go down**, in both directions: an unsuppress removes an id, and a fold's
    /// retirement removes the deletions it executed ([`Self::retire`]).
    pub fn len(&self) -> usize {
        self.denied().cardinality() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty() && self.suppressed.is_empty()
    }

    /// Apply one disposition change to `entity`. Each op touches exactly one store — see this
    /// type's doc for why that is the whole safety argument.
    pub fn apply(&mut self, entity: EntityId, op: ChangeOp) {
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
        }
    }

    /// Withdraw `executed` from `deleted` — **Rule F's only retirement route, and it takes only
    /// deletions** (write-path §5.4).
    ///
    /// `suppressed` is not an operand and there is no sibling for it: a suppression's invisibility
    /// rests on its overlay entry for as long as it stands, so any retirement route at all is
    /// fail-open — *"any stamp eventually retires the entry and re-exposes the item"*. That is why
    /// this takes one bitmap and subtracts it from one store rather than taking a set of entities
    /// and asking which store they are in.
    ///
    /// # The caller's obligation, which nothing here can check
    ///
    /// **`executed` must contain only entities whose row and postings the publication being made
    /// in this same swap demonstrably removed** — compaction §5's rule, `{ e ∈ D₀ : no
    /// carried-forward artefact names e }`, evaluated against what was published and never against
    /// what a plan predicted. An entity retired while any carried-forward segment, tier or run
    /// still names it is an acknowledged deletion served to every authorised principal, permanently
    /// and with no error: the fold's identity match cannot see it, because no fragment is stale —
    /// the entity genuinely is in the post-fold postings.
    ///
    /// **In the publication's own swap, never before it.** An entry withdrawn while the old
    /// geometry is still live re-exposes the item for the width of that window. After it is merely
    /// wasteful, and is the safe direction if the ordering ever has to give.
    ///
    /// Returns how many entries were retired, which is what makes "this fold retired nothing"
    /// distinguishable from "this fold was not asked to".
    pub fn retire(&mut self, executed: &Bitmap) -> u64 {
        let before = self.deleted.cardinality();
        self.deleted.andnot_inplace(executed);
        before - self.deleted.cardinality()
    }

    /// Re-state this overlay as WAL records, so the `ChangeByEntity` records it was accumulated
    /// from can be deleted (lifecycle §4's rotation; [`WalRecord::OverlaySnapshot`]).
    ///
    /// **Entries are ordered by entity id**, so the same overlay always encodes to the same bytes.
    /// A record whose bytes depend on a set's iteration order is one that cannot be compared,
    /// re-derived, or re-encoded by [`crate::Wal::retry_durability`] with any confidence.
    ///
    /// There is no neutral-entry rule here any more, and its absence is the point: under one map a
    /// `suppress → unsuppress` left a husk whose presence was load-bearing, so the snapshot had to
    /// emit an `Unsuppress` to preserve it. Separate stores make the husk unrepresentable.
    pub fn snapshot(&self) -> Vec<OverlaySnapshotEntry> {
        let mut out = Vec::with_capacity(self.len());
        for entity in self.touched() {
            if self.is_deleted(entity) {
                out.push(OverlaySnapshotEntry {
                    entity_id: entity,
                    op: ChangeOp::Delete,
                });
            }
            if self.is_suppressed(entity) {
                out.push(OverlaySnapshotEntry {
                    entity_id: entity,
                    op: ChangeOp::Suppress,
                });
            }
        }
        out
    }

    /// Fold a snapshot back in.
    ///
    /// Applied, never assigned: the snapshot is a record in a position, and a `ChangeByEntity`
    /// earlier in the same file has already been applied when this runs.
    pub fn apply_snapshot(&mut self, entries: &[OverlaySnapshotEntry]) {
        for entry in entries {
            self.apply(entry.entity_id, entry.op);
        }
    }
}

/// Entity ids are capped at `u32::MAX` by the I9 allocator (contracts §2.6 r6), which is what lets
/// the deny sets be Roaring bitmaps at all.
///
/// **Checked, and the one narrowing in this crate.** A membership is a Roaring bitmap too, so
/// [`crate::membership`] builds one through this rather than through a second `as` that would
/// truncate an id outside the space into another entity's — a document nobody named put into an
/// artifact, with nothing reporting it. The build refuses the same id where it decodes one
/// (`tessera_build::spill`'s member table).
pub(crate) fn as_u32(entity: EntityId) -> u32 {
    u32::try_from(entity.raw())
        .expect("entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)")
}

/// Replay a WAL's records into an `Overlay` and an `IngestBuffer`.
///
/// Single left-to-right pass over `records`, in on-disk order (the order changes and ingests
/// were accepted in — WAL append order is causal order):
/// - `IngestBatch` rows populate `IngestBuffer` (term descriptors resolved via `dict` plus a
///   deterministic, replay-order in-memory extension — see [`DescriptorResolver`]) and register
///   `external_id → entity_id` in this replay's own external-id map, which the caller keeps for
///   live `/control/changes` admission.
/// - `ChangeByEntity` records apply their disposition directly. **No resolution happens here at
///   all**: the entity was fixed at admission, which is what makes the record replay to the same
///   entity under a rotated identity key and lets it address an item that never had an external
///   id. The external-id-keyed `Change` record this replay once also had to resolve was deleted
///   with `WAL_VERSION` 5 (decision 0048), and replay became infallible with it.
/// - `OverlaySnapshot` records are applied **at the position they occupy**, never used as a
///   starting state the walk then resumes from. Those are different algorithms: recovery walks
///   every surviving file in sequence order, and a file older than the one holding the snapshot may
///   still carry change records above the point the snapshot was taken at. Starting *at* the
///   snapshot would skip them — which looks like an optimisation and is a silent un-deny.
///
/// The `view_ids_of_key` a caller with **no manifest** passes [`replay`]: the owner's id alone.
///
/// Correct exactly where there is no `members` relation to expand — the unit tests here, and a
/// bundle whose groups share nothing. `Engine::open` passes `Manifest::view_ids_for_key` instead,
/// which is the definition (`views.md` §3.3, decision 0115); this is not a second one, it is the
/// same expansion over an empty relation.
pub fn owner_id_only(group: &str, key: &str) -> Vec<String> {
    vec![format!(
        "{group}{}{key}",
        tessera_types::view::GROUP_SEPARATOR
    )]
}

/// `view_ids_of_key` turns a `ViewDrop`'s `(owner group, key)` into every view id it names — the
/// owner's and every sharing group's (`views.md` §3.3). It is a parameter rather than a derivation
/// because the `members` relation lives in the bundle manifest and this crate does not depend on
/// `tessera-store`; `Engine::open` supplies `Manifest::view_ids_for_key`, and a caller with no
/// manifest supplies the owner's id alone.
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
pub fn replay<'a>(
    records: &[WalRecord],
    dict: &'a Dict,
    seed: Overlay,
    view_ids_of_key: &dyn Fn(&str, &str) -> Vec<String>,
) -> (
    Overlay,
    IngestBuffer,
    FxHashMap<Vec<u8>, EntityId>,
    DescriptorResolver<'a>,
) {
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
            WalRecord::ChangeByEntity { entity_id, op } => {
                // No resolution at all: the entity was fixed at admission, which is what makes
                // this record replay to the same entity under a rotated identity key, and what
                // lets it address an item that never had an external id.
                overlay.apply(*entity_id, *op);
            }
            WalRecord::OverlaySnapshot { entries } => {
                overlay.apply_snapshot(entries);
            }
            // **Applied by the caller, against the live vocabularies, not here.** This replay
            // builds the overlay and the buffer, and reaches neither the bundle manifest a binding
            // is seeded from nor the minter that has to hold it — `tessera-lifecycle` does not
            // depend on `tessera-store`. `WritePath::reconstruct` walks the same records for the
            // mints, after seeding, so the seed-before-replay order is preserved where the state
            // lives. The rows here already carry their codes, so the buffer needs no binding to
            // read one.
            WalRecord::VocabularyMint { .. } => {}
            // **The registry is rebuilt by the caller, and only the deny lane is this function's
            // business.** A layer's own entity is an ordinary entity as far as the overlay is
            // concerned: a suppression against it arrives as a `ChangeByEntity` and is applied by
            // the arm above, with no special case, which is the whole reason a layer takes an
            // entity at all. What these records carry beyond that — the declaration and the
            // reserved runs — belongs to the registry the write path reconstructs, in the same
            // second pass as the vocabulary mints and for the same reason.
            // An artifact's own entity is an ordinary entity here too, on the same argument: its
            // suppression arrives as a `ChangeByEntity`. The membership the record carries belongs
            // to the artifact store, rebuilt in that same second pass.
            // **A drop discards the rows the buffer held for the view, here as on the live
            // path** (`views.md` §3.4, `Executor::publish_roster`). They name a coordinate system
            // that no longer exists, so nothing will ever give them geometry — and since a
            // dropped key may be created again (decision 0115), a replay that left them would
            // land the *predecessor's* rows in the new view. That is the reason a buffered row
            // needs no incarnation of its own: replay is ordered, so the drop is met between the
            // rows it discards and the rows the recreate takes, and it is the one and only place
            // the two sets can be told apart.
            //
            // **Dropping a view still deletes no entity** (`views.md` §3.4).
            // `delete_dangling`'s deletions arrive here as the ordinary `ChangeByEntity` records
            // the arm above applies, which is what keeps the drop from being a second retirement
            // route.
            WalRecord::ViewDrop { view } => {
                // **Every id the key resolves to, not just the owner's.** A key is one view of the
                // group that owns it *and* one of every group sharing its views (`views.md` §3.3),
                // and the record names the owner — so a prune built from the record alone would
                // leave the sharing group's buffered rows to be flushed into whatever takes the
                // key next. The expansion is `Manifest::view_ids_for_key`'s, passed in because
                // this crate holds no manifest.
                let ids = view_ids_of_key(&view.group, &view.key);
                let orphaned: Vec<(EntityId, String)> = buffer
                    .rows()
                    .filter(|(_, item)| ids.iter().any(|id| id == &item.view))
                    .map(|(entity, item)| (*entity, item.view.clone()))
                    .collect();
                for (entity, view) in orphaned {
                    buffer.remove_in_view(entity, &view);
                }
            }
            // A view create is the roster's, rebuilt by the caller in that same second pass, and
            // names no entity.
            WalRecord::LayerCreate { .. }
            | WalRecord::LayerDrop { .. }
            | WalRecord::ArtifactPublish { .. }
            | WalRecord::ArtifactGrow { .. }
            | WalRecord::ArtifactFill { .. }
            | WalRecord::ViewCreate { .. } => {}
            // The ingest design's remaining records (`ingest.md` §7.1) are not applied by anything
            // yet, and a replay that meets one refuses to open before reaching here
            // (`crate::wal::unbuilt_track`). None of them names the deny lane: a values row fills
            // cells, and a declaration names no entity.
            WalRecord::ValuesBatch { .. }
            | WalRecord::AttributeDeclare { .. }
            | WalRecord::VocabularyDeclare { .. }
            | WalRecord::ViewGroupCreate { .. }
            | WalRecord::PlainViewCreate { .. } => {}
        }
    }

    drop_deleted(&overlay, &mut buffer);
    (overlay, buffer, established, resolver)
}

/// Drop every buffered row whose entity the overlay has deleted — **the buffer never holds a
/// deleted row** (write-path §4.2, decision 0047).
///
/// A deleted row acquires no geometry: `plan_flush` skips it, so a flush never consumes it and
/// its entry would sit in the buffer for the process's lifetime. That is not merely untidy —
/// `IngestBuffer::oldest_wal_pos` is the rotation's reclaim bound, so one such row **pins its WAL
/// member and every member after it**, and a deployment that deletes before its first flush stops
/// reclaiming the log entirely.
///
/// **Composition-neutral, which is what makes the removal safe rather than merely cheap.**
/// `compose::verdict` answers `Some(false)` from `overlay.is_deleted` before it ever consults the
/// buffer, and a deletion retires only at the fold that executes it (Rule F), so nothing downstream
/// can observe the difference. Under decision 0047 the entity is *forgotten* — its id stays burned
/// (I9), a re-ingest of its external id binds a new one — so the end state after reclamation, no
/// row and no buffer entry, is the ruled one and not a loss.
///
/// Applied as an end-of-pass rule rather than at each `Delete`, because the manifests' deny seed
/// is applied *before* the walk: a tombstone the seed carries would otherwise miss the
/// `IngestBatch` record replayed after it.
fn drop_deleted(overlay: &Overlay, buffer: &mut IngestBuffer) {
    let deleted: Vec<EntityId> = buffer
        .iter()
        .map(|(entity, _)| *entity)
        .filter(|entity| overlay.is_deleted(*entity))
        .collect();
    for entity in deleted {
        buffer.remove(entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::ChangeOp;

    #[test]
    fn two_facts_are_independent() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(1);

        overlay.apply(e, ChangeOp::Delete);
        overlay.apply(e, ChangeOp::Suppress);
        overlay.apply(e, ChangeOp::Unsuppress);

        assert!(
            overlay.is_deleted(e),
            "delete is terminal; unsuppress must not clear it"
        );
        assert!(
            !overlay.is_suppressed(e),
            "unsuppress clears the suppression and nothing else — it cannot reach the other \
             store at all"
        );
    }

    /// **Rule F's route takes deletions and cannot reach a suppression** — the fail-open caught
    /// twice in review, in its most direct form.
    ///
    /// A retirement set is entities, not dispositions, so an entity that is both deleted and
    /// suppressed is the case that matters: the deletion retires and the suppression stands, and
    /// the item is still hidden afterwards. There is no sibling of this method for `suppressed`
    /// and there must not be one — a suppression's invisibility rests on its entry for as long as
    /// it stands, so any retirement route at all re-exposes the item.
    #[test]
    fn retirement_takes_deletions_and_leaves_every_suppression_standing() {
        let mut overlay = Overlay::new();
        let both = EntityId::new(1);
        let deleted_only = EntityId::new(2);
        let suppressed_only = EntityId::new(3);
        let untouched_delete = EntityId::new(4);

        overlay.apply(both, ChangeOp::Delete);
        overlay.apply(both, ChangeOp::Suppress);
        overlay.apply(deleted_only, ChangeOp::Delete);
        overlay.apply(suppressed_only, ChangeOp::Suppress);
        overlay.apply(untouched_delete, ChangeOp::Delete);

        let mut executed = Bitmap::new();
        for entity in [both, deleted_only, suppressed_only] {
            executed.add(as_u32(entity));
        }
        assert_eq!(
            overlay.retire(&executed),
            2,
            "the count is deletions withdrawn — the suppressed-only entity was in the set and \
             contributed nothing"
        );

        assert!(!overlay.is_deleted(both));
        assert!(
            overlay.is_suppressed(both),
            "an entity that is both loses only its deletion; the suppression is what still hides it"
        );
        assert!(!overlay.is_deleted(deleted_only));
        assert!(
            overlay.is_suppressed(suppressed_only),
            "a suppression named in a retirement set is not retired — it has no route at all"
        );
        assert!(
            overlay.is_deleted(untouched_delete),
            "a deletion outside the set stands, which is the fail-closed direction: the next fold \
             takes it"
        );
        assert_eq!(overlay.len(), 3);
    }

    /// An empty retirement set is a no-op, which is what every fold whose tombstones were all
    /// carried forward produces.
    #[test]
    fn retiring_nothing_withdraws_nothing() {
        let mut overlay = Overlay::new();
        overlay.apply(EntityId::new(9), ChangeOp::Delete);
        assert_eq!(overlay.retire(&Bitmap::new()), 0);
        assert!(overlay.is_deleted(EntityId::new(9)));
    }

    #[test]
    fn unsuppress_never_clears_deleted_reverse_order() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(2);

        overlay.apply(e, ChangeOp::Suppress);
        overlay.apply(e, ChangeOp::Delete);
        overlay.apply(e, ChangeOp::Unsuppress);

        assert!(overlay.is_deleted(e));
        assert!(!overlay.is_suppressed(e));
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
    /// **external-id map** (below — change records never write to it; only `IngestBatch` rows do),
    /// and the **allocator seed**, which `high_water_from` derives from `IngestBatch` rows alone —
    /// pinned by `alloc.rs`'s own tests.
    #[test]
    fn a_disposition_replayed_twice_is_the_same_as_replayed_once() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let entity = EntityId::new(7);
        let suppress = WalRecord::ChangeByEntity {
            entity_id: entity,
            op: ChangeOp::Suppress,
        };

        let (once, _, established_once, _) = replay(
            std::slice::from_ref(&suppress),
            &dict,
            Overlay::new(),
            &owner_id_only,
        );
        let (twice, _, established_twice, _) = replay(
            &[suppress.clone(), suppress],
            &dict,
            Overlay::new(),
            &owner_id_only,
        );

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
            "a change record establishes no external id, so a second copy cannot double-register one"
        );
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
                    view: "default".to_string(),
                    join: false,
                    descriptors: Vec::new(),
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                    scoped: Vec::new(),
                },
                crate::wal::WalRow {
                    external_id: None,
                    entity_id: EntityId::new(101),
                    view: "default".to_string(),
                    join: false,
                    descriptors: Vec::new(),
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                    scoped: Vec::new(),
                },
            ],
        }];

        let (_overlay, buffer, established, _resolver) =
            replay(&records, &dict, Overlay::new(), &owner_id_only);

        assert!(
            established.is_empty(),
            "no external id was supplied, so nothing should be established"
        );
        assert_eq!(buffer.len(), 2, "both null-external-id rows still buffer");
    }
}
