//! The ingest buffer: `WalRow`s not yet in any segment.
//!
//! **It holds exactly the rows without geometry, and nothing else may reach it.** An ingested item
//! is durable (WAL-fsynced) and participates in authorisation state (I1's composition rule 4) the
//! moment it is accepted, but it has no `Permutation::row_of` entry anywhere until the flush tick
//! gives it one — so an ack is a durability receipt, never a visibility promise, and the gap is
//! bounded by `flush_max_age_secs` (write-path §4.1).
//!
//! Two rules keep membership exact, and both are enforced away from here because both are
//! statements about the *other* state a row can be in. A row that acquired geometry leaves at its
//! flush's publication (`Executor::publish_flush`, by consumed id) or at replay
//! (`WritePath::reconstruct`, by the `row_of` predicate). A row that was **deleted** leaves as the
//! deletion applies (`crate::overlay::drop_deleted`): it will never acquire geometry, so nothing
//! else would ever remove it, and [`IngestBuffer::oldest_wal_pos`] is the WAL's reclaim bound —
//! one such row pins its member and every member after it, for ever.
//!
//! Term descriptors are resolved through [`DescriptorResolver`]: the bundle dictionary first,
//! then a deterministic in-memory extension interned in replay order for descriptors the
//! dictionary has never seen. A term that lives only in the extension is unsatisfiable by any
//! session's `satisfied` set (which is computed from the auth plugin's grant against terms the
//! *auth* side knows about — an in-memory-only *data*-side term id is never handed to a viewer's
//! credential evaluation), so this is fail-closed, not fail-open: a novel descriptor can buffer
//! an item, but cannot make it visible, until the next build assigns it a durable term id.
//!
//! **Extension ids are allocated from the top of the `u32` range downward**,
//! never from `dict.len()` upward: a downward-from-`u32::MAX` extension id can never collide with
//! a *future* dictionary ordinal the way an upward one could. An upward scheme's "unsatisfiable"
//! property was prose-only and silently broken by growth: extension id `N == dict.len()` at
//! replay time is exactly the ordinal the *next* `tessera build` would assign to some unrelated,
//! real descriptor; if the overlay/buffer ever survived a bundle swap without a fresh replay
//! against the new dictionary — nothing does that today, and nothing in this module guarantees
//! nothing ever will — a stale extension-tagged entity would silently start
//! evaluating against whatever real term inherited that ordinal, and a viewer legitimately holding
//! that term would then be shown it. Reserving the top of the id space (the
//! `max_distinct_terms` bound is 200,000,000, vanishingly far from `u32::MAX`'s ~4.29 billion)
//! makes that collision structurally impossible rather than merely unlikely-so-far.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_types::{EntityId, TermId};

use crate::wal::{WalRow, WalScalar};

/// Resolves term descriptors to `TermId`s: the bundle dictionary first, then a deterministic
/// (replay-order) in-memory extension for descriptors the dictionary has never interned.
///
/// Determinism obligation: called in WAL replay order (or live-accept order, which is the same
/// append order), so the same WAL byte-for-byte always yields the same extension assignment —
/// this is what makes replay reproducible across restarts, not merely "some valid resolution".
pub struct DescriptorResolver<'a> {
    dict: &'a Dict,
    extension: FxHashMap<Vec<u8>, TermId>,
    next_extension_id: u32,
}

// Manual (not derived): `Dict` itself carries no `Debug` impl, and adding one purely to satisfy
// this struct's derive would be scope creep on another crate. `dict` is omitted from the output.
impl std::fmt::Debug for DescriptorResolver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DescriptorResolver")
            .field("extension", &self.extension)
            .field("next_extension_id", &self.next_extension_id)
            .finish()
    }
}

/// Extension ids count down from here — see this module's doc for why the top of the range,
/// never `dict.len()` upward. The plugin ABI's `declared_bounds().max_distinct_terms`
/// (200,000,000) is the largest a real dictionary is sized for; this leaves a margin of roughly
/// 4.09 billion ids between the highest extension id ever handed out in a single session and the
/// highest ordinal a dictionary could plausibly reach, so exhausting it would require an
/// implausible number of distinct novel descriptors in one session, not merely dictionary growth
/// over time.
const EXTENSION_ID_START: u32 = u32::MAX;

/// Compile-time guarantee that the extension range starts strictly above the plugin ABI's
/// declared `max_distinct_terms` bound (200,000,000) — a real dictionary is never sized to reach
/// anywhere near this range, so an extension id can never be mistaken for one.
const _: () = assert!(EXTENSION_ID_START > 200_000_000);

impl<'a> DescriptorResolver<'a> {
    pub fn new(dict: &'a Dict) -> Self {
        DescriptorResolver {
            dict,
            extension: FxHashMap::default(),
            next_extension_id: EXTENSION_ID_START,
        }
    }

    /// Resolve one descriptor: a dictionary hit returns the durable, bundle-relative `TermId`
    /// unchanged; a miss is interned into the in-memory extension (assigning the next id counting
    /// down from [`EXTENSION_ID_START`] — never colliding with a dictionary ordinal, however much
    /// the dictionary grows) and that assignment is reused for any repeat of the same descriptor
    /// within this resolver's lifetime.
    pub fn resolve(&mut self, descriptor: &[u8]) -> TermId {
        if let Some(id) = self.dict.lookup(descriptor) {
            return id;
        }
        if let Some(&id) = self.extension.get(descriptor) {
            return id;
        }
        let id = TermId::new(self.next_extension_id);
        debug_assert!(
            self.next_extension_id > self.dict.len(),
            "descriptor extension id space exhausted down to the dictionary's own range — an \
             implausible number of distinct novel descriptors in one session"
        );
        self.next_extension_id -= 1;
        self.extension.insert(descriptor.to_vec(), id);
        id
    }

    /// Resume a resolver from a previously-persisted extension state.
    ///
    /// The server keeps accepting live `/control/ingest`/`/control/changes` requests after
    /// `replay` has returned — a fresh `DescriptorResolver::new` for each live request would
    /// restart extension-id assignment from [`EXTENSION_ID_START`] every time, colliding with ids
    /// already handed out to *other* novel descriptors earlier in the same process's lifetime
    /// (breaking the determinism obligation this module's doc describes: the same WAL, replayed
    /// again after a restart, must reproduce the same assignment). Extracting a resolver's state
    /// via [`DescriptorResolver::into_state`] after `replay` and resuming it here — once, at
    /// `Engine::open`, then persisting the state back after every live resolution — keeps one
    /// continuous assignment sequence across the whole process lifetime, matching what a full
    /// WAL replay (bundle + WAL + these new records) would compute.
    pub fn resume(
        dict: &'a Dict,
        extension: FxHashMap<Vec<u8>, TermId>,
        next_extension_id: u32,
    ) -> Self {
        DescriptorResolver {
            dict,
            extension,
            next_extension_id,
        }
    }

    /// Extract this resolver's mutable extension state, detaching it from `dict`'s borrow so it
    /// can be stored (e.g. behind a `Mutex`, in `tessera-engine`'s `Engine`) and later resumed.
    pub fn into_state(self) -> (FxHashMap<Vec<u8>, TermId>, u32) {
        (self.extension, self.next_extension_id)
    }
}

/// One buffered item's authorisation-relevant state: its resolved terms and the geometry/scalars
/// carried by its WAL row. The geometry is kept for a flush that would give the item a row;
/// composition reads only `terms`.
///
/// `slice` is carried because a flush reads the *buffer*, not the WAL, and has to know which row
/// space each item's row belongs in — see [`crate::wal::WalRow`]'s field for why that cannot be
/// re-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferedItem {
    pub terms: Vec<TermId>,
    pub slice: String,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<WalScalar>,
    /// The caller-supplied external id, or `None` for an item ingested without one (contracts
    /// §3.4 r6) — carried because the **flush** is what writes it into the bundle's external-id
    /// extent and locator, and the flush reads the buffer rather than the WAL.
    ///
    /// Without it a flushed item is addressable by its `tessera_id` alone the moment its WAL record
    /// is reclaimed: the drill-down has nothing to answer with, and the ingest duplicate check has
    /// nothing to collide against — which admits a byte-identical second copy that no external id
    /// names, so no deny can ever reach it.
    pub external_id: Option<Vec<u8>>,
    /// The sequence-global WAL position of the record this row arrived in — what a rotation
    /// reclaims below (write-path §4.5).
    ///
    /// **`None` means "not known", and it is not the same as zero.** Both are fail-safe, because a
    /// rotation may only reclaim below the oldest position it is *sure* of — but a caller that
    /// treated `None` as a position would reclaim everything, so the distinction is carried in the
    /// type rather than in a sentinel. It is `None` on the way out of [`IngestBuffer::insert_row`]
    /// because neither replay nor the live apply knows the position at that point; both stamp it
    /// immediately afterwards with [`IngestBuffer::set_wal_pos`].
    pub wal_pos: Option<u64>,
}

/// Replayed `WalRow`s not yet folded into a bundle, keyed by (internal) `EntityId` — the id the
/// row was allocated under (SA §6.2: WAL rows carry their already-allocated id; replay reuses it,
/// never re-allocates).
/// `Clone` because the live `/control/ingest` acceptance path builds the
/// next generation's buffer by cloning the current one and inserting the newly-accepted rows,
/// rather than mutating shared state in place — the immutable-snapshot-behind-`ArcSwap` design
/// (see `tessera_engine::Generation`'s doc) requires every generation's buffer to be a distinct,
/// never-mutated-after-publication value.
#[derive(Debug, Default, Clone)]
pub struct IngestBuffer {
    /// **`Arc<BufferedItem>`, because this map is cloned far more often than it is read.**
    ///
    /// `Executor::apply_window` deep-copies the whole buffer once per commit-window close, to build
    /// the next generation's immutable snapshot — so with `B` rows buffered between flushes and a
    /// close every `W`, a flush interval pays `B²/2W` item copies. Measured at 5.99 us/row (42% of
    /// ingest) at a 250M base and 2.60 us/row at 1M; the difference is term density, since a
    /// `BufferedItem` carries a `Vec<TermId>`, an `Option<Vec<u8>>`, a `String` and a scalars `Vec`
    /// — four heap allocations copied per row per close.
    ///
    /// Behind an `Arc` the clone copies a pointer and bumps a refcount, and the items themselves
    /// are shared across every generation that still names them. The snapshot property is
    /// unchanged: an `Arc<BufferedItem>` is never mutated in place once a generation holds it —
    /// [`IngestBuffer::set_wal_pos`] is the one writer and it goes through `Arc::make_mut`, which
    /// copies only when the item is genuinely shared.
    ///
    /// This does **not** remove the O(buffered) term — the hash table itself is still copied per
    /// close. It removes the per-item deep copy, which is what the measurement says dominates it.
    items: FxHashMap<EntityId, Arc<BufferedItem>>,
}

impl IngestBuffer {
    pub fn new() -> Self {
        IngestBuffer {
            items: FxHashMap::default(),
        }
    }

    /// Insert one WAL row's item, resolving its term descriptors via `resolver`.
    pub fn insert_row(&mut self, row: &WalRow, resolver: &mut DescriptorResolver<'_>) {
        let terms = row
            .descriptors
            .iter()
            .map(|d| resolver.resolve(d))
            .collect();
        self.insert_row_with_terms(row, terms);
    }

    /// Insert one WAL row's item with already-resolved `terms`, bypassing descriptor resolution.
    /// Exposed for tests (and any future caller that already holds resolved `TermId`s, e.g. a
    /// live-accept path that resolved descriptors once up front); production replay should
    /// normally go through [`Self::insert_row`].
    pub fn insert_row_with_terms(&mut self, row: &WalRow, terms: Vec<TermId>) {
        self.items.insert(
            row.entity_id,
            Arc::new(BufferedItem {
                terms,
                external_id: row.external_id.clone(),
                slice: row.slice.clone(),
                x: row.x,
                y: row.y,
                scalars: row.scalars.clone(),
                wal_pos: None,
            }),
        );
    }

    /// Record which WAL position `entity`'s row arrived at. No-op if the entity is not buffered,
    /// which is the ordinary case for a stamp arriving after a flush has consumed the row.
    pub fn set_wal_pos(&mut self, entity: EntityId, wal_pos: u64) {
        if let Some(item) = self.items.get_mut(&entity) {
            // Copies only if a published generation still shares this item; at the call site it is
            // stamped immediately after insert, where the refcount is one and this is in place.
            Arc::make_mut(item).wal_pos = Some(wal_pos);
        }
    }

    /// The lowest WAL position any buffered row arrived at, or `None` if any of them does not know
    /// its own — **the position a rotation may reclaim below** (write-path §4.5).
    ///
    /// Every other record class below that point is already redundant: `Change` records are
    /// restated by the rotation's own overlay snapshot, `Lease` records by the side-manifest's
    /// entity-id high-water, and `Flush` records by nothing needing them. Only an ingest row that
    /// has not yet acquired geometry pins the log.
    ///
    /// `Some(None)` is impossible by construction; the outer `Option` is emptiness and the inner
    /// answer is "one of them is unknown, so reclaim nothing".
    pub fn oldest_wal_pos(&self) -> Option<Option<u64>> {
        if self.items.is_empty() {
            return None;
        }
        Some(
            self.items
                .values()
                .try_fold(u64::MAX, |acc, item| item.wal_pos.map(|p| acc.min(p))),
        )
    }

    /// Remove one item — **what a flush's publication does with exactly the entities it
    /// consumed** (§1.2).
    ///
    /// By entity id and never by range: the flush ran while the executor went on accepting ingest,
    /// so a range spanning the consumed ids would also take the rows that arrived meanwhile, which
    /// have no geometry and would be lost from both the buffer and every segment.
    pub fn remove(&mut self, entity: EntityId) {
        self.items.remove(&entity);
    }

    pub fn get(&self, entity: EntityId) -> Option<&BufferedItem> {
        self.items.get(&entity).map(|item| &**item)
    }

    pub fn contains(&self, entity: EntityId) -> bool {
        self.items.contains_key(&entity)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &BufferedItem)> {
        self.items.iter().map(|(entity, item)| (entity, &**item))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dict_with(descriptors: &[&[u8]]) -> (Dict, TempDir) {
        let temp = TempDir::new().unwrap();
        let mut writer = tessera_authz::DictWriter::new(temp.path());
        for d in descriptors {
            writer.intern(d);
        }
        let paths = writer.finish().unwrap();
        (Dict::load(&paths).unwrap(), temp)
    }

    #[test]
    fn resolver_prefers_dict_then_extends_deterministically() {
        let (dict, _temp) = dict_with(&[b"1207", b"9"]);
        let mut resolver = DescriptorResolver::new(&dict);

        assert_eq!(resolver.resolve(b"1207"), TermId::new(0));
        assert_eq!(resolver.resolve(b"9"), TermId::new(1));
        // Novel descriptor: extension counts down from `u32::MAX`, never up from `dict.len()`.
        assert_eq!(resolver.resolve(b"novel"), TermId::new(u32::MAX));
        // Repeat resolves to the same extension id.
        assert_eq!(resolver.resolve(b"novel"), TermId::new(u32::MAX));
        // A second distinct novel descriptor gets the next (one lower) id.
        assert_eq!(resolver.resolve(b"novel-2"), TermId::new(u32::MAX - 1));
    }

    /// Extension ids must never be able to collide with a dictionary ordinal,
    /// however large the dictionary grows — encoded as a property over dictionaries up to the ABI's
    /// declared `max_distinct_terms` bound (200,000,000), far below where extension ids start.
    #[test]
    fn extension_ids_never_collide_with_a_dictionary_sized_up_to_the_declared_bound() {
        const MAX_DISTINCT_TERMS: u32 = 200_000_000; // R6 declared_bounds().max_distinct_terms

        // The compile-time assertion next to `EXTENSION_ID_START`'s definition already proves
        // `EXTENSION_ID_START > MAX_DISTINCT_TERMS` unconditionally; a real dictionary this large
        // would be expensive to build in a unit test, so exercise the property that actually
        // depends on runtime behaviour: resolving several novel descriptors against a small real
        // dictionary never produces an id anywhere near dictionary-ordinal range.
        let (dict, _temp) = dict_with(&[b"a", b"b", b"c"]);
        let mut resolver = DescriptorResolver::new(&dict);
        for (i, descriptor) in [b"x".as_slice(), b"y", b"z"].iter().enumerate() {
            let id = resolver.resolve(descriptor).raw();
            assert!(
                id > MAX_DISTINCT_TERMS,
                "extension id {id} (descriptor #{i}) collides with the declared-bound dictionary \
                 ordinal range"
            );
        }
    }
}
