//! The entity-ID allocator, seeded from durable state at boot.
//!
//! `Allocator::new` is always seeded from `max(manifest_hw, replayed rows/leases)`: the highest
//! entity ID any durable artefact (bundle manifest, or a replayed WAL row/lease) has ever claimed.
//! Above that mark no id has been issued, and as long as the seed never goes backwards across a
//! restart, no id above it is handed out twice.
//!
//! ## Freed ids
//!
//! An edit moves an item to a new entity and keeps its number, the entity it was first given,
//! from which its `mosaica_id` is taken. The entity it leaves is deleted, and the fold that
//! removes its rows frees it at tenancy 0. A deleted item's number is freed by the fold that
//! removes the number's last entity, one tenancy higher than it was, so the item that takes it next
//! has a `mosaica_id` no earlier holder had; a number at [`Tenancy::MAX`] is retired instead and
//! never freed. The tenancy index records each number's tenancy (`mosaica_store::tenancy`), and
//! the free and held sets here are kept by tenancy. A row-less entity is never freed.
//!
//! A commit window draws its edits' new entities first, each from tenancy 0, lowest id first, then
//! from the high-water: an edit's entity carries its item's `mosaica_id` and never becomes a
//! number, so it takes no id an item has held as its number. Its new items then take freed ids from
//! the lowest tenancy, lowest id first, then from the high-water. Freed ids are issued before any id
//! from the high-water, so churn reuses a bounded set of ids rather than growing the id space and
//! everything sized by it. A suppressed entity is freed like any other: the fold that removes it
//! drops its suppression with it.
//!
//! A fold's freed ids are held back until the log keeps no record older than the fold's
//! publication ([`Allocator::release_after`], [`Allocator::promote`]). Replay applies every record
//! the log keeps, and a record older than that names the entity's previous holder: its rows, its
//! deletion, its memberships. Holding the ids back means a record naming a freed id always names
//! its current holder.
//!
//! The free and held sets are written into every side-manifest, each as one set of ids. A restart
//! takes them from the served manifest, splits them by the tenancy index that manifest names, and
//! removes every id a kept log record or the overlay names from both: an id issued after that
//! manifest was written appears in one or the other until a later manifest records it, and a held
//! id a kept record names has had its previous holder's state replayed.
//!
//! ## One space, two regions, growing towards each other
//!
//! Points are allocated **upward from 0**. Row-less entities — an artifact, and the entity a layer
//! takes so that layer suppression can ride the deny lane — are allocated **downward from the
//! top**. Both marks live in this one struct, and **exhaustion is the two marks meeting**, which is the
//! true condition where a fixed ceiling per region would be a guess about the split.
//!
//! **The reason is not identifier supply, which is where a reader looks first.** Three structures
//! size themselves over entity *ranges* rather than entity counts: a flush or merge segment's row
//! table is dense over its span, a merge allocates one slot per entity across its whole merged
//! window, and the fold's pre-flight charges four bytes per entity and **declines the fold** when
//! the total exceeds the host. Every one of those bounds is derived from a segment extent, and a
//! row-less entity appears in no segment — so an id above every point costs nothing anywhere. The
//! cost appears only when a row-less run sits *between* two point segments later merged: the
//! merged window is dense across the gap, so a ten-million-wide run is ten million wasted slots
//! that never come back. Under a single monotone allocator that interleaving is the **normal**
//! case — a layer published between two ingest windows. Two regions make it unrepresentable
//! rather than unlikely, which is a property of this code and not a rule an operator has to
//! observe.
//!
//! **The downward mark is durable on the same terms as the upward one** and must be, or a rotation
//! and restart can re-issue a row-less id to a point: two entities, one `mosaica_id`. See
//! [`low_water_from`], which is [`high_water_from`]'s mirror, and `allocator_floor`'s companion
//! [`allocator_ceiling`].
//!
//! `mosaica_lifecycle::buffer` already allocates term-extension ids downward from `u32::MAX` on the
//! same argument in a different space; this is that pattern applied to a second population.

use std::collections::BTreeMap;
use std::ops::Range;

use croaring::Bitmap;
use mosaica_types::layer::RESERVED_BLOCK;
use mosaica_types::{EntityId, Tenancy, TermId};

use crate::wal::WalRecord;

/// The entity-ID space's ceiling (contracts §2.6): `bundle_format = 1`
/// narrows every entity ID to `u32`, and `IdentityKey::forward`'s checked conversion refuses
/// any entity at or above this bound. The allocator refusing first is what makes that
/// conversion's error unreachable in practice rather than a rare, hard-to-reach corruption
/// path: without this cap, "collision-free by construction" rested entirely on the corpus
/// happening to stay small, and nothing enforced it.
const ENTITY_ID_CEILING: u64 = u32::MAX as u64;

/// Re-exported so this module's callers need only this module. The definition lives beside
/// [`RESERVED_BLOCK`] in `mosaica_types::layer`, because how entity space is divided is a fact
/// several crates read and only this one allocates against.
pub use mosaica_types::layer::ROWLESS_CEILING;

/// Allocator errors. The batch has no effect when this is returned — neither [`Allocator::allocate`]
/// nor [`Allocator::allocate_rowless`] moves its mark on the error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Issuing the requested range would carry the point region's mark into the row-less region's,
    /// or past [`ENTITY_ID_CEILING`] when nothing row-less has been allocated. Both marks are
    /// reported because "how much is left" is the gap between them and neither alone says it.
    Exhausted { high_water: u64, low_water: u64 },
    /// The seeds already meet or cross — caught at open rather than at the first allocation, so a
    /// corrupt or hand-edited manifest mark fails closed immediately instead of silently colliding.
    SeedAtCeiling { high_water: u64, low_water: u64 },
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocError::Exhausted {
                high_water,
                low_water,
            } => write!(
                f,
                "entity-ID space exhausted: {} ids remain between the point mark {high_water} and \
                 the row-less mark {low_water}, and the request does not fit",
                low_water.saturating_sub(*high_water)
            ),
            AllocError::SeedAtCeiling {
                high_water,
                low_water,
            } => write!(
                f,
                "allocator seeds already meet: point mark {high_water}, row-less mark {low_water} \
                 (ceiling {ENTITY_ID_CEILING}); refusing to open rather than collide on the first \
                 allocation"
            ),
        }
    }
}

impl std::error::Error for AllocError {}

/// Freed point ids by the tenancy each is at. A tenancy holding no id has no entry.
pub type ByTenancy = BTreeMap<Tenancy, Bitmap>;

/// Every id of `sets`, whatever its tenancy.
pub fn union_of(sets: &ByTenancy) -> Bitmap {
    sets.values().fold(Bitmap::new(), |all, ids| all.or(ids))
}

fn cardinality_of(sets: &ByTenancy) -> u64 {
    sets.values().map(Bitmap::cardinality).sum()
}

fn absorb(into: &mut ByTenancy, from: ByTenancy) {
    for (tenancy, ids) in from {
        if !ids.is_empty() {
            into.entry(tenancy).or_default().or_inplace(&ids);
        }
    }
}

fn take_out(sets: &mut ByTenancy, ids: &Bitmap) {
    for held in sets.values_mut() {
        held.andnot_inplace(ids);
    }
    sets.retain(|_, held| !held.is_empty());
}

/// The lowest `n` ids of `sets` at `tenancy`, taken out of it.
fn take_lowest(sets: &mut ByTenancy, tenancy: Tenancy, n: u64) -> Vec<u64> {
    let Some(ids) = sets.get_mut(&tenancy) else {
        return Vec::new();
    };
    let taken: Vec<u64> = ids.iter().take(n as usize).map(u64::from).collect();
    if let Some(&last) = taken.last() {
        ids.remove_range(..=last as u32);
    }
    if ids.is_empty() {
        sets.remove(&tenancy);
    }
    taken
}

/// The ids one commit window is given, each list in the order its ids were issued.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Allocation {
    /// One for each edit's new entity.
    pub edits: Vec<u64>,
    /// One for each new item.
    pub items: Vec<u64>,
}

/// The entity-ID allocator: both region marks, the freed ids it issues first, and the freed ids it
/// holds back.
pub struct Allocator {
    high_water: u64,
    low_water: u64,
    /// Freed point ids, issued before any id from `high_water` (module doc).
    free: ByTenancy,
    /// Freed point ids held back until the log keeps no record older than the position beside
    /// them.
    held: Vec<(u64, ByTenancy)>,
}

impl Allocator {
    /// Seeds the point region at `high_water` — the caller's `max(manifest_hw, replayed rows)` —
    /// with the row-less region untouched at [`ROWLESS_CEILING`]. Unchecked: prefer
    /// [`Allocator::try_new`] wherever the seed has not already been validated, since this
    /// constructor will happily seed past the mark and let the first `allocate` refuse instead.
    pub fn new(high_water: u64) -> Self {
        Allocator::with_marks(high_water, ROWLESS_CEILING)
    }

    /// [`Allocator::new`] with both marks supplied, for a restart that has recovered a row-less
    /// allocation. `low_water` is the lowest row-less id ever issued, or [`ROWLESS_CEILING`] if
    /// none has been.
    pub fn with_marks(high_water: u64, low_water: u64) -> Self {
        Allocator {
            high_water,
            low_water,
            free: ByTenancy::new(),
            held: Vec::new(),
        }
    }

    /// [`Allocator::new`], refusing a seed that already meets the row-less mark — the check that
    /// belongs at open, so a manifest mark that already exceeds the bound is caught before any
    /// allocation is attempted rather than surfacing as an opaque exhaustion error later.
    pub fn try_new(high_water: u64) -> Result<Self, AllocError> {
        Self::try_with_marks(high_water, ROWLESS_CEILING)
    }

    /// [`Allocator::with_marks`], with [`Allocator::try_new`]'s open-time check over both.
    pub fn try_with_marks(high_water: u64, low_water: u64) -> Result<Self, AllocError> {
        if low_water > ROWLESS_CEILING || high_water >= low_water {
            return Err(AllocError::SeedAtCeiling {
                high_water,
                low_water,
            });
        }
        Ok(Allocator::with_marks(high_water, low_water))
    }

    /// Allocates **point** entity IDs for a commit window's `edits` new entities and its `items`
    /// new items, in the order the module doc gives: the edits first, from the freed ids at
    /// tenancy 0 and then the high-water, and the items after, from the freed ids of the lowest
    /// tenancy and then the high-water, which advances past every id it gives. Refuses, leaving
    /// the allocator unchanged, if the ids from the high-water would reach the row-less region: an
    /// allocation that overran it would put two entities on one `mosaica_id`.
    pub fn allocate(&mut self, edits: u64, items: u64) -> Result<Allocation, AllocError> {
        let at_zero = self.free.get(&Tenancy::ZERO).map_or(0, Bitmap::cardinality);
        let edits_reused = edits.min(at_zero);
        let items_reused = items.min(cardinality_of(&self.free) - edits_reused);
        let lo = self.high_water;
        let mid = lo + (edits - edits_reused);
        let hi = mid + (items - items_reused);
        if hi > self.low_water {
            return Err(AllocError::Exhausted {
                high_water: lo,
                low_water: self.low_water,
            });
        }
        let mut allocation = Allocation {
            edits: take_lowest(&mut self.free, Tenancy::ZERO, edits_reused),
            items: Vec::with_capacity(items as usize),
        };
        allocation.edits.extend(lo..mid);
        let tenancies: Vec<Tenancy> = self.free.keys().copied().collect();
        for tenancy in tenancies {
            let wanted = items_reused - allocation.items.len() as u64;
            if wanted == 0 {
                break;
            }
            let taken = take_lowest(&mut self.free, tenancy, wanted);
            allocation.items.extend(taken);
        }
        allocation.items.extend(mid..hi);
        self.high_water = hi;
        Ok(allocation)
    }

    /// Hold `ids` back until the log keeps no record older than `position`: a fold's freed ids,
    /// at the position its publication reached, each at the tenancy it is issued at.
    pub fn release_after(&mut self, position: u64, ids: ByTenancy) {
        let mut held = ByTenancy::new();
        absorb(&mut held, ids);
        if !held.is_empty() {
            self.held.push((position, held));
        }
    }

    /// Free every held set whose position the log's oldest kept record, `retained_from`, has
    /// reached. Returns how many ids were freed.
    pub fn promote(&mut self, retained_from: u64) -> u64 {
        let mut freed = 0;
        let mut promoted = Vec::new();
        self.held.retain_mut(|(position, ids)| {
            if *position > retained_from {
                return true;
            }
            freed += cardinality_of(ids);
            promoted.push(std::mem::take(ids));
            false
        });
        for ids in promoted {
            absorb(&mut self.free, ids);
        }
        freed
    }

    /// Seed the freed ids from durable state at open, each set split by the tenancy its ids are
    /// at. The held sets the log has been reclaimed past, to `retained_from`, are freed first,
    /// since a set can be freed and issued after the state was written. Then every id in `named`
    /// is taken out of the free and the held sets alike: an id a kept log record or the overlay
    /// names was issued since, or, in a set still held, is named by a record of its previous
    /// holder, which the replay has applied.
    pub fn seed_freed(
        &mut self,
        free: ByTenancy,
        held: Vec<(u64, ByTenancy)>,
        retained_from: u64,
        named: &Bitmap,
    ) {
        self.free = ByTenancy::new();
        absorb(&mut self.free, free);
        self.held = Vec::new();
        for (position, ids) in held {
            self.release_after(position, ids);
        }
        self.promote(retained_from);
        take_out(&mut self.free, named);
        for (_, ids) in &mut self.held {
            take_out(ids, named);
        }
        self.held.retain(|(_, ids)| !ids.is_empty());
    }

    /// The freed ids issued before the high-water, by tenancy.
    pub fn free(&self) -> &ByTenancy {
        &self.free
    }

    /// The freed ids held back, each set with the position the log must be kept from.
    pub fn held(&self) -> &[(u64, ByTenancy)] {
        &self.held
    }

    /// Allocates `blocks` consecutive, never-before-issued **row-less** [`RESERVED_BLOCK`]s,
    /// counting **down**, and lowers the low-water mark past them. Returns the run in ascending
    /// order, so `ordinal = entity − run.start` is the addressing arithmetic the representation
    /// rests on, whichever direction the mark moved.
    ///
    /// **Whole aligned blocks only, and nothing finer.** A single row-less entity — the one a layer
    /// takes so that layer suppression can ride the deny lane — is handed out by the registry from
    /// inside a block it holds, not by this method. Mixing the two here would leave the mark
    /// unaligned after every single-entity allocation, and the next block would either straddle a
    /// container boundary or discard the fragment; at ten thousand layers, discarding costs an
    /// eighth of the address space. Keeping this method coarse puts the sub-block bookkeeping in
    /// the one place that can do it without waste.
    ///
    /// Refuses, leaving both marks unchanged, when the run would reach the point region. That is
    /// the same collision the upward direction refuses, met from the other side.
    pub fn allocate_rowless(&mut self, blocks: u64) -> Result<Range<u64>, AllocError> {
        let hi = self.low_water;
        let refuse = || AllocError::Exhausted {
            high_water: self.high_water,
            low_water: hi,
        };
        let width = blocks.checked_mul(RESERVED_BLOCK).ok_or_else(refuse)?;
        let lo = hi.checked_sub(width).ok_or_else(refuse)?;
        if lo < self.high_water {
            return Err(refuse());
        }
        self.low_water = lo;
        Ok(lo..hi)
    }

    /// One past the highest point ID this allocator has handed out.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    /// One past the lowest row-less ID this allocator has handed out — so the next row-less run of
    /// `b` blocks is `[low_water − b·RESERVED_BLOCK, low_water)`. Equals [`ROWLESS_CEILING`] when
    /// nothing row-less has been allocated.
    pub fn low_water(&self) -> u64 {
        self.low_water
    }

    /// How many ids remain between the two marks. The honest "space left" figure: neither mark
    /// alone gives it, which is why exhaustion reports both.
    pub fn remaining(&self) -> u64 {
        self.low_water.saturating_sub(self.high_water)
    }
}

/// Computes the entity-ID high-water mark implied by a set of replayed WAL records: the maximum
/// `WalRow.entity_id.raw() + 1` (with overlay-snapshot entries as a weak floor).
///
/// This is the "replayed rows" half of `Allocator::new`'s `max(manifest_hw, replayed rows)`
/// seeding contract — callers should rebuild with
/// `Allocator::new(manifest_hw.max(high_water_from(&replayed)))` rather than carrying a
/// pre-crash `Allocator::high_water()` value across a restart, since the allocator itself does
/// not persist: only what actually made it into the WAL (or the bundle manifest) did.
pub fn high_water_from<'r>(records: impl IntoIterator<Item = &'r WalRecord>) -> u64 {
    let mut hw = 0u64;
    for rec in records {
        match rec {
            WalRecord::IngestBatch { rows, edits, .. } => {
                for row in rows.iter().chain(edits.iter().flat_map(|edit| &edit.rows)) {
                    let candidate = row.entity_id.raw() + 1;
                    if candidate > hw {
                        hw = candidate;
                    }
                }
            }
            // A mint names no entity. It is the one record whose subject is vocabulary space
            // rather than entity space, and the two allocators share nothing (§3.4).
            WalRecord::VocabularyMint { .. } => {}
            // An overlay snapshot names entities that were certainly allocated, so it raises the
            // floor — but only for entities something has *denied*, which is a weak bound and not
            // the mechanism. Rotation deletes the `IngestBatch` records this function really
            // derives from; what replaces them is the side-manifest's own `entity_id_high_water`,
            // refreshed at every flush publication — never the build `MANIFEST.json`, which would
            // reallocate every flushed entity id and violate I9.
            WalRecord::OverlaySnapshot { entries } => {
                for entry in entries {
                    let candidate = entry.entity_id.raw() + 1;
                    if candidate > hw {
                        hw = candidate;
                    }
                }
            }
            WalRecord::ChangeBatch { .. } => {}
            // A row-less allocation moves the *other* mark, and moving this one with it would
            // hand every point id below the row-less region away in a single step. A growth
            // allocates nothing at all — it names an artifact that already has its ordinal and its
            // entity — so it moves neither mark.
            // A view create and a drop allocate nothing at all: a view is a coordinate system
            // and holds no entity, which is what keeps entity space the invariant plane
            // (`views.md` §1).
            WalRecord::LayerCreate { .. }
            | WalRecord::LayerDrop { .. }
            | WalRecord::ArtifactPublish { .. }
            | WalRecord::ArtifactGrow { .. }
            | WalRecord::ViewCreate { .. }
            | WalRecord::ViewDrop { .. } => {}
            // The declarations allocate nothing: a column, a vocabulary and a group hold no
            // entity, and a fill names an artifact that already has its ordinal and its entity.
            WalRecord::ArtifactFill { .. }
            | WalRecord::AttributeDeclare { .. }
            | WalRecord::UniqueDeclare { .. }
            | WalRecord::VocabularyDeclare { .. }
            | WalRecord::ViewGroupCreate { .. }
            | WalRecord::PlainViewCreate { .. } => {}
        }
    }
    hw
}

/// Every point entity `records` name: the rows an ingest allocated or joined, the entities its
/// edits left, every entity a change or an overlay snapshot names, and every member an artifact
/// publication or growth names. [`Allocator::seed_freed`] takes these out of the freed ids.
pub fn entities_named<'r>(records: impl IntoIterator<Item = &'r WalRecord>) -> Bitmap {
    let mut named = Bitmap::new();
    let mut sets: Vec<&Vec<u8>> = Vec::new();
    let mut add = |entity: EntityId| {
        if let Ok(entity) = u32::try_from(entity.raw()) {
            named.add(entity);
        }
    };
    for record in records {
        match record {
            WalRecord::IngestBatch { rows, edits, .. } => {
                for row in rows.iter().chain(edits.iter().flat_map(|edit| &edit.rows)) {
                    add(row.entity_id);
                }
                for edit in edits {
                    add(edit.old);
                }
            }
            WalRecord::ChangeBatch { changes } => {
                for (entity, _) in changes {
                    add(*entity);
                }
            }
            WalRecord::ViewDrop { deleted, .. } => {
                for entity in deleted {
                    add(*entity);
                }
            }
            WalRecord::OverlaySnapshot { entries } => {
                for entry in entries {
                    add(entry.entity_id);
                }
            }
            WalRecord::ArtifactPublish { artifacts, .. } => {
                for artifact in artifacts {
                    sets.push(&artifact.members);
                    sets.extend(artifact.contents.iter().map(|c| &c.generated_from));
                }
            }
            WalRecord::ArtifactGrow { growth, .. } => {
                for grown in growth {
                    sets.extend([&grown.joining, &grown.leaving]);
                }
            }
            _ => {}
        }
    }
    // A set that does not decode names nothing the replay could apply.
    for bytes in sets.into_iter().filter(|bytes| !bytes.is_empty()) {
        if let Some(members) = crate::membership::deserialise_members(bytes) {
            named.or_inplace(&members);
        }
    }
    named
}

/// Computes the row-less low-water mark implied by a set of replayed WAL records: the lowest entity
/// any registration or level extension claimed, or [`ROWLESS_CEILING`] if none did.
///
/// **[`high_water_from`]'s mirror, and it exists for the failure that motivated the two regions.**
/// Without it a rotation and restart reseeds the row-less mark at the ceiling and the next layer
/// registration is handed ids a live layer already holds — two entities sharing a `mosaica_id`,
/// which is the one thing "collision-free by construction" is not allowed to mean sometimes.
///
/// **Two records move this mark, not one.** A [`WalRecord::LayerCreate`] takes the layer's entity
/// and its levels' first blocks; a [`WalRecord::ArtifactPublish`] takes another block whenever a
/// level outgrows its reservation. Reading only the first was the shape this function had while
/// levels could not grow, and leaving it that way once they could would reissue an extension block
/// on the first restart after a large publication — with every artifact in it already suppressible
/// by a `mosaica_id` a caller holds.
///
/// **A drop does not raise it.** The name is tombstoned and the ids are not reclaimed (decision
/// 0072 is settled and unbuilt), so a dropped layer's run must stay below the mark: raising it
/// would reissue exactly the ids whose bookmarks and suppressions are still live.
pub fn low_water_from<'r>(records: impl IntoIterator<Item = &'r WalRecord>) -> u64 {
    let mut lw = ROWLESS_CEILING;
    for rec in records {
        match rec {
            WalRecord::LayerCreate {
                layer_entity, runs, ..
            } => {
                lw = lw.min(layer_entity.raw());
                for level in runs {
                    for run in level.runs() {
                        lw = lw.min(run.start);
                    }
                }
            }
            WalRecord::ArtifactPublish { extend_runs, .. } => {
                for run in extend_runs {
                    lw = lw.min(run.start);
                }
            }
            _ => {}
        }
    }
    lw
}

/// The ceiling a restart seeds its row-less mark from: the lowest of everything durable that could
/// name an allocated row-less id.
///
/// [`allocator_floor`]'s mirror, and it takes the **minimum** where that one takes the maximum —
/// the two regions grow towards each other, so "furthest along" is downward here. The same three
/// homes apply: the bundle manifest, every side manifest, and the WAL term that rotation reclaims
/// ([`low_water_from`]), and only the combination is correct.
pub fn allocator_ceiling(bundle_low_water: u64, side_manifest_low_waters: &[u64]) -> u64 {
    side_manifest_low_waters
        .iter()
        .copied()
        .fold(bundle_low_water, u64::min)
}

/// The floor a restart seeds its allocator from: the highest of everything durable that could
/// name an allocated id.
///
/// **This exists as a function because the fold makes the two manifest values disagree on
/// purpose, and the safety of that is a property of this expression rather than of either
/// writer.** `publish_fold` lowers `MANIFEST.json`'s `entity_id_high_water` to the *snapshot's*
/// entity bound — it has a second reader, the base locator's declared length, for which a live
/// value would claim every post-snapshot entity — and writes the live value into
/// `SEGMENTS-<n>.json` instead. Either field read alone therefore reissues ids: the bundle's
/// because the fold lowered it, the side-manifest's because a build writes none. Only the `max`
/// is correct, and a change on either side has to re-check it here (compaction §12's obligation
/// 10, and `write.rs`'s note at the fold's manifest assembly).
///
/// The WAL term is the third home and the one rotation reclaims, which is why it cannot stand
/// alone either: see [`high_water_from`].
pub fn allocator_floor(bundle_high_water: u64, side_manifest_high_waters: &[u64]) -> u64 {
    side_manifest_high_waters
        .iter()
        .copied()
        .fold(bundle_high_water, u64::max)
}

/// One item awaiting entity-ID assignment at serve time (append ingest).
///
/// `terms` are already-**resolved** `TermId`s — bundle-relative ordinals for descriptors the
/// dictionary already knows, extended with the deterministic in-memory ordinals a session
/// assigns to novel descriptors (see `mosaica_lifecycle::wal`'s module docs). Resolution happens
/// in the caller, not here, so this module stays free of the dictionary/interning machinery.
pub struct PendingItem {
    pub terms: Vec<TermId>,
    /// Filled in by [`assign_sorted`]; `None` beforehand.
    pub entity_id: Option<EntityId>,
    /// Whether the id is an edit's new entity, which takes a freed id only at tenancy 0.
    pub edit: bool,
}

/// The signature-sorted assignment key (§11.1): an item's sorted, deduplicated term-ID list.
///
/// This is a deliberate duplicate of `mosaica_build::signature_sort_key`, not an import: the
/// crate dependency direction runs `mosaica-build → mosaica-lifecycle` (the build pipeline will
/// depend on this crate's allocator), never the reverse, so importing from `mosaica-build` here
/// would invert SA §3's crate graph. The two copies must stay in lockstep — this one operates on
/// resolved `TermId`s exactly as `mosaica_build::signature_sort_key` does, and the logic is three
/// lines by design specifically so keeping them in sync is cheap.
fn signature_sort_key(terms: &[TermId]) -> Vec<u32> {
    let mut key: Vec<u32> = terms.iter().map(|t| t.raw()).collect();
    key.sort_unstable();
    key.dedup();
    key
}

/// Assigns entity IDs to `items` in ascending order of `signature_sort_key`, ties in batch order:
/// the edits' new entities in their own order and the new items in theirs, from one
/// [`Allocator::allocate`]. Items with identical signatures take adjacent ids from the
/// high-water, which is what makes their postings compress as runs; freed ids come first and
/// scatter among other signatures' runs.
///
/// Fallible: propagates [`AllocError::Exhausted`] from the underlying
/// `Allocator::allocate` rather than swallowing it — a batch that would exhaust the entity-ID
/// space has no effect, exactly as `allocate` leaves the allocator unchanged on that error.
pub fn assign_sorted(items: &mut [PendingItem], alloc: &mut Allocator) -> Result<(), AllocError> {
    // Compute each item's signature sort key once, up front, rather than inside
    // the comparator — `sort_by`'s comparator can be called O(n log n) times, and
    // `signature_sort_key` allocates, so recomputing it per-comparison would be O(n log n)
    // allocations instead of O(n).
    //
    // **A row that arrives with an entity is a join** (`views.md` §4): the same document in a
    // second view, whose identity was decided when it was first ingested. It takes no id and no
    // rank — allocating one would mint a second entity for one document, and the join rule exists
    // precisely so that cannot happen — so the allocation is sized by the rows that need one.
    let mut edits: Vec<(usize, Vec<u32>)> = Vec::new();
    let mut created: Vec<(usize, Vec<u32>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if item.entity_id.is_none() {
            let keyed = (i, signature_sort_key(&item.terms));
            if item.edit {
                edits.push(keyed);
            } else {
                created.push(keyed);
            }
        }
    }
    // Stable, so items with one signature keep their order in the batch.
    edits.sort_by(|(_, ka), (_, kb)| ka.cmp(kb));
    created.sort_by(|(_, ka), (_, kb)| ka.cmp(kb));

    let ids = alloc.allocate(edits.len() as u64, created.len() as u64)?;
    let assigned = edits
        .into_iter()
        .zip(ids.edits)
        .chain(created.into_iter().zip(ids.items));
    for ((idx, _), id) in assigned {
        items[idx].entity_id = Some(EntityId::new(id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ids` at `tenancy`.
    fn at(tenancy: u16, ids: &[u32]) -> ByTenancy {
        ByTenancy::from([(Tenancy::new(tenancy).unwrap(), Bitmap::of(ids))])
    }

    fn listed(sets: &ByTenancy) -> Vec<(u16, Vec<u32>)> {
        sets.iter()
            .map(|(tenancy, ids)| (tenancy.raw(), ids.iter().collect()))
            .collect()
    }

    #[test]
    fn allocate_is_monotone_without_freed_ids() {
        let mut alloc = Allocator::new(10);
        let a = alloc.allocate(0, 3).unwrap();
        let b = alloc.allocate(2, 3).unwrap();
        assert_eq!(a.items, (10..13).collect::<Vec<_>>());
        assert_eq!((b.edits, b.items), (vec![13, 14], vec![15, 16, 17]));
        assert_eq!(alloc.high_water(), 18);
    }

    #[test]
    fn freed_ids_are_issued_lowest_first_and_once() {
        let mut alloc = Allocator::new(100);
        alloc.release_after(7, at(0, &[40, 12, 90]));
        assert_eq!(
            alloc.allocate(0, 2).unwrap().items,
            vec![100, 101],
            "held ids are not issued"
        );
        assert_eq!(
            alloc.promote(6),
            0,
            "the log still keeps a record older than 7"
        );
        assert_eq!(alloc.promote(7), 3);
        assert_eq!(alloc.allocate(0, 2).unwrap().items, vec![12, 40]);
        assert_eq!(alloc.allocate(0, 3).unwrap().items, vec![90, 102, 103]);
        assert_eq!(alloc.high_water(), 104);
        assert!(alloc.free().is_empty());
    }

    #[test]
    fn edits_take_tenancy_zero_and_new_items_the_lowest_tenancy_first() {
        let mut alloc = Allocator::new(100);
        alloc.release_after(1, at(0, &[40, 12]));
        alloc.release_after(1, at(3, &[2]));
        alloc.release_after(2, at(1, &[30, 5]));
        alloc.promote(2);
        let given = alloc.allocate(3, 4).unwrap();
        assert_eq!(
            given.edits,
            vec![12, 40, 100],
            "an edit takes tenancy 0, then the high-water"
        );
        assert_eq!(
            given.items,
            vec![5, 30, 2, 101],
            "a new item takes the lowest tenancy, lowest id first, then the high-water"
        );
        assert!(alloc.free().is_empty());

        alloc.release_after(3, at(0, &[7]));
        alloc.release_after(3, at(2, &[9]));
        alloc.promote(3);
        let given = alloc.allocate(0, 1).unwrap();
        assert_eq!(
            given.items,
            vec![7],
            "tenancy 0 comes first for a new item too"
        );
        let given = alloc.allocate(1, 0).unwrap();
        assert_eq!(
            given.edits,
            vec![102],
            "an edit takes no id above tenancy 0"
        );
        assert_eq!(listed(alloc.free()), vec![(2, vec![9])]);
    }

    #[test]
    fn a_refused_allocation_takes_no_freed_id() {
        let mut alloc = Allocator::with_marks(ROWLESS_CEILING - 1, ROWLESS_CEILING);
        alloc.release_after(0, at(0, &[5]));
        alloc.release_after(0, at(4, &[3]));
        alloc.promote(0);
        assert!(matches!(
            alloc.allocate(1, 3),
            Err(AllocError::Exhausted { .. })
        ));
        assert_eq!(listed(alloc.free()), vec![(0, vec![5]), (4, vec![3])]);
        assert_eq!(alloc.high_water(), ROWLESS_CEILING - 1);
        let given = alloc.allocate(1, 2).unwrap();
        assert_eq!(
            (given.edits, given.items),
            (vec![5], vec![3, ROWLESS_CEILING - 1])
        );
    }

    #[test]
    fn seeding_drops_every_id_something_names_and_keeps_each_tenancy() {
        let mut alloc = Allocator::new(50);
        let mut free = at(0, &[3, 4]);
        free.extend(at(2, &[9]));
        alloc.seed_freed(
            free,
            vec![
                (12, at(0, &[20, 21])),
                (10, at(1, &[30, 31])),
                (13, ByTenancy::new()),
                (14, at(5, &[40])),
            ],
            11,
            &Bitmap::of(&[4, 21, 31, 40]),
        );
        assert_eq!(
            listed(alloc.free()),
            vec![(0, vec![3]), (1, vec![30]), (2, vec![9])],
            "a set the log was reclaimed past is freed, and a named id is taken out of it too"
        );
        assert_eq!(
            alloc
                .held()
                .iter()
                .map(|(p, ids)| (*p, listed(ids)))
                .collect::<Vec<_>>(),
            vec![(12, vec![(0, vec![20])])],
            "a named id leaves a set still held, and an emptied or empty set is dropped"
        );
    }

    #[test]
    fn the_allocator_refuses_to_issue_a_point_id_that_reaches_the_rowless_region() {
        // Without the cap, `allocate` would be `lo + n` on a u64 and "collision-free by
        // construction" would rest on the corpus happening to stay small. Past 2^32 two entities
        // would share a mosaica_id and `invert` would return the WRONG one.
        let mut a = Allocator::new(ROWLESS_CEILING - 2);
        assert!(a.allocate(0, 1).is_ok());
        assert!(matches!(
            a.allocate(5, 5),
            Err(AllocError::Exhausted { .. })
        ));
        // The failed call must not have moved the high-water mark (the batch has no effect).
        assert_eq!(a.high_water(), ROWLESS_CEILING - 1);

        // And the seed itself: a manifest high-water past the bound is refused at open, not
        // silently carried into the first ingest.
        assert!(Allocator::try_new(1u64 << 33).is_err());
        assert!(Allocator::try_new(u32::MAX as u64).is_err());
        assert!(Allocator::try_new(ROWLESS_CEILING).is_err());
        assert!(Allocator::try_new(ROWLESS_CEILING - 1).is_ok());
    }

    #[test]
    fn the_point_ceiling_is_a_block_boundary_and_the_fragment_is_unusable() {
        // Stated as a test because the loss is deliberate and small — one block out of 65 536 — and
        // a later change that "recovers" it would put the first row-less block across a container
        // boundary, which is the cost alignment exists to avoid.
        assert_eq!(ROWLESS_CEILING % RESERVED_BLOCK, 0);
        assert_eq!(ENTITY_ID_CEILING - ROWLESS_CEILING, RESERVED_BLOCK - 1);
    }

    #[test]
    fn rowless_ids_count_down_in_aligned_blocks_and_never_meet_a_point() {
        let mut a = Allocator::new(0);
        let first = a.allocate_rowless(1).unwrap();
        let second = a.allocate_rowless(2).unwrap();

        // Downward, contiguous, and each run block-aligned at both ends.
        assert_eq!(first, ROWLESS_CEILING - RESERVED_BLOCK..ROWLESS_CEILING);
        assert_eq!(second, first.start - 2 * RESERVED_BLOCK..first.start);
        for run in [&first, &second] {
            assert_eq!(run.start % RESERVED_BLOCK, 0);
            assert_eq!((run.end - run.start) % RESERVED_BLOCK, 0);
        }
        assert_eq!(a.low_water(), second.start);
        // The point mark has not moved: the two regions are independent until they meet.
        assert_eq!(a.high_water(), 0);
        assert_eq!(a.remaining(), second.start);
    }

    #[test]
    fn exhaustion_is_the_two_marks_meeting_from_either_side() {
        // One block of space between them, and both directions must see the same wall.
        let mut a = Allocator::with_marks(ROWLESS_CEILING - RESERVED_BLOCK, ROWLESS_CEILING);
        assert_eq!(a.remaining(), RESERVED_BLOCK);

        // Downward: one block fits exactly, a second does not.
        let mut down = Allocator::with_marks(a.high_water(), a.low_water());
        assert!(down.allocate_rowless(1).is_ok());
        assert_eq!(down.remaining(), 0);
        assert!(matches!(
            down.allocate_rowless(1),
            Err(AllocError::Exhausted { .. })
        ));

        // Upward: the same wall, met from the other side, and the refusal reports both marks so
        // "how much is left" is answerable from the error alone.
        assert!(a.allocate(0, RESERVED_BLOCK).is_ok());
        match a.allocate(0, 1) {
            Err(AllocError::Exhausted {
                high_water,
                low_water,
            }) => {
                assert_eq!(high_water, ROWLESS_CEILING);
                assert_eq!(low_water, ROWLESS_CEILING);
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }

        // And a request whose *width* overflows a u64 is refused rather than wrapping into a run
        // that looks valid.
        let mut wide = Allocator::new(0);
        assert!(matches!(
            wide.allocate_rowless(u64::MAX),
            Err(AllocError::Exhausted { .. })
        ));
        assert_eq!(wide.low_water(), ROWLESS_CEILING);
    }

    fn layer_create(entity: u64, run_start: u64) -> WalRecord {
        use mosaica_types::layer::{
            EntityRun, Hierarchy, HierarchyKind, MembershipSource, ReservedRuns,
        };
        WalRecord::LayerCreate {
            declaration: Box::new(mosaica_types::layer::LayerDeclaration {
                scope: Default::default(),
                name: format!("l{entity}"),
                title: Some("l".into()),
                views: Vec::new(),
                membership: MembershipSource::Enumerated,
                value_set: Default::default(),
                visibility: None,
                artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
                require_member_visibility: None,
                hierarchy: Hierarchy {
                    kind: HierarchyKind::Flat,
                    prune_children: false,
                },
                content: Default::default(),
                depends_on: Vec::new(),
                levels: Vec::new(),
                layout: None,
                shape: None,
            }),
            layer_entity: EntityId::new(entity),
            runs: vec![ReservedRuns::from_runs(vec![EntityRun {
                start: run_start,
                end: run_start + RESERVED_BLOCK,
            }])],
            version: 1,
        }
    }

    #[test]
    fn low_water_from_recovers_the_rowless_mark_a_rotation_would_otherwise_lose() {
        // The hazard decision 0074 names as the part to get right: a layer allocation that raised
        // no durable mark survives a rotation and restart only until the next allocation reissues
        // its ids — two entities, one mosaica_id.
        assert_eq!(low_water_from(&[]), ROWLESS_CEILING);

        let a = layer_create(ROWLESS_CEILING - 1, ROWLESS_CEILING - RESERVED_BLOCK);
        let b = layer_create(ROWLESS_CEILING - 2, ROWLESS_CEILING - 3 * RESERVED_BLOCK);
        assert_eq!(
            low_water_from(&[a.clone(), b.clone()]),
            ROWLESS_CEILING - 3 * RESERVED_BLOCK,
            "the lowest run start any registration claimed"
        );
        // Order-independent, like its upward mirror: a later, higher record must not pull the mark
        // back up.
        assert_eq!(
            low_water_from(&[b, a.clone()]),
            ROWLESS_CEILING - 3 * RESERVED_BLOCK
        );

        // A drop does not raise it. The name is tombstoned and the ids stay spent (decision 0072
        // is settled and unbuilt), so raising the mark would reissue exactly the ids whose
        // bookmarks and suppressions are still live.
        assert_eq!(
            low_water_from(&[
                a,
                WalRecord::LayerDrop {
                    name: "l4294901758".into(),
                    version: 2,
                }
            ]),
            ROWLESS_CEILING - RESERVED_BLOCK
        );
    }

    #[test]
    fn the_two_marks_are_recovered_from_the_same_records_without_touching_each_other() {
        // One log carrying both populations. Each function must read only its own records — a
        // row-less allocation moving the point mark would hand away every id below the row-less
        // region in a single step, and a row is not evidence about the row-less mark at all.
        let log = vec![
            row(5),
            layer_create(ROWLESS_CEILING - 1, ROWLESS_CEILING - RESERVED_BLOCK),
            row(11),
        ];
        assert_eq!(high_water_from(&log), 12);
        assert_eq!(low_water_from(&log), ROWLESS_CEILING - RESERVED_BLOCK);

        // And the pair reseeds an allocator that then refuses to reissue either.
        let mut a = Allocator::try_with_marks(high_water_from(&log), low_water_from(&log)).unwrap();
        assert!(a.allocate(0, 1).unwrap().items[0] >= 12);
        assert!(a.allocate_rowless(1).unwrap().end <= ROWLESS_CEILING - RESERVED_BLOCK);
    }

    #[test]
    fn allocator_ceiling_takes_the_minimum_where_the_floor_takes_the_maximum() {
        // The two regions grow towards each other, so "furthest along" is downward here. Reading
        // either mark from one home alone reissues ids, which is why both functions exist.
        assert_eq!(allocator_floor(10, &[7, 42, 3]), 42);
        assert_eq!(allocator_ceiling(ROWLESS_CEILING, &[]), ROWLESS_CEILING);
        assert_eq!(
            allocator_ceiling(ROWLESS_CEILING, &[ROWLESS_CEILING - 8, ROWLESS_CEILING - 2]),
            ROWLESS_CEILING - 8
        );
    }

    #[test]
    fn signature_sort_key_matches_mosaica_build_semantics() {
        // Same three-line rule as mosaica_build::signature_sort_key: sorted, deduplicated.
        let key = signature_sort_key(&[TermId::new(9), TermId::new(2), TermId::new(2)]);
        assert_eq!(key, vec![2, 9]);
    }

    fn row(entity_id: u64) -> WalRecord {
        WalRecord::IngestBatch {
            edits: Vec::new(),
            receipt: Vec::new(),
            batch_id: "b".into(),
            body_hash: [0u8; 32],
            rows: vec![crate::wal::WalRow {
                entity_id: EntityId::new(entity_id),
                view: "default".to_string(),
                join: false,
                descriptors: Vec::new(),
                x: 0.0,
                y: 0.0,
                scalars: Vec::new(),
                scoped: Vec::new(),
            }],
        }
    }

    #[test]
    fn high_water_from_takes_the_max_over_rows() {
        assert_eq!(high_water_from(&[]), 0);
        // A row with entity_id 5 means IDs 0..=5 are taken, so the next free ID is 6.
        assert_eq!(high_water_from(&[row(5)]), 6);
        // A later, lower row must not pull the high-water mark backwards.
        assert_eq!(high_water_from(&[row(5), row(3)]), 6);
        // Change records carry no entity-ID information beyond an entity already allocated.
        assert_eq!(
            high_water_from(&[WalRecord::ChangeBatch {
                changes: vec![(EntityId::new(1), crate::wal::ChangeOp::Delete)],
            }]),
            0
        );
    }
}
