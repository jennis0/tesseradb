//! The overlay: per-entity deny state accumulated from `/control/changes`
//! (`write-path.md` §5.4's two removal rules).
//!
//! The overlay is **two independent stores**, never one overwritable disposition: deletions and
//! suppressions are separate facts that leave on different triggers. **Rule S**: an entry leaves
//! `suppressed` by its unsuppress, or at the compaction fold that removes the entity it names. A
//! fold removes only deleted entities, and an item that still exists holds its suppression on its
//! current entity, which no fold removes. **Rule F**: an entry leaves `deleted` only at the
//! compaction fold that *executes* it.
//!
//! **A deletion retires at the fold that executes it.** [`Overlay::retire`] is Rule F's only route
//! out of `deleted`, and its caller is the fold's own publication, which derives the executed set
//! from what it demonstrably removed (`mosaica_engine`'s `compact`, compaction §5). Rule F's safety
//! is the identity match the publication builds (`write-path.md` §5.4, compaction §4), not a stamp
//! ordering.
//!
//! Collapsing the two into a single enum ("last write wins") is fail-open, and has been caught
//! twice: the sequence `delete → suppress → unsuppress` must not re-expose a deleted item. Two
//! separate containers, each written by exactly one op, make that impossible for any refactor that
//! still type-checks — where two fields in one struct left it a rule a reader had to keep in mind.
//! There were three stores until decision 0048; deleting the `evaluate` one is not a licence to
//! collapse the two that remain, whose separation carries the whole argument above.

use croaring::Bitmap;

use std::collections::BTreeMap;

use mosaica_authz::Dict;
use mosaica_types::{EntityId, LayerSpace, ShardId, Target};

use crate::buffer::{DescriptorResolver, IngestBuffer};
use crate::wal::{ChangeOp, OverlaySnapshotEntry, WalRecord};

/// Per-entity deny state, held as **two independent stores**.
///
/// The two facts leave on different triggers: a suppression by its unsuppress, or with its entity
/// at the fold that removes it (Rule S), a deletion only at the compaction fold that executes it
/// (Rule F, `write-path.md` §5.4). One retirement stamp for every deny would be fail-open for
/// suppression, because any stamp eventually retires the entry and re-exposes the item.
///
/// **Two stores rather than two fields in one struct, and the difference is not cosmetic.**
/// Collapsing the facts into one last-write-wins disposition was caught fail-open twice in review;
/// the counterexample is `delete → suppress → unsuppress`. Two fields made that a rule a refactor
/// could still break while compiling. Two containers, mutated in two places, cannot be collapsed by
/// any refactor that still type-checks. It also makes r1 *unexpressible* rather than merely
/// rejected: [`Overlay::suppressed`] is a bitmap of entity ids and carries no stamp field for a
/// retirement rule to act on at all. It leaves by an `Unsuppress`, or with its entity's executed
/// deletion in [`Overlay::retire`].
///
/// Both sets are Roaring bitmaps because that is what they are — sets of entity ids, in entity
/// space, where a set is the whole content.
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    /// Set by `Delete`, and removed from **only** by [`Overlay::retire`] — Rule F's single route.
    deleted: Bitmap,
    /// Set by `Suppress`, cleared by `Unsuppress` and, for an entity whose deletion it retires, by
    /// [`Overlay::retire`]. Never touched by `Delete`, which is true by construction rather than by
    /// discipline.
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
    /// **The one input to the row-space deny mask** (`Shard::denied`), and the reason it is a
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
    /// things and leave under different rules (`write-path.md` §5.4): a suppression by its
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
    /// `len` is `|deleted ∪ suppressed|`, and a standing suppression leaves only by its
    /// unsuppress, so a deployment holding 500,000 standing suppressions is permanently over any
    /// limit expressed in those terms, and a trigger reading it would dispatch a full no-op fold
    /// every interval, rewriting the corpus to retire nothing. A trigger keys on what a fold can
    /// reduce; the alarm stays on total depth, which is the right thing for an operator to see.
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

    /// Withdraw `executed` from `deleted`, **Rule F's only retirement route** (write-path §5.4),
    /// and withdraw [`Self::suppressions_retired_by`] from `suppressed`.
    ///
    /// An executed entity has no row left anywhere, so its suppression hides nothing more, and an
    /// id a fold frees must carry none to the item that takes it. An item that still exists holds
    /// its suppression on its current entity, which is not deleted, so a suppression of an entity
    /// in `executed` that this overlay does not delete stands.
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
        let retiring = executed.and(&self.deleted);
        self.suppressed.andnot_inplace(&retiring);
        self.deleted.andnot_inplace(&retiring);
        retiring.cardinality()
    }

    /// The suppressions [`Self::retire`] withdraws for `executed`: those of the entities in it
    /// that this overlay deletes.
    pub fn suppressions_retired_by(&self, executed: &Bitmap) -> Bitmap {
        executed.and(&self.deleted).and(&self.suppressed)
    }

    /// Re-state this overlay as WAL records, each entity named as `target` makes it, so the
    /// `ChangeBatch` records it was accumulated from can be deleted (lifecycle §4's rotation;
    /// [`WalRecord::OverlaySnapshot`]).
    ///
    /// **Entries are ordered by entity id**, so the same overlay always encodes to the same bytes.
    /// A record whose bytes depend on a set's iteration order is one that cannot be compared,
    /// re-derived, or re-encoded by [`crate::Wal::retry_durability`] with any confidence.
    pub fn snapshot(&self, target: impl Fn(EntityId) -> Target) -> Vec<OverlaySnapshotEntry> {
        let mut out = Vec::with_capacity(self.len());
        for entity in self.touched() {
            if self.is_deleted(entity) {
                out.push(OverlaySnapshotEntry {
                    target: target(entity),
                    op: ChangeOp::Delete,
                });
            }
            if self.is_suppressed(entity) {
                out.push(OverlaySnapshotEntry {
                    target: target(entity),
                    op: ChangeOp::Suppress,
                });
            }
        }
        out
    }
}

/// The deny state of every point shard's items and of every layer space's artifacts, each target
/// in the overlay of the shard or the space it names.
#[derive(Debug, Default, Clone)]
pub struct Overlays {
    /// One per point shard holding a deny; a shard absent here denies nothing.
    pub items: BTreeMap<ShardId, Overlay>,
    /// One per layer space holding a deny; a space absent here denies nothing.
    pub spaces: BTreeMap<LayerSpace, Overlay>,
}

impl Overlays {
    /// Apply one change to the overlay its target names.
    pub fn apply(&mut self, target: Target, op: ChangeOp) {
        match target {
            Target::Item(shard, entity) => self.items.entry(shard).or_default().apply(entity, op),
            Target::Artifact(space, entity) => {
                self.spaces.entry(space).or_default().apply(entity, op)
            }
        }
    }

    /// Fold a snapshot back in, each entry into the overlay its target names. Applied, never
    /// assigned: the snapshot is a record in a position, and a `ChangeBatch` earlier in the same
    /// file has already been applied when this runs.
    pub fn apply_snapshot(&mut self, entries: &[OverlaySnapshotEntry]) {
        for entry in entries {
            self.apply(entry.target, entry.op);
        }
    }
}

/// Every overlay re-stated as one snapshot: each shard's items, then each space's.
pub fn snapshot<'o>(
    shards: impl IntoIterator<Item = (ShardId, &'o Overlay)>,
    spaces: impl IntoIterator<Item = (LayerSpace, &'o Overlay)>,
) -> Vec<OverlaySnapshotEntry> {
    let mut out = Vec::new();
    for (shard, items) in shards {
        out.extend(items.snapshot(|entity| Target::Item(shard, entity)));
    }
    for (space, overlay) in spaces {
        out.extend(overlay.snapshot(|entity| Target::Artifact(space, entity)));
    }
    out
}

/// Entity ids are capped at `u32::MAX` by the I9 allocator (contracts §2.6 r6), which is what lets
/// the deny sets be Roaring bitmaps at all.
///
/// **Checked, and the one narrowing in this crate.** A membership is a Roaring bitmap too, so
/// [`crate::membership`] builds one through this rather than through a second `as` that would
/// truncate an id outside the space into another entity's — a document nobody named put into an
/// artifact, with nothing reporting it. The build refuses the same id where it decodes one
/// (`mosaica_build::spill`'s member table).
pub(crate) fn as_u32(entity: EntityId) -> u32 {
    u32::try_from(entity.raw())
        .expect("entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)")
}

/// The `view_ids_of_key` a caller with **no manifest** passes [`Replay::new`]: the owner's id
/// alone.
///
/// Correct exactly where there is no `members` relation to expand — the unit tests here, and a
/// bundle whose groups share nothing. `Engine::open` passes `Manifest::view_ids_for_key` instead,
/// which is the definition (`views.md` §3.3, decision 0115); this is not a second one, it is the
/// same expansion over an empty relation.
pub fn owner_id_only(group: &str, key: &str) -> Vec<String> {
    vec![format!(
        "{group}{}{key}",
        mosaica_types::view::GROUP_SEPARATOR
    )]
}

/// Replays a WAL's records into the overlays and an `IngestBuffer` per point shard, one record at
/// a time.
///
/// Single left-to-right pass over the records, in on-disk order (the order changes and ingests
/// were accepted in — WAL append order is causal order):
/// - `IngestBatch` rows populate the buffer of the shard each names (term descriptors resolved via
///   `dict` plus a deterministic, replay-order in-memory extension shared by every shard — see
///   [`DescriptorResolver`]).
/// - `ChangeBatch` records apply their disposition directly. **No resolution happens here at
///   all**: the entity was fixed at admission, which is what makes replay infallible.
/// - `OverlaySnapshot` records are applied **at the position they occupy**, never used as a
///   starting state the walk then resumes from. Those are different algorithms: recovery walks
///   every surviving file in sequence order, and a file older than the one holding the snapshot may
///   still carry change records above the point the snapshot was taken at. Starting *at* the
///   snapshot would skip them — which looks like an optimisation and is a silent un-deny.
///
/// `view_ids_of_key` turns a `ViewDrop`'s `(owner group, key)` into every view id it names — the
/// owner's and every sharing group's (`views.md` §3.3). It is a parameter rather than a derivation
/// because the `members` relation lives in the bundle manifest and this crate does not depend on
/// `mosaica-store`; `Engine::open` supplies `Manifest::view_ids_for_key`, and a caller with no
/// manifest supplies the owner's id alone.
///
/// [`Replay::finish`] returns, alongside the overlay and buffer, the `DescriptorResolver` in its
/// final state, borrowed from `dict`. `Engine::open` immediately detaches it (`.into_state()`) into
/// owned data it keeps for the process's lifetime, so a live `/control/ingest` acceptance can keep
/// resolving novel descriptors from exactly where replay left off, rather than restarting the
/// sequence (see [`DescriptorResolver::resume`]'s doc for why restarting descriptor extension ids
/// would be fail-open).
pub struct Replay<'a> {
    overlay: Overlays,
    buffers: BTreeMap<ShardId, IngestBuffer>,
    resolver: DescriptorResolver<'a>,
    view_ids_of_key: &'a dyn Fn(&str, &str) -> Vec<String>,
}

impl<'a> Replay<'a> {
    pub fn new(
        dict: &'a Dict,
        seed: Overlays,
        view_ids_of_key: &'a dyn Fn(&str, &str) -> Vec<String>,
    ) -> Self {
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
        Replay {
            overlay: seed,
            buffers: BTreeMap::new(),
            resolver: DescriptorResolver::new(dict),
            view_ids_of_key,
        }
    }

    /// Applies `record`, which sits at `position` in the log, and stamps each row it buffers with
    /// that position. `flushed` names the rows, each by its shard, that already have geometry:
    /// they are not buffered, though their descriptors are still resolved.
    pub fn apply(
        &mut self,
        record: &WalRecord,
        position: u64,
        flushed: impl Fn(ShardId, EntityId, &str) -> bool,
    ) {
        match record {
            WalRecord::IngestBatch { rows, edits, .. } => {
                // An edit deletes the entity its item leaves and gives the new one the old one's
                // suppression, both in the record that creates the new one, in the item's shard.
                for edit in edits {
                    self.overlay
                        .apply(Target::Item(edit.shard, edit.old), ChangeOp::Delete);
                    if let Some(first) = edit.rows.first() {
                        if edit.suppressed {
                            self.overlay.apply(
                                Target::Item(edit.shard, first.entity_id),
                                ChangeOp::Suppress,
                            );
                        }
                    }
                }
                for row in rows.iter().chain(edits.iter().flat_map(|edit| &edit.rows)) {
                    if flushed(row.shard, row.entity_id, &row.view) {
                        // Resolved all the same, so the extension sequence is the one the live
                        // path took.
                        for descriptor in &row.descriptors {
                            self.resolver.resolve(descriptor);
                        }
                        continue;
                    }
                    let buffer = self.buffers.entry(row.shard).or_default();
                    buffer.insert_row(row, &mut self.resolver);
                    buffer.set_wal_pos(row.entity_id, &row.view, position);
                }
            }
            WalRecord::ChangeBatch { changes } => {
                // No resolution at all: each target was fixed at admission, and names its overlay.
                for (target, op) in changes {
                    self.overlay.apply(*target, *op);
                }
            }
            WalRecord::OverlaySnapshot { entries } => {
                self.overlay.apply_snapshot(entries);
            }
            // **Applied by the caller, against the live vocabularies, not here.** This replay
            // builds the overlay and the buffer, and reaches neither the bundle manifest a binding
            // is seeded from nor the minter that has to hold it — `mosaica-lifecycle` does not
            // depend on `mosaica-store`. `WritePath::reconstruct` walks the same records for the
            // mints, after seeding, so the seed-before-replay order is preserved where the state
            // lives. The rows here already carry their codes, so the buffer needs no binding to
            // read one.
            WalRecord::VocabularyMint { .. } => {}
            // **The registry is rebuilt by the caller, and only the deny lane is this replay's
            // business.** A suppression of a layer or an artifact arrives as a `ChangeBatch`
            // naming its space and is applied by the arm above. What these records carry beyond
            // that belongs to the registry and the artifact store, which the write path rebuilds
            // in its own walk of the log.
            // **A drop discards the rows the buffer held for the view, here as on the live
            // path** (`views.md` §3.4, `Executor::publish_roster`). They name a coordinate system
            // that no longer exists, so nothing will ever give them geometry — and since a
            // dropped key may be created again (decision 0115), a replay that left them would
            // land the *predecessor's* rows in the new view. That is the reason a buffered row
            // needs no incarnation of its own: replay is ordered, so the drop is met between the
            // rows it discards and the rows the recreate takes, and it is the one and only place
            // the two sets can be told apart.
            //
            // The items the drop left in no view and the artifacts of the incarnation it killed
            // are deleted here as a `ChangeBatch` delete is, and retire at the fold like any
            // other deletion.
            WalRecord::ViewDrop { view, deleted } => {
                // **Every id the key resolves to, not just the owner's.** A key is one view of the
                // group that owns it *and* one of every group sharing its views (`views.md` §3.3),
                // and the record names the owner — so a prune built from the record alone would
                // leave the sharing group's buffered rows to be flushed into whatever takes the
                // key next. The expansion is `Manifest::view_ids_for_key`'s, passed in because
                // this crate holds no manifest.
                let views = (self.view_ids_of_key)(&view.group, &view.key);
                for buffer in self.buffers.values_mut() {
                    buffer.remove_views(&views);
                }
                for target in deleted {
                    self.overlay.apply(*target, ChangeOp::Delete);
                }
            }
            // A view create is the roster's, which the write path rebuilds, and names no entity.
            WalRecord::LayerCreate { .. }
            | WalRecord::LayerDrop { .. }
            | WalRecord::ArtifactPublish { .. }
            | WalRecord::ArtifactGrow { .. }
            | WalRecord::ArtifactFill { .. }
            | WalRecord::ViewCreate { .. } => {}
            // A declaration names no entity.
            WalRecord::AttributeDeclare { .. }
            | WalRecord::UniqueDeclare { .. }
            | WalRecord::VocabularyDeclare { .. }
            | WalRecord::ViewGroupCreate { .. }
            | WalRecord::PlainViewCreate { .. }
            | WalRecord::ShardsDeclare { .. } => {}
            // Opening and sealing a shard buffer and deny nothing.
            WalRecord::ShardOpen { .. } | WalRecord::ShardSeal { .. } => {}
        }
    }

    /// The overlays, each point shard's buffer and the resolver the records replayed to. A shard
    /// no row was buffered for has no buffer here.
    pub fn finish(
        mut self,
    ) -> (
        Overlays,
        BTreeMap<ShardId, IngestBuffer>,
        DescriptorResolver<'a>,
    ) {
        for (shard, buffer) in &mut self.buffers {
            if let Some(overlay) = self.overlay.items.get(shard) {
                drop_deleted(overlay, buffer);
            }
        }
        (self.overlay, self.buffers, self.resolver)
    }
}

/// Drop every row the buffer holds for an entity the overlay has deleted, in every view — **the
/// buffer never holds anything of a deleted entity** (write-path §4.2, decision 0047). A join
/// carries no terms and so is absent from the entity-space walk, and it pins the log exactly as an
/// own row does.
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
/// can observe the difference. A deleted item names nothing, so the end state after reclamation,
/// no row and no buffer entry, is the ruled one and not a loss.
///
/// Applied as an end-of-pass rule rather than at each `Delete`, because the manifests' deny seed
/// is applied *before* the walk: a tombstone the seed carries would otherwise miss the
/// `IngestBatch` record replayed after it.
fn drop_deleted(overlay: &Overlay, buffer: &mut IngestBuffer) {
    buffer.remove_where(|entity| overlay.is_deleted(entity));
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

    /// **Rule F's route takes the executed entities' deletions and suppressions, and nothing
    /// else.** A suppression of an entity the overlay does not delete stands even where the set
    /// names it, and so does a deletion outside the set.
    #[test]
    fn retirement_takes_the_executed_entities_and_leaves_every_other_deny_standing() {
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

        let executed = Bitmap::of(&[as_u32(both), as_u32(deleted_only), as_u32(suppressed_only)]);
        assert_eq!(
            overlay.suppressions_retired_by(&executed),
            Bitmap::of(&[as_u32(both)])
        );
        assert_eq!(overlay.retire(&executed), 2, "the count is deletions withdrawn");
        assert!(!overlay.touches(both), "an executed entity leaves both stores");
        assert!(!overlay.touches(deleted_only));
        assert!(
            overlay.is_suppressed(suppressed_only),
            "a suppression of an entity the overlay does not delete stands, though the set names it"
        );
        assert!(
            overlay.is_deleted(untouched_delete),
            "a deletion outside the set stands, which is the fail-closed direction: the next fold \
             takes it"
        );
        assert_eq!(overlay.len(), 2);
    }

    /// An empty retirement set is a no-op, which is what every fold whose tombstones were all
    /// carried forward produces.
    #[test]
    fn retiring_nothing_withdraws_nothing() {
        let mut overlay = Overlay::new();
        overlay.apply(EntityId::new(9), ChangeOp::Delete);
        overlay.apply(EntityId::new(9), ChangeOp::Suppress);
        assert_eq!(overlay.retire(&Bitmap::new()), 0);
        assert!(overlay.is_deleted(EntityId::new(9)) && overlay.is_suppressed(EntityId::new(9)));
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
    /// The deny lane's durability retry (`mosaica-engine`'s `Executor::retry_deny_durability`)
    /// re-writes a window's records in place, so it leaves one copy — but it is safe to re-write
    /// only because replaying a disposition twice is indistinguishable from replaying it once, and
    /// that is a property of *this* function, not of the retry. Any future repair that appends a
    /// second copy instead of rewinding depends on it directly.
    ///
    /// Two pieces of replayed state could have carried the difference, and each is checked here or
    /// named: the **overlay** (below, by comparing against the single-copy replay), and the
    /// **allocator seed**, which `raise_high_water` derives from `IngestBatch` rows alone — pinned
    /// by `alloc.rs`'s own tests.
    #[test]
    fn a_disposition_replayed_twice_is_the_same_as_replayed_once() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = mosaica_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let entity = EntityId::new(7);
        let suppress = WalRecord::ChangeBatch {
            changes: vec![(
                mosaica_types::Target::Item(mosaica_types::ShardId::ZERO, entity),
                ChangeOp::Suppress,
            )],
        };

        let mut once = Replay::new(&dict, Overlays::default(), &owner_id_only);
        once.apply(&suppress, 0, |_, _, _| false);
        let once = once
            .finish()
            .0
            .items
            .remove(&ShardId::ZERO)
            .unwrap_or_default();
        let mut twice = Replay::new(&dict, Overlays::default(), &owner_id_only);
        twice.apply(&suppress, 0, |_, _, _| false);
        twice.apply(&suppress, 1, |_, _, _| false);
        let twice = twice
            .finish()
            .0
            .items
            .remove(&ShardId::ZERO)
            .unwrap_or_default();

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
    }

    #[test]
    fn a_log_naming_two_shards_replays_into_each_shards_buffer_and_overlay() {
        use crate::wal::{WalEdit, WalRow};

        let temp = tempfile::TempDir::new().unwrap();
        let paths = mosaica_authz::DictWriter::new(temp.path())
            .finish()
            .unwrap();
        let dict = Dict::load(&paths).unwrap();
        let (zero, one) = (ShardId::ZERO, ShardId::new(1).unwrap());
        let row = |shard: ShardId, entity: u64| WalRow {
            shard,
            entity_id: EntityId::new(entity),
            view: "s0".into(),
            join: false,
            descriptors: Vec::new(),
            x: 0.5,
            y: 0.5,
            scalars: Vec::new(),
            scoped: Vec::new(),
        };
        let batch = WalRecord::IngestBatch {
            batch_id: "b".into(),
            body_hash: [0; 32],
            rows: vec![row(zero, 3), row(one, 3), row(one, 4)],
            edits: vec![WalEdit {
                shard: one,
                old: EntityId::new(1),
                number: EntityId::new(1),
                suppressed: true,
                rows: vec![row(one, 5)],
            }],
            receipt: Vec::new(),
        };
        let drop = WalRecord::ViewDrop {
            view: mosaica_types::view::DeadIncarnation {
                group: "g".into(),
                key: "k".into(),
                incarnation: 0,
            },
            deleted: vec![Target::Item(zero, EntityId::new(3))],
        };

        let mut replay = Replay::new(&dict, Overlays::default(), &owner_id_only);
        replay.apply(&batch, 7, |_, _, _| false);
        replay.apply(&drop, 9, |_, _, _| false);
        let (overlays, buffers, _) = replay.finish();

        let entities = |shard: ShardId| -> Vec<u64> {
            let mut held: Vec<u64> = buffers[&shard].rows().map(|(e, _)| e.raw()).collect();
            held.sort_unstable();
            held
        };
        assert_eq!(
            entities(zero),
            Vec::<u64>::new(),
            "shard 0's row was deleted"
        );
        assert_eq!(entities(one), vec![3, 4, 5]);
        let (items_zero, items_one) = (&overlays.items[&zero], &overlays.items[&one]);
        assert!(items_zero.is_deleted(EntityId::new(3)));
        assert!(
            !items_one.is_deleted(EntityId::new(3)),
            "the same number in shard 1"
        );
        assert!(
            items_one.is_deleted(EntityId::new(1)),
            "the edit's old entity"
        );
        assert!(
            items_one.is_suppressed(EntityId::new(5)),
            "the edit's new entity"
        );
        assert!(!items_zero.touches(EntityId::new(1)) && !items_zero.touches(EntityId::new(5)));
    }
}
