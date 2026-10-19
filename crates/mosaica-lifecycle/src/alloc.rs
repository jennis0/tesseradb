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
//! A commit window draws its edits' new entities first, each in its item's shard from tenancy 0,
//! lowest id first, then from that shard's high water: an edit's entity carries its item's
//! `mosaica_id` and never becomes a number, so it takes no id an item has held as its number. Its
//! new items then go where `place` says: into the largest pool of freed ids, or the open shard's
//! freed ids and then its high water, lowest tenancy, lowest id first. A sealed shard's tenancy-0
//! ids are its own edits' alone. Freed ids are issued before any id from a high water, so churn
//! reuses a bounded set of ids rather than growing the id space and everything sized by it. A
//! suppressed entity is freed like any other: the fold that removes it drops its suppression with
//! it.
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
    check_room, reached_size, reuse_min_free, sealing_high_water, EntityId, ItemAt, ShardFull,
    ShardId, ShardState, ShardsExhausted, Target, Tenancy, TermId, ENTITY_END,
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

/// The lowest `n` ids of `sets` at `tenancy`, taken out of it and added to `taken` at it.
fn take_lowest(sets: &mut ByTenancy, tenancy: Tenancy, n: u64, taken: &mut ByTenancy) -> Vec<u64> {
    let Some(ids) = sets.get_mut(&tenancy) else {
        return Vec::new();
    };
    let lowest: Vec<u32> = ids.iter().take(n as usize).collect();
    if let Some(&last) = lowest.last() {
        ids.remove_range(..=last);
        taken.entry(tenancy).or_default().add_many(&lowest);
    }
    if ids.is_empty() {
        sets.remove(&tenancy);
    }
    lowest.into_iter().map(u64::from).collect()
}

/// Why a window's ids cannot be given. Either way the allocators are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// An edit's shard has no tenancy-0 id left and its high water is at [`ENTITY_END`].
    Full(ShardFull),
    /// New items need a shard past the last a bundle holds.
    ShardsExhausted(ShardsExhausted),
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocError::Full(full) => full.fmt(f),
            AllocError::ShardsExhausted(exhausted) => exhausted.fmt(f),
        }
    }
}

impl std::error::Error for AllocError {}

/// The entity-ID allocator of one point shard: its high water, whether it is open, the freed ids
/// it issues first, and the freed ids it holds back.
pub struct Allocator {
    high_water: u64,
    state: ShardState,
    /// Freed point ids, issued before any id from `high_water` (module doc).
    free: ByTenancy,
    /// Freed point ids held back until the log keeps no record older than the position beside
    /// them.
    held: Vec<(u64, ByTenancy)>,
}

impl Allocator {
    /// An open shard's allocator, its high water at `high_water`, the caller's `max(manifest_hw,
    /// replayed rows)`. Unchecked: prefer [`Allocator::try_new`] wherever the seed has not already
    /// been validated.
    pub fn new(high_water: u64) -> Self {
        Allocator {
            high_water,
            state: ShardState::Open,
            free: ByTenancy::new(),
            held: Vec::new(),
        }
    }

    /// [`Allocator::new`], or `None` for a seed past [`ENTITY_END`]: a durable mark already past
    /// it is caught at open rather than at the first allocation.
    pub fn try_new(high_water: u64) -> Option<Self> {
        (high_water <= ENTITY_END).then(|| Allocator::new(high_water))
    }

    pub fn state(&self) -> ShardState {
        self.state
    }

    /// From here on the shard gives new items only ids freed at tenancy 1 or more.
    pub fn seal(&mut self) {
        self.state = ShardState::Sealed;
    }

    /// Undo a window that did not commit: the freed ids it took return at their tenancies, the
    /// high water falls back to where the window found it, and a seal it made is lifted.
    pub fn give_back(&mut self, taken: Taken) {
        absorb(&mut self.free, taken.free);
        self.high_water = taken.high_water;
        self.state = taken.state;
    }

    fn at_zero(&self) -> u64 {
        self.free.get(&Tenancy::ZERO).map_or(0, Bitmap::cardinality)
    }

    /// The freed ids [`Allocator::allocate_items`] can give, held ids aside: every tenancy in an
    /// open shard, and tenancy 1 and above in a sealed one, whose tenancy-0 ids are its edits'.
    fn item_pool(&self) -> u64 {
        match self.state {
            ShardState::Open => cardinality_of(&self.free),
            ShardState::Sealed => cardinality_of(&self.free) - self.at_zero(),
        }
    }

    /// Whether `n` edits fit: tenancy-0 ids, then the high water up to [`ENTITY_END`], whatever
    /// the shard's state and size.
    fn edits_room(&self, n: u64) -> Result<(), ShardFull> {
        check_room(self.high_water, n - n.min(self.at_zero()))
    }

    /// The new entities of `n` edits of this shard's items: tenancy-0 ids lowest first, then the
    /// high water up to [`ENTITY_END`]. An edit's entity never becomes a number, so it takes no id
    /// an item has held as its number. Each freed id it gives is added to `taken`. Refuses,
    /// leaving the allocator unchanged, where the high water would pass the end.
    pub fn allocate_edits(&mut self, n: u64, taken: &mut ByTenancy) -> Result<Vec<u64>, ShardFull> {
        self.edits_room(n)?;
        let mut ids = take_lowest(&mut self.free, Tenancy::ZERO, n, taken);
        let lo = self.high_water;
        self.high_water += n - ids.len() as u64;
        ids.extend(lo..self.high_water);
        Ok(ids)
    }

    /// Ids for up to `n` new items: the freed ids [`Allocator::item_pool`] counts, lowest tenancy
    /// then lowest id first, then, in an open shard, the high water up to `size`. Fewer than `n`
    /// where the shard has no more to give. Each freed id it gives is added to `taken` at its
    /// tenancy.
    pub fn allocate_items(&mut self, n: u64, size: u64, taken: &mut ByTenancy) -> Vec<u64> {
        let mut ids: Vec<u64> = Vec::with_capacity(n as usize);
        let tenancies: Vec<Tenancy> = self.free.keys().copied().collect();
        for tenancy in tenancies {
            if tenancy == Tenancy::ZERO && self.state == ShardState::Sealed {
                continue;
            }
            let wanted = n - ids.len() as u64;
            if wanted == 0 {
                break;
            }
            ids.extend(take_lowest(&mut self.free, tenancy, wanted, taken));
        }
        if self.state == ShardState::Open {
            let bound = sealing_high_water(size).max(self.high_water);
            let lo = self.high_water;
            self.high_water = (lo + (n - ids.len() as u64)).min(bound);
            ids.extend(lo..self.high_water);
        }
        ids
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

/// One shard as [`place`] sees it: what it can give new items once the window's edits have taken
/// their ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShardRoom {
    shard: ShardId,
    state: ShardState,
    /// [`Allocator::item_pool`].
    pool: u64,
    high_water: u64,
}

/// Each shard's room once `edits` has taken its edits' ids there: tenancy-0 ids first, then the
/// high water. Refuses where a shard's edits pass [`ENTITY_END`].
fn rooms<'a>(
    shards: impl IntoIterator<Item = (ShardId, &'a Allocator)>,
    edits: &BTreeMap<ShardId, u64>,
) -> Result<Vec<ShardRoom>, ShardFull> {
    shards
        .into_iter()
        .map(|(shard, alloc)| {
            let n = edits.get(&shard).copied().unwrap_or(0);
            alloc.edits_room(n)?;
            let reused = n.min(alloc.at_zero());
            Ok(ShardRoom {
                shard,
                state: alloc.state(),
                pool: match alloc.state() {
                    ShardState::Open => alloc.item_pool() - reused,
                    ShardState::Sealed => alloc.item_pool(),
                },
                high_water: alloc.high_water() + (n - reused),
            })
        })
        .collect()
}

/// Where a window's new items go, in the order they take their ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Placement {
    /// Each shard with how many items it takes, in signature order across the list. One shard
    /// appears twice when it gives its pool first and its high water later.
    targets: Vec<(ShardId, u64)>,
    /// Open shards whose high water reached the size, in the order they sealed: one that items
    /// still to place passed by, then, once every item is placed, any other at the size.
    sealed: Vec<ShardId>,
    /// Shards numbered from the next shard id, in order. A shard opened and filled in one window
    /// is in [`Placement::sealed`] too.
    opened: Vec<ShardId>,
}

/// Where `items` new items go among `shards`, given the shard `size` and the next shard number.
///
/// The shard with the largest pool of at least [`reuse_min_free`] takes items into its pool, ties
/// to the lowest shard; otherwise the open shard takes them into its pool and then its high water
/// up to `size`; otherwise shard `next_id` opens. The choice is made again whenever a shard has
/// given all it can. An open shard that reaches `size` ([`reached_size`]) seals, whether the
/// window's new items or its edits took it there, and whether or not items are left.
fn place(
    shards: &[ShardRoom],
    next_id: u32,
    size: u64,
    items: u64,
) -> Result<Placement, ShardsExhausted> {
    let threshold = reuse_min_free(size);
    let bound = sealing_high_water(size);
    let mut shards = shards.to_vec();
    shards.sort_by_key(|room| room.shard);
    let mut next_id = next_id;
    let mut placement = Placement::default();
    let target =
        |placement: &mut Placement, shard: ShardId, n: u64| match placement.targets.last_mut() {
            Some((last, count)) if *last == shard => *count += n,
            _ => placement.targets.push((shard, n)),
        };
    let mut left = items;
    while left > 0 {
        let pooled = shards
            .iter_mut()
            .filter(|room| room.pool >= threshold)
            .reduce(|best, room| if room.pool > best.pool { room } else { best });
        if let Some(room) = pooled {
            let taken = left.min(room.pool);
            room.pool -= taken;
            left -= taken;
            target(&mut placement, room.shard, taken);
            continue;
        }
        if let Some(room) = shards
            .iter_mut()
            .rfind(|room| room.state == ShardState::Open)
        {
            let from_pool = left.min(room.pool);
            let from_high_water = (left - from_pool).min(bound.saturating_sub(room.high_water));
            room.pool -= from_pool;
            room.high_water += from_high_water;
            left -= from_pool + from_high_water;
            if from_pool + from_high_water > 0 {
                target(&mut placement, room.shard, from_pool + from_high_water);
            }
            if left > 0 {
                room.state = ShardState::Sealed;
                placement.sealed.push(room.shard);
            }
            continue;
        }
        let shard = ShardId::new(next_id).map_err(|_| ShardsExhausted { needed: left })?;
        next_id += 1;
        placement.opened.push(shard);
        shards.push(ShardRoom {
            shard,
            state: ShardState::Open,
            pool: 0,
            high_water: 0,
        });
    }
    for room in &mut shards {
        if room.state == ShardState::Open && reached_size(room.high_water, size) {
            room.state = ShardState::Sealed;
            placement.sealed.push(room.shard);
        }
    }
    Ok(placement)
}

/// Whether a window fits among `shards`, ascending, when its edits take `edits` ids in each shard
/// and its `items` new items are placed there and in the shards it opens from `next_id`:
/// [`assign_sorted`]'s rule, asked before the window is assigned. Held ids are not counted.
pub fn check_window(
    shards: &[(ShardId, &Allocator)],
    edits: &BTreeMap<ShardId, u64>,
    items: u64,
    size: u64,
    next_id: u32,
) -> Result<(), AllocError> {
    let rooms = rooms(shards.iter().map(|(shard, alloc)| (*shard, *alloc)), edits)
        .map_err(AllocError::Full)?;
    place(&rooms, next_id, size, items).map_err(AllocError::ShardsExhausted)?;
    Ok(())
}

/// What a window took from one allocator, which [`Allocator::give_back`] returns: the freed ids
/// it issued, each at its tenancy, and the high water and state it found.
pub struct Taken {
    free: ByTenancy,
    high_water: u64,
    state: ShardState,
}

/// What a window's assignment did to the shards.
#[derive(Default)]
pub struct Placed {
    /// What it took from each shard it was given, ascending.
    pub taken: Vec<(ShardId, Taken)>,
    /// The shards that sealed, in the order they sealed.
    pub sealed: Vec<ShardId>,
    /// The shards that opened, each with its allocator, ascending.
    pub opened: Vec<(ShardId, Allocator)>,
}

impl Placed {
    /// The log records that open and seal what this placement opened and sealed: every opening,
    /// ascending, then every seal in order, so a shard opened and filled in one window is opened
    /// before it is sealed.
    pub fn records(&self) -> Vec<WalRecord> {
        let opened = self
            .opened
            .iter()
            .map(|(shard, _)| WalRecord::ShardOpen { shard: *shard });
        let sealed = self
            .sealed
            .iter()
            .map(|shard| WalRecord::ShardSeal { shard: *shard });
        opened.chain(sealed).collect()
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
        // A receipt names an item by `mosaica_id`, and its rows by its record, which a flush wrote.
        WalRecord::BatchesCarried { .. } => {}
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
    /// Filled in by [`assign_sorted`]; a join's arrives filled.
    pub item: Option<ItemAt>,
    /// For an edit's new entity, the shard of the item it edits, which the entity is taken in.
    pub edit: Option<ShardId>,
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

/// Every point shard's allocator a window may take ids in, ascending by shard, with the shard size
/// and the number the next shard opens at.
pub struct Allocators<'a> {
    pub shards: Vec<(ShardId, &'a mut Allocator)>,
    pub size: u64,
    pub next_id: u32,
}

impl<'a> Allocators<'a> {
    /// `shard`'s allocator alone, at `size`, the next shard opening past it.
    pub fn single(shard: ShardId, alloc: &'a mut Allocator, size: u64) -> Self {
        Allocators {
            shards: vec![(shard, alloc)],
            size,
            next_id: shard.raw() + 1,
        }
    }
}

/// Assigns entity ids to `items`, the edits' new entities and the new items each in ascending
/// order of `signature_sort_key`, ties in batch order. Each edit takes its entity in its own
/// shard ([`Allocator::allocate_edits`]); the new items are cut in that order at the counts
/// `place` gives and take their ids there ([`Allocator::allocate_items`]). Items with identical
/// signatures take adjacent ids from a high water, which is what makes their postings compress as
/// runs; freed ids come first and scatter among other signatures' runs.
///
/// Fails, leaving every allocator unchanged, where an edit's shard is full or no shard number is
/// left for the new items.
pub fn assign_sorted(
    items: &mut [PendingItem],
    allocators: &mut Allocators<'_>,
) -> Result<Placed, AllocError> {
    // Compute each item's signature sort key once, up front, rather than inside
    // the comparator — `sort_by`'s comparator can be called O(n log n) times, and
    // `signature_sort_key` allocates, so recomputing it per-comparison would be O(n log n)
    // allocations instead of O(n).
    //
    // **A row that arrives with an entity is a join** (`views.md` §4): the same document in a
    // second view, whose identity was decided when it was first ingested. It takes no id and no
    // rank — allocating one would mint a second entity for one document, and the join rule exists
    // precisely so that cannot happen — so the allocation is sized by the rows that need one.
    let mut keyed: Vec<(Option<ShardId>, Vec<u32>, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.item.is_none())
        .map(|(i, item)| (item.edit, signature_sort_key(&item.terms), i))
        .collect();
    // Edits first, by shard, then the new items. Stable, so items with one signature keep their
    // order in the batch.
    keyed.sort_by(|(ea, ka, _), (eb, kb, _)| (ea.is_none(), ea, ka).cmp(&(eb.is_none(), eb, kb)));
    let edited = keyed.iter().take_while(|(edit, ..)| edit.is_some()).count();
    let edits = mosaica_types::by_shard(
        keyed[..edited]
            .iter()
            .map(|(edit, ..)| (edit.expect("the edits sort first"), ())),
    );
    let created = (keyed.len() - edited) as u64;

    let counts = edits
        .iter()
        .map(|(shard, edited)| (*shard, edited.len() as u64))
        .collect();
    let rooms = rooms(
        allocators
            .shards
            .iter()
            .map(|(shard, alloc)| (*shard, &**alloc)),
        &counts,
    )
    .map_err(AllocError::Full)?;
    for shard in edits.keys() {
        assert!(
            rooms.iter().any(|room| room.shard == *shard),
            "an edit's shard is one the window allocates in"
        );
    }
    let placement = place(&rooms, allocators.next_id, allocators.size, created)
        .map_err(AllocError::ShardsExhausted)?;

    let found: Vec<(ShardId, u64, ShardState)> = allocators
        .shards
        .iter()
        .map(|(shard, alloc)| (*shard, alloc.high_water(), alloc.state()))
        .collect();
    let mut freed: BTreeMap<ShardId, ByTenancy> = BTreeMap::new();
    let mut ids: Vec<ItemAt> = Vec::with_capacity(keyed.len());
    for (shard, alloc) in allocators.shards.iter_mut() {
        if let Some(edited) = edits.get(shard) {
            let given = alloc
                .allocate_edits(edited.len() as u64, freed.entry(*shard).or_default())
                .expect("the edits' room was checked above");
            ids.extend(
                given
                    .into_iter()
                    .map(|id| ItemAt::new(*shard, EntityId::new(id))),
            );
        }
    }
    let mut opened: Vec<(ShardId, Allocator)> = placement
        .opened
        .iter()
        .map(|&shard| (shard, Allocator::new(0)))
        .collect();
    fn alloc_of<'s>(
        held: &'s mut [(ShardId, &mut Allocator)],
        opened: &'s mut [(ShardId, Allocator)],
        shard: ShardId,
    ) -> &'s mut Allocator {
        match held.iter_mut().find(|(id, _)| *id == shard) {
            Some((_, alloc)) => alloc,
            None => opened
                .iter_mut()
                .find(|(id, _)| *id == shard)
                .map(|(_, alloc)| alloc)
                .expect("a placed shard is one the window holds or opened"),
        }
    }
    for &(shard, n) in &placement.targets {
        let given = alloc_of(&mut allocators.shards, &mut opened, shard).allocate_items(
            n,
            allocators.size,
            freed.entry(shard).or_default(),
        );
        debug_assert_eq!(
            given.len() as u64,
            n,
            "the placement counted what the shard gives"
        );
        ids.extend(
            given
                .into_iter()
                .map(|id| ItemAt::new(shard, EntityId::new(id))),
        );
    }
    for &shard in &placement.sealed {
        alloc_of(&mut allocators.shards, &mut opened, shard).seal();
    }

    for ((_, _, idx), id) in keyed.into_iter().zip(ids) {
        items[idx].item = Some(id);
    }
    let taken = found
        .into_iter()
        .map(|(shard, high_water, state)| {
            let free = freed.remove(&shard).unwrap_or_default();
            (
                shard,
                Taken {
                    free,
                    high_water,
                    state,
                },
            )
        })
        .collect();
    Ok(Placed {
        taken,
        sealed: placement.sealed,
        opened,
    })
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

    /// An allocator at `high_water` with `free` freed and issuable.
    fn freed(high_water: u64, free: &[(u16, &[u32])]) -> Allocator {
        let mut alloc = Allocator::new(high_water);
        for (tenancy, ids) in free {
            alloc.release_after(0, at(*tenancy, ids));
        }
        alloc.promote(0);
        alloc
    }

    fn shard(raw: u32) -> ShardId {
        ShardId::new(raw).unwrap()
    }

    const NO_SIZE: u64 = u64::MAX;

    /// [`Allocator::allocate_items`], keeping no record of what it took.
    fn take_items(alloc: &mut Allocator, n: u64, size: u64) -> Vec<u64> {
        alloc.allocate_items(n, size, &mut ByTenancy::new())
    }

    /// [`Allocator::allocate_edits`], keeping no record of what it took.
    fn take_edits(alloc: &mut Allocator, n: u64) -> Result<Vec<u64>, ShardFull> {
        alloc.allocate_edits(n, &mut ByTenancy::new())
    }

    #[test]
    fn ids_rise_from_the_high_water_without_freed_ids() {
        let mut alloc = Allocator::new(10);
        assert_eq!(take_items(&mut alloc, 3, NO_SIZE), vec![10, 11, 12]);
        assert_eq!(take_edits(&mut alloc, 2).unwrap(), vec![13, 14]);
        assert_eq!(take_items(&mut alloc, 3, NO_SIZE), vec![15, 16, 17]);
        assert_eq!(alloc.high_water(), 18);
    }

    #[test]
    fn freed_ids_are_issued_lowest_first_and_once() {
        let mut alloc = Allocator::new(100);
        alloc.release_after(7, at(0, &[40, 12, 90]));
        assert_eq!(
            take_items(&mut alloc, 2, NO_SIZE),
            vec![100, 101],
            "held ids are not issued"
        );
        assert_eq!(
            alloc.promote(6),
            0,
            "the log still keeps a record older than 7"
        );
        assert_eq!(alloc.promote(7), 3);
        assert_eq!(take_items(&mut alloc, 2, NO_SIZE), vec![12, 40]);
        assert_eq!(take_items(&mut alloc, 3, NO_SIZE), vec![90, 102, 103]);
        assert_eq!(alloc.high_water(), 104);
        assert!(alloc.free().is_empty());
    }

    #[test]
    fn edits_take_tenancy_zero_and_new_items_the_lowest_tenancy_first() {
        let mut alloc = freed(100, &[(0, &[40, 12]), (3, &[2]), (1, &[30, 5])]);
        assert_eq!(
            take_edits(&mut alloc, 3).unwrap(),
            vec![12, 40, 100],
            "an edit takes tenancy 0, then the high water"
        );
        assert_eq!(
            take_items(&mut alloc, 4, NO_SIZE),
            vec![5, 30, 2, 101],
            "a new item takes the lowest tenancy, lowest id first, then the high water"
        );
        assert!(alloc.free().is_empty());

        let mut alloc = freed(102, &[(0, &[7]), (2, &[9])]);
        assert_eq!(
            take_items(&mut alloc, 1, NO_SIZE),
            vec![7],
            "tenancy 0 comes first for a new item of an open shard"
        );
        assert_eq!(
            take_edits(&mut alloc, 1).unwrap(),
            vec![102],
            "an edit takes no id above tenancy 0"
        );
        assert_eq!(listed(alloc.free()), vec![(2, vec![9])]);
    }

    #[test]
    fn a_sealed_shard_gives_new_items_only_ids_freed_above_tenancy_zero() {
        let mut alloc = freed(50, &[(0, &[3, 4]), (1, &[8]), (2, &[6])]);
        alloc.seal();
        assert_eq!(alloc.state(), ShardState::Sealed);
        assert_eq!(alloc.item_pool(), 2);
        assert_eq!(
            take_items(&mut alloc, 5, NO_SIZE),
            vec![8, 6],
            "never a tenancy-0 id and never the high water"
        );
        assert_eq!(alloc.high_water(), 50);
        assert_eq!(
            take_edits(&mut alloc, 3).unwrap(),
            vec![3, 4, 50],
            "its edits take its tenancy-0 ids, then its high water"
        );
    }

    #[test]
    fn a_refused_edit_takes_no_freed_id() {
        let mut alloc = freed(ENTITY_END - 1, &[(0, &[5]), (4, &[3])]);
        assert_eq!(
            take_edits(&mut alloc, 3),
            Err(ShardFull {
                high_water: ENTITY_END - 1,
                needed: 2,
            })
        );
        assert_eq!(listed(alloc.free()), vec![(0, vec![5]), (4, vec![3])]);
        assert_eq!(alloc.high_water(), ENTITY_END - 1);
        assert_eq!(take_edits(&mut alloc, 2).unwrap(), vec![5, ENTITY_END - 1]);
    }

    #[test]
    fn the_size_bounds_new_items_and_not_edits() {
        let mut alloc = freed(8, &[(0, &[2]), (1, &[3])]);
        alloc.release_after(5, at(0, &[4]));
        let exhausted = |needed| Err(AllocError::ShardsExhausted(ShardsExhausted { needed }));
        // With no shard number left, the new items must fit this shard.
        let fits = |alloc: &Allocator, edits, items, next_id| {
            check_window(
                &[(shard(0), alloc)],
                &BTreeMap::from([(shard(0), edits)]),
                items,
                10,
                next_id,
            )
        };
        let last = ShardId::LIMIT;
        assert_eq!(
            fits(&alloc, 1, 3, last),
            Ok(()),
            "the edit takes 2, two items take 3 and 8, and one 9"
        );
        assert_eq!(
            fits(&alloc, 1, 4, last),
            exhausted(1),
            "the held 4 is not counted"
        );
        assert_eq!(
            fits(&alloc, 1, 4, 1),
            Ok(()),
            "the fourth item opens shard 1"
        );
        assert_eq!(
            fits(&alloc, 4, 0, last),
            Ok(()),
            "edits take 2, then 8 and 9, and run past the size to 10"
        );
        assert_eq!(fits(&alloc, 4, 1, last), Ok(()), "the item takes 3");
        assert_eq!(
            fits(&alloc, 4, 2, last),
            exhausted(1),
            "the edits run past the size, and the second item needs a new shard"
        );
        assert_eq!(fits(&alloc, 4, 2, 1), Ok(()));
        assert_eq!(take_edits(&mut alloc, 1).unwrap(), vec![2]);
        assert_eq!(
            take_items(&mut alloc, 4, 10),
            vec![3, 8, 9],
            "items stop at the size"
        );
        assert_eq!(take_items(&mut alloc, 1, 10), Vec::<u64>::new());
        assert_eq!(
            take_edits(&mut alloc, 2).unwrap(),
            vec![10, 11],
            "an edit runs past the size"
        );
        assert_eq!(take_items(&mut alloc, 1, 13), vec![12]);
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
        assert_eq!(
            take_items(&mut a, 3, NO_SIZE),
            vec![ENTITY_END - 2, ENTITY_END - 1]
        );
        assert_eq!(
            take_edits(&mut a, 1),
            Err(ShardFull {
                high_water: ENTITY_END,
                needed: 1,
            })
        );
        assert_eq!(a.high_water(), ENTITY_END, "a refused window takes nothing");

        assert!(Allocator::try_new(ENTITY_END).is_some());
        assert!(Allocator::try_new(ENTITY_END + 1).is_none());
    }

    fn room(raw: u32, state: ShardState, pool: u64, high_water: u64) -> ShardRoom {
        ShardRoom {
            shard: shard(raw),
            state,
            pool,
            high_water,
        }
    }

    fn targets(placement: &Placement) -> Vec<(u32, u64)> {
        placement
            .targets
            .iter()
            .map(|(shard, n)| (shard.raw(), *n))
            .collect()
    }

    use ShardState::{Open, Sealed};

    #[test]
    fn the_largest_pool_at_the_threshold_comes_first_and_ties_go_to_the_lowest_shard() {
        // A size of 4,096 sets the threshold at 4.
        let shards = [
            room(0, Sealed, 5, 4096),
            room(1, Sealed, 9, 4096),
            room(2, Sealed, 9, 4096),
            room(3, Sealed, 3, 4096),
            room(4, Open, 2, 100),
        ];
        let placement = place(&shards, 5, 4096, 30).unwrap();
        assert_eq!(
            targets(&placement),
            vec![(1, 9), (2, 9), (0, 5), (4, 7)],
            "pools of 4 or more by size, then the open shard's pool and high water; shard 3's \
             pool of 3 is below the threshold"
        );
        assert!(placement.sealed.is_empty() && placement.opened.is_empty());
    }

    #[test]
    fn a_pool_below_the_threshold_waits_and_the_open_shard_takes_the_items() {
        let shards = [room(0, Sealed, 3, 4096), room(1, Open, 0, 10)];
        assert_eq!(targets(&place(&shards, 2, 4096, 2).unwrap()), vec![(1, 2)]);
        assert_eq!(
            targets(&place(&shards, 2, 4096, 0).unwrap()),
            Vec::<(u32, u64)>::new()
        );
    }

    #[test]
    fn one_open_shard_gives_its_pool_and_then_its_high_water_in_one_run() {
        for pool in [0, 1, 3, 4, 50] {
            let placement = place(&[room(0, Open, pool, 10)], 1, 4096, 20).unwrap();
            assert_eq!(targets(&placement), vec![(0, 20)], "a pool of {pool}");
            assert!(placement.sealed.is_empty() && placement.opened.is_empty());
        }
    }

    #[test]
    fn a_window_straddles_the_open_shard_and_the_shards_it_opens() {
        let placement = place(&[room(0, Sealed, 0, 10), room(1, Open, 1, 8)], 2, 10, 25).unwrap();
        assert_eq!(
            targets(&placement),
            vec![(1, 3), (2, 10), (3, 10), (4, 2)],
            "shard 1's pool and two ids to its size, then two full shards and a third"
        );
        assert_eq!(
            placement.sealed.iter().map(|s| s.raw()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            placement.opened.iter().map(|s| s.raw()).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        let placement = place(&[room(0, Open, 0, 12)], 1, 10, 1).unwrap();
        assert_eq!(
            (targets(&placement), placement.sealed, placement.opened),
            (vec![(1, 1)], vec![shard(0)], vec![shard(1)]),
            "an open shard its edits took past the size seals at the first new item"
        );
    }

    #[test]
    fn an_open_shard_that_reaches_the_size_with_nothing_left_seals() {
        let sealed = |shards: &[ShardRoom], items| {
            let placement = place(shards, 2, 10, items).unwrap();
            assert!(placement.opened.is_empty(), "{items} items open no shard");
            placement.sealed
        };
        assert_eq!(
            sealed(&[room(0, Sealed, 0, 10), room(1, Open, 0, 8)], 2),
            vec![shard(1)],
            "filled exactly by the new items"
        );
        assert_eq!(sealed(&[room(1, Open, 0, 8)], 1), Vec::<ShardId>::new());
        for high_water in [10, 12] {
            assert_eq!(
                sealed(&[room(1, Open, 0, high_water)], 0),
                vec![shard(1)],
                "a window of edits alone that took it to {high_water}"
            );
        }
        assert_eq!(
            sealed(&[room(0, Sealed, 9, 4096), room(1, Open, 0, 10)], 4),
            vec![shard(1)],
            "every item went to a pool"
        );
        let placement = place(&[], 1, 10, 10).unwrap();
        assert_eq!(
            (placement.opened, placement.sealed),
            (vec![shard(1)], vec![shard(1)]),
            "a shard opened and filled exactly in one window"
        );
    }

    #[test]
    fn no_shard_past_the_last_number_opens() {
        let last = ShardId::LIMIT - 1;
        let shards = [room(last, Open, 0, 8)];
        assert_eq!(
            place(&shards, ShardId::LIMIT, 10, 3),
            Err(ShardsExhausted { needed: 1 })
        );
        assert_eq!(
            targets(&place(&shards, ShardId::LIMIT, 10, 2).unwrap()),
            vec![(last, 2)]
        );
        assert_eq!(
            place(&[], ShardId::LIMIT - 1, 10, 25),
            Err(ShardsExhausted { needed: 15 }),
            "the last shard takes its size and no more"
        );
    }

    fn pending(terms: &[u32], edit: Option<u32>) -> PendingItem {
        PendingItem {
            terms: terms.iter().map(|&t| TermId::new(t)).collect(),
            item: None,
            edit: edit.map(shard),
        }
    }

    fn assigned(items: &[PendingItem]) -> Vec<(u32, u64)> {
        items
            .iter()
            .map(|item| {
                let item = item.item.unwrap();
                (item.shard.raw(), item.entity.raw())
            })
            .collect()
    }

    #[test]
    fn edits_take_ids_in_their_own_shard_and_items_are_cut_in_signature_order() {
        let mut zero = freed(10, &[(0, &[1]), (1, &[2, 3, 4, 5])]);
        zero.seal();
        let mut one = Allocator::new(7);
        let mut items = vec![
            pending(&[9], None),
            pending(&[3], Some(0)),
            pending(&[1], None),
            pending(&[5], None),
            pending(&[2], Some(1)),
            pending(&[4], None),
            pending(&[6], None),
            pending(&[8], None),
            pending(&[1], Some(0)),
        ];
        let placed = assign_sorted(
            &mut items,
            &mut Allocators {
                shards: vec![(shard(0), &mut zero), (shard(1), &mut one)],
                size: 4096,
                next_id: 2,
            },
        )
        .unwrap();
        assert_eq!(
            assigned(&items),
            vec![
                (1, 9),
                (0, 10),
                (0, 2),
                (0, 4),
                (1, 7),
                (0, 3),
                (0, 5),
                (1, 8),
                (0, 1),
            ],
            "shard 0's edits take its tenancy-0 id and then its high water, shard 1's its high \
             water; the items in signature order fill shard 0's pool of 4 and go on into shard 1"
        );
        assert!(placed.sealed.is_empty() && placed.opened.is_empty());
        assert_eq!((zero.high_water(), one.high_water()), (11, 10));
    }

    #[test]
    fn a_window_that_cannot_be_placed_changes_no_allocator() {
        let mut zero = freed(ENTITY_END, &[(0, &[1])]);
        let before = |alloc: &Allocator| (alloc.high_water(), listed(alloc.free()));
        let held = before(&zero);
        let mut items = vec![pending(&[1], Some(0)), pending(&[2], Some(0))];
        let error =
            assign_sorted(&mut items, &mut Allocators::single(shard(0), &mut zero, 10)).err();
        assert!(matches!(error, Some(AllocError::Full(_))));
        assert_eq!(before(&zero), held);

        let mut last = Allocator::new(8);
        let mut items = vec![
            pending(&[1], None),
            pending(&[2], None),
            pending(&[3], None),
        ];
        let error = assign_sorted(
            &mut items,
            &mut Allocators::single(shard(ShardId::LIMIT - 1), &mut last, 10),
        )
        .err();
        assert_eq!(
            error,
            Some(AllocError::ShardsExhausted(ShardsExhausted { needed: 1 }))
        );
        assert_eq!((last.high_water(), last.state()), (8, Open));
        assert!(items.iter().all(|item| item.item.is_none()));
    }

    #[test]
    fn a_window_that_does_not_commit_gives_back_what_it_took() {
        let mut zero = freed(10, &[(0, &[1]), (1, &[2, 3])]);
        zero.seal();
        let mut one = freed(8, &[(0, &[4]), (2, &[5])]);
        let state = |alloc: &Allocator| (alloc.high_water(), alloc.state(), listed(alloc.free()));
        let before = (state(&zero), state(&one));
        let assign = |zero: &mut Allocator, one: &mut Allocator| {
            let mut items: Vec<PendingItem> = (0..7).map(|_| pending(&[1], None)).collect();
            items.extend([pending(&[1], Some(0)), pending(&[1], Some(1))]);
            let placed = assign_sorted(
                &mut items,
                &mut Allocators {
                    shards: vec![(shard(0), zero), (shard(1), one)],
                    size: 10,
                    next_id: 2,
                },
            )
            .unwrap();
            (assigned(&items), placed)
        };

        let (first, placed) = assign(&mut zero, &mut one);
        assert_eq!(
            first,
            vec![
                (0, 2),
                (0, 3),
                (1, 5),
                (1, 8),
                (1, 9),
                (2, 0),
                (2, 1),
                (0, 1),
                (1, 4),
            ],
            "the window takes freed ids at every tenancy, shard 1's high water to the size, and a \
             new shard"
        );
        assert_eq!(placed.sealed, vec![shard(1)]);
        for ((_, taken), alloc) in placed.taken.into_iter().zip([&mut zero, &mut one]) {
            alloc.give_back(taken);
        }
        assert_eq!(
            (state(&zero), state(&one)),
            before,
            "each shard has its freed ids back at their tenancies, its high water and its state"
        );
        let (again, _) = assign(&mut zero, &mut one);
        assert_eq!(again, first, "the next window is given the same ids");
    }

    #[test]
    fn a_full_open_shard_seals_and_the_rest_take_a_new_shard() {
        let mut zero = Allocator::new(9);
        let mut items = vec![
            pending(&[2], None),
            pending(&[1], None),
            pending(&[3], None),
        ];
        let placed =
            assign_sorted(&mut items, &mut Allocators::single(shard(0), &mut zero, 10)).unwrap();
        assert_eq!(assigned(&items), vec![(1, 0), (0, 9), (1, 1)]);
        assert_eq!(placed.sealed, vec![shard(0)]);
        assert_eq!(zero.state(), Sealed);
        let [(opened, alloc)] = placed.opened.as_slice() else {
            panic!("one shard opened");
        };
        assert_eq!(
            (*opened, alloc.state(), alloc.high_water()),
            (shard(1), Open, 2)
        );
        assert_eq!(
            placed.records(),
            vec![
                WalRecord::ShardOpen { shard: shard(1) },
                WalRecord::ShardSeal { shard: shard(0) },
            ]
        );
    }

    #[test]
    fn a_shard_opened_and_filled_in_one_window_is_opened_before_it_is_sealed() {
        let mut zero = Allocator::new(10);
        let mut items: Vec<PendingItem> = (0..15).map(|_| pending(&[1], None)).collect();
        let placed =
            assign_sorted(&mut items, &mut Allocators::single(shard(0), &mut zero, 10)).unwrap();
        assert_eq!(
            placed.records(),
            vec![
                WalRecord::ShardOpen { shard: shard(1) },
                WalRecord::ShardOpen { shard: shard(2) },
                WalRecord::ShardSeal { shard: shard(0) },
                WalRecord::ShardSeal { shard: shard(1) },
            ]
        );
    }

    #[test]
    fn a_window_fits_the_pools_at_the_threshold_the_open_shards_room_and_the_shards_it_opens() {
        let size = 2048;
        let exhausted = |needed| Err(AllocError::ShardsExhausted(ShardsExhausted { needed }));
        let mut pooled = freed(3000, &[(0, &[1, 2]), (1, &[3, 4])]);
        pooled.seal();
        let mut below = freed(2100, &[(1, &[5])]);
        below.seal();
        let open = freed(2040, &[(0, &[6])]);
        let shards = [(shard(0), &pooled), (shard(1), &below), (shard(2), &open)];
        let none = BTreeMap::new();
        let last = ShardId::LIMIT;
        // Shard 0's pool of 2 is at the threshold of 2; shard 1's of 1 is below it.
        assert_eq!(check_window(&shards, &none, 11, size, last), Ok(()));
        assert_eq!(check_window(&shards, &none, 12, size, last), exhausted(1));
        assert_eq!(
            check_window(&shards, &none, 12, size, 3),
            Ok(()),
            "the twelfth item opens shard 3"
        );
        let edited = BTreeMap::from([(shard(2), 1)]);
        assert_eq!(
            check_window(&shards, &edited, 11, size, last),
            exhausted(1),
            "the open shard's edit takes its tenancy-0 id before a new item can"
        );
        let edited = BTreeMap::from([(shard(0), 2), (shard(1), 1)]);
        assert_eq!(
            check_window(&shards, &edited, 11, size, last),
            Ok(()),
            "a sealed shard's edits are not bounded by the size"
        );

        let mut ended = freed(ENTITY_END, &[(1, &[7])]);
        ended.seal();
        let shards = [(shard(0), &ended), (shard(1), &open)];
        let edited = BTreeMap::from([(shard(0), 1)]);
        assert_eq!(
            check_window(&shards, &edited, 0, size, 2),
            Err(AllocError::Full(ShardFull {
                high_water: ENTITY_END,
                needed: 1,
            }))
        );
    }

    #[test]
    fn a_window_needing_a_shard_past_the_last_number_does_not_fit() {
        let full = Allocator::new(10);
        let shards = [(shard(0), &full)];
        let none = BTreeMap::new();
        let next_id = ShardId::LIMIT - 1;
        assert_eq!(
            check_window(&shards, &none, 10, 10, next_id),
            Ok(()),
            "the last shard number takes ten"
        );
        assert_eq!(
            check_window(&shards, &none, 11, 10, next_id),
            Err(AllocError::ShardsExhausted(ShardsExhausted { needed: 1 }))
        );
    }

    #[test]
    fn a_window_fits_where_its_assignment_succeeds() {
        let size = 10;
        let allocs: [fn() -> Allocator; 2] = [
            || freed(8, &[(0, &[2]), (1, &[3])]),
            || freed(ENTITY_END - 1, &[(0, &[2])]),
        ];
        for alloc in allocs {
            for next_id in [1, ShardId::LIMIT - 1, ShardId::LIMIT] {
                for (edits, items) in [
                    (0, 4),
                    (0, 5),
                    (1, 3),
                    (2, 3),
                    (3, 0),
                    (4, 0),
                    (4, 1),
                    (4, 2),
                    (0, 15),
                    (0, 25),
                ] {
                    let fits = check_window(
                        &[(shard(0), &alloc())],
                        &BTreeMap::from([(shard(0), edits)]),
                        items,
                        size,
                        next_id,
                    );
                    let mut window: Vec<PendingItem> = (0..edits)
                        .map(|_| pending(&[1], Some(0)))
                        .chain((0..items).map(|_| pending(&[1], None)))
                        .collect();
                    let mut assigning = alloc();
                    let assigned = assign_sorted(
                        &mut window,
                        &mut Allocators {
                            shards: vec![(shard(0), &mut assigning)],
                            size,
                            next_id,
                        },
                    );
                    assert_eq!(
                        fits,
                        assigned.map(|_| ()),
                        "{edits} edits, {items} items, next shard {next_id}"
                    );
                }
            }
        }
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
            principal: String::new(),
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
