//! The entity-ID allocator, seeded from durable state at boot.
//!
//! `Allocator::new` is seeded from `max(the shard record's high_water, replayed rows)`: the highest
//! entity ID any durable record has claimed. Above that mark no id has been issued, and as long as
//! the seed never goes backwards across a restart, no id above it is handed out twice.
//!
//! ## Freed ids
//!
//! An edit moves an item to a new entity and keeps its number, the entity it was first given,
//! from which its `mosaica_id` is taken. The entity it leaves is deleted, and the fold that
//! removes its rows frees it at tenancy 0. A deleted item's number is freed by the fold that
//! removes the number's last entity, one tenancy higher than it was, so the item that takes it next
//! has a `mosaica_id` no earlier holder had; a number at [`Tenancy::MAX`] is retired instead and
//! never freed. The tenancy index records each number's tenancy (`mosaica_store::tenancy`), and
//! the free and held sets here are kept by tenancy.
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

use std::collections::BTreeMap;

use croaring::Bitmap;
use mosaica_types::{
    check_room, EntityId, ShardFull, ShardId, Target, Tenancy, TermId, ENTITY_END,
};

use crate::wal::WalRecord;

/// Freed point ids by the tenancy each is at. A tenancy holding no id has no entry.
pub type ByTenancy = BTreeMap<Tenancy, Bitmap>;

/// Every id of `sets`, whatever its tenancy.
pub fn union_of(sets: &ByTenancy) -> Bitmap {
    let mut all = Bitmap::new();
    for ids in sets.values() {
        all.or_inplace(ids);
    }
    all
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

/// The entity-ID allocator: the high water, the freed ids it issues first, and the freed ids it
/// holds back.
pub struct Allocator {
    high_water: u64,
    /// The shard's size, which `high_water` may not pass.
    size: u64,
    /// Freed point ids, issued before any id from `high_water` (module doc).
    free: ByTenancy,
    /// Freed point ids held back until the log keeps no record older than the position beside
    /// them.
    held: Vec<(u64, ByTenancy)>,
}

impl Allocator {
    /// Seeds the high water at `high_water`, the caller's `max(manifest_hw, replayed rows)`. No
    /// size bounds it until [`Allocator::set_size`] gives one. Unchecked: prefer
    /// [`Allocator::try_new`] wherever the seed has not already been validated.
    pub fn new(high_water: u64) -> Self {
        Allocator {
            high_water,
            size: u64::MAX,
            free: ByTenancy::new(),
            held: Vec::new(),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Set the shard's size. The caller checks it against [`Allocator::high_water`] with
    /// [`mosaica_types::check_shard_size`].
    pub fn set_size(&mut self, size: u64) {
        self.size = size;
    }

    /// How many of a window's `edits` and `items` take a freed id, in [`Allocator::allocate`]'s
    /// order: an edit takes one at tenancy 0, and a new item one at any tenancy that is left.
    fn reused(&self, edits: u64, items: u64) -> (u64, u64) {
        let at_zero = self.free.get(&Tenancy::ZERO).map_or(0, Bitmap::cardinality);
        let edits_reused = edits.min(at_zero);
        let items_reused = items.min(cardinality_of(&self.free) - edits_reused);
        (edits_reused, items_reused)
    }

    /// Whether a window of `edits` edits and `items` new items fits: the ids it takes from the
    /// high water, after the freed ids, may pass neither the size nor [`ENTITY_END`]. Held ids are
    /// not counted.
    pub fn room(&self, edits: u64, items: u64) -> Result<(), ShardFull> {
        let (edits_reused, items_reused) = self.reused(edits, items);
        let needed = (edits - edits_reused) + (items - items_reused);
        check_room(self.size, ENTITY_END, self.high_water, needed)
    }

    /// [`Allocator::new`], or `None` for a seed past [`ENTITY_END`]: a durable mark already past
    /// it is caught at open rather than at the first allocation.
    pub fn try_new(high_water: u64) -> Option<Self> {
        (high_water <= ENTITY_END).then(|| Allocator::new(high_water))
    }

    /// Allocates entity IDs for a commit window's `edits` new entities and its `items` new items,
    /// in the order the module doc gives. The high-water advances past every id it gives. Refuses,
    /// leaving the allocator unchanged, where [`Allocator::room`] does.
    pub fn allocate(&mut self, edits: u64, items: u64) -> Result<Allocation, ShardFull> {
        self.room(edits, items)?;
        let (edits_reused, items_reused) = self.reused(edits, items);
        let lo = self.high_water;
        let mid = lo + (edits - edits_reused);
        let hi = mid + (items - items_reused);
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
    pub fn release_after(&mut self, position: u64, mut ids: ByTenancy) {
        ids.retain(|_, held| !held.is_empty());
        if !ids.is_empty() {
            self.held.push((position, ids));
        }
    }

    /// Free every held set whose position the log's oldest kept record, `retained_from`, has
    /// reached. Returns how many ids were freed.
    pub fn promote(&mut self, retained_from: u64) -> u64 {
        let mut freed = 0;
        self.held.retain_mut(|(position, ids)| {
            if *position > retained_from {
                return true;
            }
            freed += cardinality_of(ids);
            absorb(&mut self.free, std::mem::take(ids));
            false
        });
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
        self.free = free;
        self.free.retain(|_, ids| !ids.is_empty());
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

    /// One past the highest ID this allocator has handed out.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

}

/// Raise each shard's mark in `hw` to the highest `entity_id + 1` a row of `record` names in that
/// shard; an overlay snapshot's items are a weak floor.
pub fn raise_high_water(record: &WalRecord, hw: &mut BTreeMap<ShardId, u64>) {
    let mut raise = |shard: ShardId, entity: EntityId| {
        let mark = hw.entry(shard).or_insert(0);
        *mark = (*mark).max(entity.raw() + 1);
    };
    match record {
        WalRecord::IngestBatch { rows, edits, .. } => {
            for row in rows.iter().chain(edits.iter().flat_map(|edit| &edit.rows)) {
                raise(row.shard, row.entity_id);
            }
        }
        // A mint names no entity. It is the one record whose subject is vocabulary space
        // rather than entity space, and the two allocators share nothing.
        WalRecord::VocabularyMint { .. } => {}
        // An overlay snapshot names entities that were certainly allocated, so it raises the
        // floor — but only for entities something has *denied*, which is a weak bound and not
        // the mechanism. Rotation deletes the `IngestBatch` records this function really
        // derives from; what replaces them is the shard record's `high_water` in the
        // side-manifest, refreshed at every publication.
        WalRecord::OverlaySnapshot { entries } => {
            for entry in entries {
                if let Target::Item(shard, entity) = entry.target {
                    raise(shard, entity);
                }
            }
        }
        WalRecord::ChangeBatch { .. } => {}
        // A layer's and its artifacts' entities are in its layer space, not a point shard's.
        // A view create and a drop allocate nothing at all: a view is a coordinate system
        // and holds no entity, which is what keeps entity space the invariant plane.
        WalRecord::LayerCreate { .. }
        | WalRecord::LayerDrop { .. }
        | WalRecord::ArtifactPublish { .. }
        | WalRecord::ArtifactGrow { .. }
        | WalRecord::ViewCreate { .. }
        | WalRecord::ViewDrop { .. } => {}
        // The declarations allocate nothing: a column, a vocabulary and a group hold no
        // entity, and a fill names an artifact that already has its ordinal and its entity. A
        // shard opens at high water 0, and a seal moves no mark.
        WalRecord::ArtifactFill { .. }
        | WalRecord::AttributeDeclare { .. }
        | WalRecord::UniqueDeclare { .. }
        | WalRecord::VocabularyDeclare { .. }
        | WalRecord::ViewGroupCreate { .. }
        | WalRecord::PlainViewCreate { .. }
        | WalRecord::ShardsDeclare { .. }
        | WalRecord::ShardOpen { .. }
        | WalRecord::ShardSeal { .. } => {}
    }
}

/// Add each point entity `record` names to its shard's set in `named`: rows, edits' old entities,
/// items a change, view drop or snapshot names, and artifact members and generating sets.
/// [`Allocator::seed_freed`] takes each shard's out of that shard's freed ids.
pub fn name_entities(record: &WalRecord, named: &mut BTreeMap<ShardId, Bitmap>) {
    let mut sets: Vec<&[crate::wal::WalShardSet]> = Vec::new();
    let mut add = |target: Target| {
        if let Target::Item(shard, entity) = target {
            if let Ok(entity) = u32::try_from(entity.raw()) {
                named.entry(shard).or_default().add(entity);
            }
        }
    };
    match record {
        WalRecord::IngestBatch { rows, edits, .. } => {
            for row in rows.iter().chain(edits.iter().flat_map(|edit| &edit.rows)) {
                add(Target::Item(row.shard, row.entity_id));
            }
            for edit in edits {
                add(Target::Item(edit.shard, edit.old));
            }
        }
        WalRecord::ChangeBatch { changes } => {
            for (target, _) in changes {
                add(*target);
            }
        }
        WalRecord::ViewDrop { deleted, .. } => {
            for target in deleted {
                add(*target);
            }
        }
        WalRecord::OverlaySnapshot { entries } => {
            for entry in entries {
                add(entry.target);
            }
        }
        WalRecord::ArtifactPublish { artifacts, .. } => {
            for artifact in artifacts {
                sets.push(&artifact.members);
                sets.extend(
                    artifact
                        .contents
                        .iter()
                        .map(|c| c.generated_from.as_slice()),
                );
            }
        }
        WalRecord::ArtifactGrow { growth, .. } => {
            for grown in growth {
                sets.extend([grown.joining.as_slice(), grown.leaving.as_slice()]);
            }
        }
        _ => {}
    }
    // A set that does not decode names nothing the replay could apply.
    for logged in sets {
        if let Some(members) = crate::membership::ShardSets::from_wal(logged) {
            for (shard, set) in members.iter() {
                named.entry(shard).or_default().or_inplace(set);
            }
        }
    }
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
    /// Whether the id is an edit's new entity.
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

/// Assigns entity IDs to `items` from one [`Allocator::allocate`], the edits' new entities and the
/// new items each in ascending order of `signature_sort_key`, ties in batch order. Items with
/// identical signatures take adjacent ids from the high-water, which is what makes their postings
/// compress as runs; freed ids come first and scatter among other signatures' runs.
///
/// Fails, leaving the allocator unchanged, where [`Allocator::allocate`] does.
pub fn assign_sorted(items: &mut [PendingItem], alloc: &mut Allocator) -> Result<(), ShardFull> {
    // Compute each item's signature sort key once, up front, rather than inside
    // the comparator — `sort_by`'s comparator can be called O(n log n) times, and
    // `signature_sort_key` allocates, so recomputing it per-comparison would be O(n log n)
    // allocations instead of O(n).
    //
    // **A row that arrives with an entity is a join** (`views.md` §4): the same document in a
    // second view, whose identity was decided when it was first ingested. It takes no id and no
    // rank — allocating one would mint a second entity for one document, and the join rule exists
    // precisely so that cannot happen — so the allocation is sized by the rows that need one.
    let mut keyed: Vec<(bool, Vec<u32>, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.entity_id.is_none())
        .map(|(i, item)| (!item.edit, signature_sort_key(&item.terms), i))
        .collect();
    // Stable, so items with one signature keep their order in the batch.
    keyed.sort_by(|(ca, ka, _), (cb, kb, _)| (ca, ka).cmp(&(cb, kb)));
    let edits = keyed.iter().take_while(|(created, ..)| !created).count();

    let ids = alloc.allocate(edits as u64, (keyed.len() - edits) as u64)?;
    let ids = ids.edits.into_iter().chain(ids.items);
    for ((_, _, idx), id) in keyed.into_iter().zip(ids) {
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
        let mut alloc = Allocator::new(ENTITY_END - 1);
        alloc.release_after(0, at(0, &[5]));
        alloc.release_after(0, at(4, &[3]));
        alloc.promote(0);
        assert!(alloc.allocate(1, 3).is_err());
        assert_eq!(listed(alloc.free()), vec![(0, vec![5]), (4, vec![3])]);
        assert_eq!(alloc.high_water(), ENTITY_END - 1);
        let given = alloc.allocate(1, 2).unwrap();
        assert_eq!(
            (given.edits, given.items),
            (vec![5], vec![3, ENTITY_END - 1])
        );
    }

    #[test]
    fn freed_ids_come_before_the_size_and_held_ids_do_not() {
        let mut alloc = Allocator::new(8);
        alloc.set_size(10);
        alloc.release_after(1, at(0, &[2]));
        alloc.release_after(1, at(1, &[3]));
        alloc.release_after(5, at(0, &[4]));
        alloc.promote(1);
        let full = |needed| {
            Err(ShardFull {
                limit: mosaica_types::ShardLimit::Size(10),
                high_water: 8,
                needed,
            })
        };
        assert_eq!(
            alloc.room(1, 3),
            Ok(()),
            "the edit takes 2, two items take 3 and 8, and one 9"
        );
        assert_eq!(alloc.room(1, 4), full(3), "the held 4 is not counted");
        assert_eq!(alloc.room(3, 0), Ok(()));
        assert_eq!(
            alloc.room(4, 0),
            full(3),
            "an edit takes no id above tenancy 0"
        );
        assert_eq!(alloc.allocate(1, 4), Err(full(3).unwrap_err()));
        assert_eq!(alloc.high_water(), 8, "a refused window takes nothing");
        assert_eq!(listed(alloc.free()), vec![(0, vec![2]), (1, vec![3])]);
        let given = alloc.allocate(1, 3).unwrap();
        assert_eq!((given.edits, given.items), (vec![2], vec![3, 8, 9]));
        assert_eq!(
            alloc.room(0, 1),
            full(1).map_err(|f| ShardFull {
                high_water: 10,
                ..f
            })
        );
        alloc.set_size(11);
        assert_eq!(alloc.allocate(0, 1).unwrap().items, vec![10]);
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
    fn a_shard_issues_ids_to_the_end_of_its_space_and_none_past_it() {
        let mut a = Allocator::new(ENTITY_END - 2);
        assert_eq!(a.allocate(0, 2).unwrap().items, vec![ENTITY_END - 2, ENTITY_END - 1]);
        assert_eq!(
            a.allocate(0, 1),
            Err(ShardFull {
                limit: mosaica_types::ShardLimit::End(ENTITY_END),
                high_water: ENTITY_END,
                needed: 1,
            })
        );
        assert_eq!(a.high_water(), ENTITY_END, "a refused window takes nothing");

        assert!(Allocator::try_new(ENTITY_END).is_some());
        assert!(Allocator::try_new(ENTITY_END + 1).is_none());
    }

    #[test]
    fn signature_sort_key_matches_mosaica_build_semantics() {
        // Same three-line rule as mosaica_build::signature_sort_key: sorted, deduplicated.
        let key = signature_sort_key(&[TermId::new(9), TermId::new(2), TermId::new(2)]);
        assert_eq!(key, vec![2, 9]);
    }

    fn row(shard: u32, entity_id: u64) -> WalRecord {
        WalRecord::IngestBatch {
            edits: Vec::new(),
            receipt: Vec::new(),
            batch_id: "b".into(),
            body_hash: [0u8; 32],
            rows: vec![crate::wal::WalRow {
                shard: ShardId::new(shard).unwrap(),
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

    fn marks(marks: &[(u32, u64)]) -> BTreeMap<ShardId, u64> {
        marks
            .iter()
            .map(|&(shard, mark)| (ShardId::new(shard).unwrap(), mark))
            .collect()
    }

    fn high_water_from(records: &[WalRecord]) -> BTreeMap<ShardId, u64> {
        let mut hw = BTreeMap::new();
        for record in records {
            raise_high_water(record, &mut hw);
        }
        hw
    }

    #[test]
    fn high_water_from_takes_the_max_over_rows() {
        assert_eq!(high_water_from(&[]), marks(&[]));
        // A row with entity_id 5 means IDs 0..=5 are taken, so the next free ID is 6.
        assert_eq!(high_water_from(&[row(0, 5)]), marks(&[(0, 6)]));
        // A later, lower row must not pull the high-water mark backwards.
        assert_eq!(high_water_from(&[row(0, 5), row(0, 3)]), marks(&[(0, 6)]));
        // Each shard's mark is its own rows'.
        assert_eq!(
            high_water_from(&[row(0, 5), row(1, 3)]),
            marks(&[(0, 6), (1, 4)])
        );
        // Change records carry no entity-ID information beyond an entity already allocated.
        assert_eq!(
            high_water_from(&[WalRecord::ChangeBatch {
                changes: vec![(
                    mosaica_types::Target::Item(mosaica_types::ShardId::ZERO, EntityId::new(1)),
                    crate::wal::ChangeOp::Delete,
                )],
            }]),
            marks(&[])
        );
    }

    #[test]
    fn each_entity_is_named_in_its_own_shard() {
        let one = ShardId::new(1).unwrap();
        let mut named = BTreeMap::new();
        for record in &[
            row(1, 5),
            WalRecord::ViewDrop {
                view: mosaica_types::view::DeadIncarnation {
                    group: "g".into(),
                    key: "k".into(),
                    incarnation: 0,
                },
                deleted: vec![Target::Item(ShardId::ZERO, EntityId::new(7))],
            },
        ] {
            name_entities(record, &mut named);
        }
        assert_eq!(
            named,
            BTreeMap::from([(ShardId::ZERO, Bitmap::of(&[7])), (one, Bitmap::of(&[5]))])
        );
    }
}
