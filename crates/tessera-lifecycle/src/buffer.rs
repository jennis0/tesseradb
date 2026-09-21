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
/// `view` is carried because a flush reads the *buffer*, not the WAL, and has to know which row
/// space each item's row belongs in — see [`crate::wal::WalRow`]'s field for why that cannot be
/// re-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferedItem {
    pub terms: Vec<TermId>,
    pub view: String,
    /// **This row joined an entity that already exists to a second view** (`views.md` §4) — it
    /// carries geometry and nothing else.
    ///
    /// The entity, its label and its entity-scoped attributes are the ones it already had, so a
    /// join carries no descriptors, contributes no postings and no filter-column value, and is
    /// invisible to every entity-space walk over this buffer ([`IngestBuffer::get`] and
    /// [`IngestBuffer::iter`] answer with the entity's *own* row). That is structural rather than
    /// disciplinary: a join that contributed terms would be a re-label with no overlay entry —
    /// exactly what decision 0047 makes a delete plus a re-ingest — and the shape here is what
    /// makes it unreachable.
    pub join: bool,
    pub x: f64,
    pub y: f64,
    pub scalars: Vec<WalScalar>,
    /// This row's values for the **group-scoped** attribute families of its view's group
    /// ([`WalRow::scoped`], `views.md` §5) — positional against
    /// `MANIFEST.groups[..].scoped_scalars` in manifest order, and empty everywhere no family is
    /// in scope.
    ///
    /// **Held per `(entity, view)`, which this buffer already is.** A scoped value belongs to the
    /// pair and not to the entity, so an entity buffered in two views of one group carries each
    /// view's own value here — including on a **join** row, the one thing a second view's row
    /// brings with it beyond geometry (`views.md` §4).
    pub scoped: Vec<WalScalar>,
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
    ///
    /// **The row list is behind an `Arc` too, for the same reason one step out.** With a bare
    /// `Vec` as the value, cloning the map allocates one `Vec` per *entity* — so a close paid a
    /// malloc and a copy per buffered row however cheap the items themselves had become, measured
    /// at ~145 ns per entry and 14.3 µs per ingested row at a 1M-row buffer
    /// (`probes/2026-09-04-ingest-executor/`). Behind an `Arc` the clone copies control bytes and
    /// pointers and allocates nothing, and the four mutators go through `Arc::make_mut`, which
    /// copies one entity's list — usually a single element — only where a published generation
    /// still shares it.
    /// **Keyed by entity, one entry per view that entity has a row in** — usually exactly one.
    ///
    /// An entity may hold a row in several views at once (`views.md` §4: the same point in two
    /// views is two batches and one `external_id`), and both of them may be awaiting the same
    /// flush. A map keyed by entity alone would have let the second overwrite the first, losing an
    /// acked row silently; keyed by `(entity, view)` alone, every entity-space reader here would
    /// have had to dedupe. The entity's own row — the one that carries its terms — is the first
    /// element, which is what makes [`IngestBuffer::get`] a lookup rather than a scan.
    items: FxHashMap<EntityId, Arc<Vec<Arc<BufferedItem>>>>,
    /// Rows, not entities: what the occupancy bound counts and what a flush consumes.
    rows: usize,
    /// Entities holding an own (non-join) row — exactly what [`Self::iter`] yields, maintained by
    /// the three mutators that can change it rather than walked. A tick landing behind a flush
    /// asks for this figure, and a walk of a million-row buffer on the executor thread is tens of
    /// milliseconds in the one state where the buffer is that large.
    owning: usize,
    /// The **entity-scoped** cells an accepted `POST /control/values` batch filled and no flush
    /// has written yet (`ingest.md` §1.4), keyed by the entity they fill.
    ///
    /// **A second map rather than a row in `items`, because a fill is not a row.** It carries no
    /// geometry, no label and no external id: it creates nothing and names an entity that exists,
    /// so every entity-space walk over this buffer must go on answering with the entity's own row
    /// and every "is this entity already in this view" arm must go on saying what it said. What a
    /// fill is for is the homes a flush writes from the buffer — the family's entity-space
    /// structure, the text layer and the record blob — and those read it through [`Self::fills`].
    ///
    /// One entry per entity, because an entity-scoped value belongs to the entity and to no view.
    fills: FxHashMap<EntityId, Arc<Fill>>,
    /// The **group-scoped** cells of the same batches, keyed by `(entity, owner view)`.
    ///
    /// **Not by entity, because a scoped value is not the entity's** (`views.md` §5): its address
    /// is `(attribute → its group, key)`, so one entity may hold an unflushed cell under two keys
    /// of one group and under the keys of two different groups at once. Keyed by entity alone,
    /// the second would overwrite the first — and a second *group*'s values would be merged
    /// positionally against the first group's family list, putting a value in another family's
    /// slot. The owner view is what makes each cell's list its own: every entry under one key
    /// resolves to one owning group, so `scoped` is positional against that group's
    /// `scoped_scalars` and against nothing else.
    scoped_fills: FxHashMap<(EntityId, String), Arc<ScopedFill>>,
}

/// One entity's unflushed **entity-scoped** filled cells (`ingest.md` §1.4): the values a
/// `POST /control/values` batch supplied for cells nothing held, waiting for the flush that writes
/// them into the family's entity-space structure and the record blob.
///
/// `scalars` is positional exactly as [`BufferedItem::scalars`] is, and every cell the fill rule
/// dropped — one nothing supplied, or one a held value already equalled — is `WalScalar::Null`,
/// so a flush writes a slot for none of them.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    /// The view the batch named. An entity-scoped value belongs to no view; what this decides is
    /// which flush pass writes the cells, the entity-space extents and the record blob being
    /// written once per pass and named for no view.
    pub view: String,
    pub scalars: Vec<WalScalar>,
    /// The WAL position of the `ValuesBatch` record this arrived in — what pins the log until the
    /// flush writes the cells, on [`BufferedItem::wal_pos`]'s rule.
    pub wal_pos: Option<u64>,
}

/// One `(entity, owner view)` cell's unflushed **group-scoped** values (`views.md` §5).
///
/// `scoped` is positional against the owning group's `scoped_scalars`, which the owner view in
/// the key determines.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedFill {
    /// The view the batch named — the door, where the key is the address. It decides which flush
    /// pass writes the cells, and that pass resolves it to the owner view the key names.
    pub view: String,
    pub scoped: Vec<WalScalar>,
    pub wal_pos: Option<u64>,
}

impl IngestBuffer {
    pub fn new() -> Self {
        IngestBuffer {
            items: FxHashMap::default(),
            rows: 0,
            owning: 0,
            fills: FxHashMap::default(),
            scoped_fills: FxHashMap::default(),
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
        let item = Arc::new(BufferedItem {
            terms,
            external_id: row.external_id.clone(),
            view: row.view.clone(),
            join: row.join,
            x: row.x,
            y: row.y,
            scalars: row.scalars.clone(),
            scoped: row.scoped.clone(),
            wal_pos: None,
        });
        // `make_mut` on a fresh entry is in place (refcount one); on an entity a published
        // generation still holds, it copies that entity's list alone.
        let rows = Arc::make_mut(self.items.entry(row.entity_id).or_default());
        let owned_before = owns_a_row(rows);
        // **One row per (entity, view), and a repeat replaces rather than accumulates.** The
        // ingest join refuses a second row in a view the entity is already in — that is the arm
        // the permutation *and* this buffer are both consulted for — so a replacement here is
        // replay meeting a row it has already seen, never two acked rows for one position.
        let added = match rows.iter_mut().find(|held| held.view == row.view) {
            Some(existing) => {
                *existing = item;
                false
            }
            // The entity's own row goes first, a join after it, so `get` answers with the row that
            // carries the entity's terms whatever order the two arrived in.
            None => {
                if row.join {
                    rows.push(item);
                } else {
                    rows.insert(0, item);
                }
                true
            }
        };
        let owned_after = owns_a_row(rows);
        if added {
            self.rows += 1;
        }
        self.note_own_row(owned_before, owned_after);
    }

    /// Carry [`Self::owning`] across one entity's list gaining or losing its own row.
    fn note_own_row(&mut self, before: bool, after: bool) {
        match (before, after) {
            (false, true) => self.owning += 1,
            (true, false) => self.owning -= 1,
            _ => {}
        }
    }

    /// Record which WAL position `entity`'s row arrived at. No-op if the entity is not buffered,
    /// which is the ordinary case for a stamp arriving after a flush has consumed the row.
    pub fn set_wal_pos(&mut self, entity: EntityId, view: &str, wal_pos: u64) {
        if let Some(rows) = self.items.get_mut(&entity) {
            if let Some(item) = Arc::make_mut(rows).iter_mut().find(|item| item.view == view) {
                // Copies only if a published generation still shares this item; at the call site it
                // is stamped immediately after insert, where the refcount is one and this is in
                // place.
                Arc::make_mut(item).wal_pos = Some(wal_pos);
            }
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
    ///
    /// **A fill pins the log too**, for the reason a row does: the `ValuesBatch` record is the
    /// only copy of the values until the flush writes them into the family's extent and the
    /// record blob (`ingest.md` §1.4).
    pub fn oldest_wal_pos(&self) -> Option<Option<u64>> {
        if self.items.is_empty() && self.fills.is_empty() && self.scoped_fills.is_empty() {
            return None;
        }
        Some(
            self.items
                .values()
                .flat_map(|rows| rows.iter())
                .map(|item| item.wal_pos)
                .chain(self.fills.values().map(|fill| fill.wal_pos))
                .chain(self.scoped_fills.values().map(|fill| fill.wal_pos))
                .try_fold(u64::MAX, |acc, pos| pos.map(|p| acc.min(p))),
        )
    }

    /// Record one accepted values batch's **entity-scoped** cells for `entity`, merging with any
    /// this entity already holds unflushed.
    ///
    /// **Merged rather than replaced.** Two batches may fill different columns of one entity
    /// between two ticks, and the fill rule has already refused any cell either of them holds — so
    /// a cell carrying a value in the held fill keeps it, and one that is absent there takes this
    /// batch's. The `view` of the first fill stands: an entity-scoped value belongs to no view,
    /// and what the field decides is only which pass writes the cells.
    pub fn fill(&mut self, entity: EntityId, fill: Fill, absent: impl Fn(&WalScalar) -> bool) {
        match self.fills.get_mut(&entity) {
            None => {
                self.fills.insert(entity, Arc::new(fill));
            }
            Some(held) => {
                let held = Arc::make_mut(held);
                merge_cells(&mut held.scalars, fill.scalars, &absent);
                held.wal_pos = oldest_of(held.wal_pos, fill.wal_pos);
            }
        }
    }

    /// Record one accepted values batch's **group-scoped** cells for the `(entity, owner view)`
    /// cell they address, on [`Self::fill`]'s merge rule (`views.md` §5).
    pub fn fill_scoped(
        &mut self,
        entity: EntityId,
        owner_view: String,
        fill: ScopedFill,
        absent: impl Fn(&WalScalar) -> bool,
    ) {
        match self.scoped_fills.get_mut(&(entity, owner_view.clone())) {
            None => {
                self.scoped_fills.insert((entity, owner_view), Arc::new(fill));
            }
            Some(held) => {
                let held = Arc::make_mut(held);
                merge_cells(&mut held.scoped, fill.scoped, &absent);
                held.wal_pos = oldest_of(held.wal_pos, fill.wal_pos);
            }
        }
    }

    /// Stamp the WAL position of the record a fill arrived in, on [`Self::set_wal_pos`]'s terms.
    pub fn set_fill_wal_pos(&mut self, entity: EntityId, wal_pos: u64) {
        if let Some(fill) = self.fills.get_mut(&entity) {
            let fill = Arc::make_mut(fill);
            fill.wal_pos = Some(oldest_of(fill.wal_pos, Some(wal_pos)).unwrap_or(wal_pos));
        }
    }

    /// [`Self::set_fill_wal_pos`] for one scoped cell.
    pub fn set_scoped_fill_wal_pos(&mut self, entity: EntityId, owner_view: &str, wal_pos: u64) {
        if let Some(fill) = self.scoped_fills.get_mut(&(entity, owner_view.to_string())) {
            let fill = Arc::make_mut(fill);
            fill.wal_pos = Some(oldest_of(fill.wal_pos, Some(wal_pos)).unwrap_or(wal_pos));
        }
    }

    /// Every unflushed entity-scoped fill, with the entity it fills — the flush's second walk
    /// beside [`Self::rows`], scoped to one view by the caller.
    pub fn fills(&self) -> impl Iterator<Item = (&EntityId, &Fill)> {
        self.fills.iter().map(|(entity, fill)| (entity, &**fill))
    }

    /// Every unflushed group-scoped fill, with the `(entity, owner view)` cell it fills.
    pub fn scoped_fills(&self) -> impl Iterator<Item = (&(EntityId, String), &ScopedFill)> {
        self.scoped_fills.iter().map(|(key, fill)| (key, &**fill))
    }

    /// This entity's unflushed entity-scoped cells, or `None` where it holds none — **a lookup,
    /// not a scan**: the fill rule asks this once per row of a batch, and a batch is capped at
    /// `max_batch_rows`.
    pub fn fill_of(&self, entity: EntityId) -> Option<&Fill> {
        self.fills.get(&entity).map(|fill| &**fill)
    }

    /// This `(entity, owner view)` cell's unflushed values, on [`Self::fill_of`]'s terms.
    pub fn scoped_fill_of(&self, entity: EntityId, owner_view: &str) -> Option<&ScopedFill> {
        // The key is borrowed as a pair, which `FxHashMap` cannot look up without owning the
        // string; the allocation is one per row per scoped column and is what keys the cell by
        // its address rather than by the entity.
        self.scoped_fills
            .get(&(entity, owner_view.to_string()))
            .map(|fill| &**fill)
    }

    /// Drop one entity's entity-scoped fill — what a flush's publication does with exactly the
    /// fills its plan consumed.
    pub fn remove_fill(&mut self, entity: EntityId) {
        self.fills.remove(&entity);
    }

    /// Drop one `(entity, owner view)` cell's fill, on [`Self::remove_fill`]'s terms.
    pub fn remove_scoped_fill(&mut self, entity: EntityId, owner_view: &str) {
        self.scoped_fills.remove(&(entity, owner_view.to_string()));
    }

    /// How many unflushed fills this buffer holds, entity-scoped and scoped together.
    /// Diagnostic, and the flush's "is there anything to do" test beside [`Self::len`].
    pub fn fill_count(&self) -> usize {
        self.fills.len() + self.scoped_fills.len()
    }

    /// Whether anything buffered belongs to one of these entities: a row, a fill or a scoped fill.
    pub fn holds_any(&self, entities: &[EntityId]) -> bool {
        entities
            .iter()
            .any(|e| self.items.contains_key(e) || self.fills.contains_key(e))
            || (!self.scoped_fills.is_empty()
                && self
                    .scoped_fills
                    .keys()
                    .any(|(entity, _)| entities.contains(entity)))
    }

    /// Drops everything buffered for a deleted entity: its rows and its fills. No flush consumes
    /// a deleted entity's rows or fills, and each holds the log at its position, so one left here
    /// would stop the log rotating.
    pub fn remove(&mut self, entity: EntityId) {
        if let Some(rows) = self.items.remove(&entity) {
            self.rows -= rows.len();
            self.note_own_row(owns_a_row(&rows), false);
        }
        self.fills.remove(&entity);
        self.scoped_fills.retain(|(held, _), _| *held != entity);
    }

    /// Drop everything buffered for every entity `condemned` answers `true` for, on
    /// [`Self::remove`]'s terms — asked of every entity this buffer holds anything for, rows, fills
    /// and scoped fills alike, rather than of those holding an own row.
    pub fn remove_where(&mut self, condemned: impl Fn(EntityId) -> bool) {
        let held: Vec<EntityId> = self
            .items
            .keys()
            .chain(self.fills.keys())
            .copied()
            .chain(self.scoped_fills.keys().map(|(entity, _)| *entity))
            .filter(|entity| condemned(*entity))
            .collect();
        for entity in held {
            self.remove(entity);
        }
    }

    /// Remove one **(entity, view)** row — what a flush's publication does with exactly the rows
    /// it consumed, and what a dropped view does with the rows that named it.
    ///
    /// A flush consumes one view at a time, so removing the entity outright would take a row of
    /// another view with it — a row that has no geometry, is in no segment, and would be lost from
    /// both.
    pub fn remove_in_view(&mut self, entity: EntityId, view: &str) {
        let Some(rows) = self.items.get_mut(&entity) else {
            return;
        };
        let before = rows.len();
        let rows = Arc::make_mut(rows);
        let owned_before = owns_a_row(rows);
        rows.retain(|item| item.view != view);
        let owned_after = owns_a_row(rows);
        let removed = before - rows.len();
        let empty = rows.is_empty();
        self.rows -= removed;
        self.note_own_row(owned_before, owned_after);
        if empty {
            self.items.remove(&entity);
        }
    }

    /// The entity's **own** row — the one carrying its terms, its external id and its scalars —
    /// or `None` where every buffered row for it is a join (`views.md` §4).
    ///
    /// **A join is not an answer here, and that is what keeps a second view out of the
    /// authorisation path.** A join carries no terms, so returning one would put an entity whose
    /// label lives in a segment through the buffer's rule and judge it by an empty term set —
    /// invisible to everyone until the next flush. `None` means "the buffer has no opinion", which
    /// sends the caller to the fragment that does.
    pub fn get(&self, entity: EntityId) -> Option<&BufferedItem> {
        self.items
            .get(&entity)
            .and_then(|rows| rows.iter().find(|item| !item.join))
            .map(|item| &**item)
    }

    /// Every buffered row this entity holds, in **any** view — joins included.
    ///
    /// **The scoped cell arm's source** (`views.md` §5, decision 0116). A scoped value is addressed
    /// by `(entity, attribute, key)`, so the question "does this deployment already hold a value
    /// for the cell this row names" is asked of every row of the entity whose view resolves to the
    /// same key, not of the entity's own row alone — which is what [`Self::get`] answers and is a
    /// different question, about labels.
    pub fn rows_of(&self, entity: EntityId) -> impl Iterator<Item = &BufferedItem> {
        self.items
            .get(&entity)
            .into_iter()
            .flat_map(|rows| rows.iter().map(|item| &**item))
    }

    /// Does this entity hold **any** buffered row?
    pub fn contains(&self, entity: EntityId) -> bool {
        self.items.contains_key(&entity)
    }

    /// Does this entity hold a buffered row **in this view**? — the commit-window half of the join
    /// rule's "already in the view" arm (`views.md` §4). A row accepted but not yet flushed is in
    /// no permutation, and a check that missed it would let two batches in one window hand flush
    /// two rows for one entity in one view.
    pub fn contains_in_view(&self, entity: EntityId, view: &str) -> bool {
        self.items
            .get(&entity)
            .is_some_and(|rows| rows.iter().any(|item| item.view == view))
    }

    /// Every entity the buffer has an **opinion** about, with its own row — the entity-space walk
    /// (`compose`, `filter`), which is about labels and dispositions rather than about geometry.
    /// An entity whose only buffered rows are joins is absent, exactly as [`Self::get`] is `None`
    /// for it.
    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &BufferedItem)> {
        self.items.iter().filter_map(|(entity, rows)| {
            rows.iter()
                .find(|item| !item.join)
                .map(|item| (entity, &**item))
        })
    }

    /// Every buffered **row**, joins included — the flush's walk, which is about geometry and is
    /// scoped to one view.
    pub fn rows(&self) -> impl Iterator<Item = (&EntityId, &BufferedItem)> {
        self.items
            .iter()
            .flat_map(|(entity, rows)| rows.iter().map(move |item| (entity, &**item)))
    }

    /// **Rows, not entities.** The occupancy bound is about what a flush has to write and what a
    /// window's clone has to copy, and both are per row.
    pub fn len(&self) -> usize {
        self.rows
    }

    /// How many entities [`Self::iter`] yields: those holding an own (non-join) row. Maintained,
    /// so this is a read rather than a walk.
    pub fn owning_entities(&self) -> usize {
        self.owning
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.fills.is_empty() && self.scoped_fills.is_empty()
    }
}

/// Whether one entity's list carries the entity's own row, the one [`IngestBuffer::iter`] answers
/// with.
fn owns_a_row(rows: &[Arc<BufferedItem>]) -> bool {
    rows.iter().any(|item| !item.join)
}

/// The older of two WAL positions, and `None` where either is unknown — `None` meaning "not
/// known" rather than zero, on [`BufferedItem::wal_pos`]'s rule.
fn oldest_of(held: Option<u64>, supplied: Option<u64>) -> Option<u64> {
    match (held, supplied) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => None,
    }
}

/// Take `supplied`'s value into every cell of `held` that is absent, widening `held` where the
/// supplied list is longer — the shape a declaration between two batches leaves.
fn merge_cells(
    held: &mut Vec<WalScalar>,
    supplied: Vec<WalScalar>,
    absent: &impl Fn(&WalScalar) -> bool,
) {
    if held.len() < supplied.len() {
        held.resize(supplied.len(), WalScalar::Null);
    }
    for (position, value) in supplied.into_iter().enumerate() {
        if absent(&value) {
            continue;
        }
        if absent(&held[position]) {
            held[position] = value;
        }
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

    fn row(entity: u64, view: &str, join: bool) -> WalRow {
        WalRow {
            external_id: Some(format!("ext-{entity}-{view}").into_bytes()),
            entity_id: EntityId::new(entity),
            view: view.to_string(),
            join,
            descriptors: Vec::new(),
            x: 0.0,
            y: 0.0,
            scalars: Vec::new(),
            scoped: Vec::new(),
        }
    }

    /// The maintained count is what the walk answers, after every mutator that can move it.
    #[test]
    fn the_owning_count_is_the_walk() {
        let mut buffer = IngestBuffer::new();
        let check = |buffer: &IngestBuffer, at: &str| {
            assert_eq!(
                buffer.owning_entities(),
                buffer.iter().count(),
                "after {at}"
            );
        };

        check(&buffer, "an empty buffer");
        buffer.insert_row_with_terms(&row(1, "a", false), vec![TermId::new(1)]);
        buffer.insert_row_with_terms(&row(1, "b", true), Vec::new());
        check(&buffer, "an own row and a join of one entity");

        // A join alone: the entity's own row lives in a segment, so the walk has no opinion on it.
        buffer.insert_row_with_terms(&row(2, "b", true), Vec::new());
        check(&buffer, "a join-only entity");

        buffer.insert_row_with_terms(&row(3, "a", false), vec![TermId::new(2)]);
        // A replay of a row already held replaces rather than accumulates.
        buffer.insert_row_with_terms(&row(3, "a", false), vec![TermId::new(2)]);
        check(&buffer, "a restated row");

        buffer.remove_in_view(EntityId::new(1), "a");
        check(&buffer, "the own row of an entity that keeps a join");
        buffer.remove_in_view(EntityId::new(1), "b");
        check(&buffer, "that entity's last row");

        buffer.remove(EntityId::new(3));
        check(&buffer, "a whole entity");
        buffer.remove(EntityId::new(2));
        check(&buffer, "a join-only entity removed whole");

        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.owning_entities(), 0);
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
