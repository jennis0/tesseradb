//! A generation: one immutable snapshot of engine state, held once for the bundle and per shard.

use std::collections::BTreeMap;
use std::sync::Arc;

use mosaica_authz::{
    DeltaTier, Dict, FragmentCache, FragmentCacheError, FrozenFragment, Grant, PostingsReader,
};
use mosaica_lifecycle::{IngestBuffer, Overlay};
use mosaica_store::read::{Bundle, ShardData};
use mosaica_store::vocabulary::Vocabularies;
use mosaica_types::{EntityId, LayerSpace, ShardId};

use crate::DenyMask;

/// What a generation holds once for the bundle. [`Generation`] derefs to this.
#[derive(Clone)]
pub struct GenerationParts {
    /// The bundle's `CURRENT` prefix (e.g. `"v00000"`) this generation was loaded from.
    pub prefix: String,
    /// Monotone counter identifying this generation's segment set — bumped only when a new
    /// bundle build is loaded, never by an overlay/buffer update.
    pub segments_version: u64,
    pub bundle: Arc<Bundle>,
    /// The dictionary every shard's postings and buffered items resolve against. A flush that
    /// promotes a descriptor republishes it; ordinals are preserved, so a session authorised
    /// earlier keeps its terms and is not granted the promoted one.
    pub dict: Arc<Dict>,
    /// How many commit windows have committed an edit, how many folds have retired entities, and
    /// how many folds have freed ids the allocator issues again, since the process started: what a
    /// [`crate::edited::Stamp`] compares.
    pub(crate) edit_epoch: u64,
    pub(crate) fold_epoch: u64,
    pub(crate) free_epoch: u64,
    /// How many publications of deletions, suppressions and unsuppressions since the process
    /// started. An ingest does not move it.
    pub(crate) deny_epoch: u64,
    /// The live category bindings: key → code per vocabulary, and every code ever assigned. A mint
    /// publishes them with the buffer that uses them. A snapshot of the executor's copy: a handler
    /// may find a key bound or not and must never mint, or two racing handlers would split its
    /// rows across two codes.
    pub vocabularies: Arc<Vocabularies>,
    /// Every category vocabulary's suggestion index. Carried across publications, since a flush
    /// changes nothing it holds and a rebuild sorts for tens of seconds at 10⁷ values; a mint grows
    /// it with `vocabularies`.
    pub suggest: Arc<crate::suggest::SuggestIndexes>,
    /// Each layer space's deny state and supplied content. A space absent here denies nothing and
    /// has no content on disc.
    pub(crate) layer_spaces: Arc<LayerSpaces>,
}

/// A generation's layer spaces, by space.
pub(crate) type LayerSpaces = std::collections::BTreeMap<LayerSpace, LayerSpaceParts>;

/// One layer space's state in a generation.
#[derive(Clone, Default)]
pub(crate) struct LayerSpaceParts {
    /// Bumped on every change to `overlay`; what a content key reads a space's deny state by.
    pub(crate) overlay_version: u64,
    /// The space's deleted and suppressed artifacts, and the layer's own entity.
    pub(crate) overlay: Arc<Overlay>,
    /// The supplied content the prefix's manifest names for the space. Content published since is
    /// read from the artifact store.
    pub(crate) content: Option<Arc<mosaica_filter::RecordStack>>,
}

impl GenerationParts {
    /// `space`'s overlay, or an empty one where the space holds no deny.
    pub fn space_overlay(&self, space: LayerSpace) -> &Overlay {
        static EMPTY: std::sync::OnceLock<Overlay> = std::sync::OnceLock::new();
        match self.layer_spaces.get(&space) {
            Some(parts) => &parts.overlay,
            None => EMPTY.get_or_init(Overlay::new),
        }
    }

    /// Whether the layer holding `space` is deleted or suppressed: a deny of its own entity.
    pub(crate) fn layer_denied(&self, space: LayerSpace) -> bool {
        let overlay = self.space_overlay(space);
        let own = mosaica_types::layer::LAYER_ENTITY;
        overlay.is_deleted(own) || overlay.is_suppressed(own)
    }

    /// `space`'s supplied content on disc, where the prefix names any.
    pub(crate) fn space_content(
        &self,
        space: LayerSpace,
    ) -> Option<&Arc<mosaica_filter::RecordStack>> {
        self.layer_spaces.get(&space)?.content.as_ref()
    }

    /// Across every space: how many artifacts and layers are denied, and how many of those are
    /// deletions, which a fold removes the records of.
    pub(crate) fn space_overlay_figures(&self) -> (usize, u64) {
        self.layer_spaces.values().fold((0, 0), |(depth, deleted), parts| {
            (
                depth + parts.overlay.len(),
                deleted + parts.overlay.deleted_len(),
            )
        })
    }
}

/// A point shard's state in a generation, less what is derived from it. [`Shard`] derefs to this.
#[derive(Clone)]
pub struct ShardParts {
    /// Entities at or past this live only in `buffer`, never in the shard's permutations.
    pub watermark: u64,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// This shard's identity (`ShardRecord::identity`) in the prefix `postings` was read from.
    pub(crate) identity: [u8; 32],
    /// Every unique column's index runs, as the shard's record lists them.
    pub(crate) unique: Arc<mosaica_store::unique::UniqueIndexes>,
    /// The unique index entries of the rows the buffer holds, published with the buffer.
    pub(crate) unique_live: Arc<crate::unique::UniqueLive>,
    /// The edited-items map's runs, as the shard's record lists them.
    pub(crate) edited: Arc<mosaica_store::edited::EditedIndex>,
    /// The edited-items pairs no run holds yet, published with the buffer.
    pub(crate) edited_live: Arc<crate::edited::EditedLive>,
    /// Each number's tenancy, from the files the shard's record names: what an item's
    /// `mosaica_id` is formed and resolved at ([`crate::edited`]). Replaced only by a publication
    /// into a new prefix.
    pub(crate) tenancy: Arc<mosaica_store::tenancy::TenancyIndex>,
    /// One sparse delta postings tier per flush segment, in publication order; empty after a
    /// build.
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// The generation's `segments_version` at which this shard's row space last changed: what a
    /// row-space artefact of the shard is keyed by, so a flush, merge or compaction of another
    /// shard leaves it standing.
    pub segments_version: u64,
    /// This shard's `segments_version` before its row space last changed; `None` before its first
    /// generation.
    pub(crate) previous_segments_version: Option<u64>,
    /// Bumped on every overlay or buffer swap; independent of `segments_version`.
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// The shard's filter columns; empty when the schema declares nothing filterable.
    pub filter_columns: Arc<crate::filter::FilterColumns>,
}

impl ShardParts {
    /// Record that this shard's row space changed at the generation's `segments_version`.
    pub(crate) fn moved_to(&mut self, segments_version: u64) {
        self.previous_segments_version = Some(self.segments_version);
        self.segments_version = segments_version;
    }
}

/// One point shard of a generation. Its derived fields are functions of `data` and its parts, so
/// only this module sets them.
pub struct Shard {
    pub data: Arc<ShardData>,
    parts: ShardParts,
    /// **Derived**: per view, the row-space image of `deleted ∪ suppressed`, subtracted from every
    /// composed mask, by [`crate::compose::derive_denied`]. Never persisted; the overlay stays
    /// authoritative.
    denied: Arc<DenyMask>,
    /// **Derived**: per view, a version of [`Self::denied`]'s rows below the view's base, which
    /// moves exactly when those rows change: at a deny, a lift or a compaction that touches them,
    /// and never at an ingest or a flush. What a correction over the denied base rows is cached
    /// under ([`crate::figures`]). Drawn from one counter for the process, so a version is never
    /// reused for another set.
    deny_versions: Arc<DenyVersions>,
    /// **Derived** by [`crate::compose::derive_buffered_rows`]: per view, the buffered entities
    /// that have a row there. Entity ids, because a merge, coalesce or fold rewrites rows.
    buffered_rows: Arc<crate::BufferedRows>,
}

/// The parts of the shards a publication names, by shard, for its change to edit.
pub(crate) struct ShardsParts(BTreeMap<ShardId, ShardParts>);

impl ShardsParts {
    /// Shard `id`'s parts. Panics where the publication does not name `id`.
    pub(crate) fn of(&mut self, id: ShardId) -> &mut ShardParts {
        self.0
            .get_mut(&id)
            .unwrap_or_else(|| panic!("the publication does not name shard {id}"))
    }
}

/// A shard's new buffer: its present one with `inserted` added and anything else only removed.
pub(crate) struct BufferSwap<'a> {
    pub(crate) buffer: Arc<IngestBuffer>,
    pub(crate) inserted: &'a [EntityId],
}

/// A shard's new overlay, which differs from its present one only by deletions and suppressions
/// of `newly_denied`, and its new buffer.
pub(crate) struct DenySwap<'a> {
    pub(crate) overlay: Arc<Overlay>,
    pub(crate) newly_denied: &'a [EntityId],
    pub(crate) buffer: BufferSwap<'a>,
}

/// What a publication sets in one shard it names beyond what its change sets, which decides how
/// the shard's derived fields are carried.
enum ShardSwap<'a> {
    Parts,
    Buffer(BufferSwap<'a>),
    Denies(DenySwap<'a>),
}

/// One immutable, atomically-swappable snapshot of engine state: what the bundle holds once, and
/// each point shard.
#[derive(Clone)]
pub struct Generation {
    parts: GenerationParts,
    /// Ascending by id.
    shards: Vec<Arc<Shard>>,
}

/// Per view, the version of its denied base rows: see [`Shard::deny_version`].
pub(crate) type DenyVersions = rustc_hash::FxHashMap<String, u64>;

/// The deny versions for `denied` over `data`: each view's carried from `previous` where its
/// denied rows below the base are the same set over the same base, and a fresh one otherwise.
fn deny_versions(
    previous: Option<&Shard>,
    denied: &DenyMask,
    data: &ShardData,
) -> Arc<DenyVersions> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let below = |rows: &croaring::Bitmap, base_rows: u32| {
        let mut out = rows.clone();
        out.remove_range(base_rows..);
        out
    };
    let mut out = DenyVersions::default();
    for (view, view_data) in &data.views {
        let base_rows = view_data.row_space.base_rows();
        let now = denied.get(view).map(|rows| below(rows, base_rows));
        let carried = previous.and_then(|previous| {
            let version = *previous.deny_versions.get(view)?;
            let before = previous.denied.get(view).map(|rows| below(rows, base_rows));
            (before == now).then_some(version)
        });
        let version =
            carried.unwrap_or_else(|| NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        out.insert(view.clone(), version);
    }
    Arc::new(out)
}

impl std::ops::Deref for Generation {
    type Target = GenerationParts;

    fn deref(&self) -> &GenerationParts {
        &self.parts
    }
}

impl std::ops::Deref for Shard {
    type Target = ShardParts;

    fn deref(&self) -> &ShardParts {
        &self.parts
    }
}

impl Shard {
    fn new(data: Arc<ShardData>, parts: ShardParts) -> Shard {
        let denied = Arc::new(crate::compose::derive_denied(&parts.overlay, &data));
        let buffered_rows = Arc::new(crate::compose::derive_buffered_rows(&parts.buffer, &data));
        let deny_versions = deny_versions(None, &denied, &data);
        Shard {
            data,
            parts,
            denied,
            deny_versions,
            buffered_rows,
        }
    }

    /// The successor over `data` holding these, with its deny versions; none is carried across a
    /// new prefix, since a fold renumbers the base.
    fn successor(
        &self,
        same_prefix: bool,
        data: Arc<ShardData>,
        parts: ShardParts,
        denied: Arc<DenyMask>,
        buffered_rows: Arc<crate::BufferedRows>,
    ) -> Shard {
        let deny_versions = match Arc::ptr_eq(&denied, &self.denied) && same_prefix {
            true => Arc::clone(&self.deny_versions),
            false => deny_versions(same_prefix.then_some(self), &denied, &data),
        };
        Shard {
            data,
            parts,
            denied,
            deny_versions,
            buffered_rows,
        }
    }

    /// This shard's mask where `data` and `overlay` are its own, else one derived over them.
    fn carried_denied(&self, data: &Arc<ShardData>, overlay: &Arc<Overlay>) -> Arc<DenyMask> {
        match Arc::ptr_eq(data, &self.data) && Arc::ptr_eq(overlay, &self.overlay) {
            true => Arc::clone(&self.denied),
            false => Arc::new(crate::compose::derive_denied(overlay, data)),
        }
    }

    fn next(&self, same_prefix: bool, data: Arc<ShardData>, parts: ShardParts) -> Shard {
        let denied = self.carried_denied(&data, &parts.overlay);
        let buffered_rows =
            match Arc::ptr_eq(&data, &self.data) && Arc::ptr_eq(&parts.buffer, &self.buffer) {
                true => Arc::clone(&self.buffered_rows),
                false => Arc::new(crate::compose::derive_buffered_rows(&parts.buffer, &data)),
            };
        self.successor(same_prefix, data, parts, denied, buffered_rows)
    }

    fn next_buffer(
        &self,
        same_prefix: bool,
        data: Arc<ShardData>,
        parts: ShardParts,
        inserted: &[EntityId],
    ) -> Shard {
        let denied = self.carried_denied(&data, &parts.overlay);
        let buffered_rows = self.next_buffered_rows(&data, &parts.buffer, inserted);
        self.successor(same_prefix, data, parts, denied, buffered_rows)
    }

    fn next_denies(
        &self,
        same_prefix: bool,
        data: Arc<ShardData>,
        parts: ShardParts,
        newly_denied: &[EntityId],
        inserted: &[EntityId],
    ) -> Shard {
        let mut denied = (*self.denied).clone();
        for (view, view_data) in &data.views {
            let Some(rows) = denied.get_mut(view) else {
                continue;
            };
            for entity in newly_denied {
                if let Some(row) = view_data.row_space.row_of(*entity) {
                    rows.add(row.raw());
                }
            }
        }
        debug_assert!(
            Arc::ptr_eq(&data, &self.data)
                && denied == crate::compose::derive_denied(&parts.overlay, &data),
            "the incremental deny mask does not equal a fresh derivation"
        );
        let buffered_rows = self.next_buffered_rows(&data, &parts.buffer, inserted);
        self.successor(same_prefix, data, parts, Arc::new(denied), buffered_rows)
    }

    fn next_buffered_rows(
        &self,
        data: &Arc<ShardData>,
        buffer: &IngestBuffer,
        inserted: &[EntityId],
    ) -> Arc<crate::BufferedRows> {
        if !Arc::ptr_eq(data, &self.data) {
            return Arc::new(crate::compose::derive_buffered_rows(buffer, data));
        }
        let mut next = crate::BufferedRows::default();
        for (view, view_data) in &data.views {
            let mut entities: Vec<EntityId> = match self.buffered_rows.get(view) {
                Some(held) => held
                    .iter()
                    .copied()
                    .filter(|entity| buffer.get(*entity).is_some())
                    .collect(),
                None => Vec::new(),
            };
            entities.extend(inserted.iter().copied().filter(|entity| {
                buffer.get(*entity).is_some() && view_data.row_space.row_of(*entity).is_some()
            }));
            entities.sort_unstable();
            entities.dedup();
            next.insert(view.clone(), entities);
        }
        debug_assert!(
            next == crate::compose::derive_buffered_rows(buffer, data),
            "the incremental buffered-row lists do not equal a fresh derivation"
        );
        Arc::new(next)
    }

    pub(crate) fn id(&self) -> ShardId {
        self.data.record.id
    }

    /// The row-space deny mask, per view.
    pub fn denied(&self) -> &DenyMask {
        &self.denied
    }

    /// `view`'s deny mask, refused with [`crate::EngineError::DenyMaskMissing`] where it has none.
    pub(crate) fn denied_in(&self, view: &str) -> crate::error::Result<&croaring::Bitmap> {
        self.denied
            .get(view)
            .ok_or_else(|| crate::EngineError::DenyMaskMissing {
                view: view.to_string(),
            })
    }

    /// [`crate::compose::compose`] in `view`, over this shard's overlay, buffer and buffered-row
    /// list.
    pub(crate) fn compose(
        &self,
        satisfied: &dyn crate::session::SatisfiedKeys,
        projection: Arc<crate::projection::RowProjection>,
        view: &str,
        row_space: &mosaica_store::RowSpace,
        denied: &croaring::Bitmap,
    ) -> crate::EffectiveMask {
        crate::compose::compose(
            satisfied,
            &self.overlay,
            &self.buffer,
            projection,
            row_space,
            denied,
            self.buffered_rows(view),
        )
    }

    /// The fragment for `grant` at this shard's watermark, from `fragments`, waiting on a build
    /// already running.
    pub(crate) fn fragment(
        &self,
        fragments: &FragmentCache,
        grant: &Grant,
    ) -> Result<Arc<FrozenFragment>, FragmentCacheError> {
        fragments.get_or_build_grant_waiting(
            self.identity,
            grant,
            &self.postings,
            &self.delta_postings,
            self.watermark,
            &mosaica_cache::NeverCancelled,
        )
    }

    /// See [`Self::deny_versions`]'s field. Zero for a view this shard does not carry.
    pub(crate) fn deny_version(&self, view: &str) -> u64 {
        self.deny_versions.get(view).copied().unwrap_or(0)
    }

    /// One view's buffered entities that have a row there — [`compose`](crate::compose::compose)'s
    /// walk. `None` where this shard has no list for the view, which sends the walk over the whole
    /// buffer instead.
    pub fn buffered_rows(&self, view: &str) -> Option<&[EntityId]> {
        self.buffered_rows.get(view).map(Vec::as_slice)
    }

    /// What a containment partition is composed from, in this shard: its base postings.
    pub(crate) fn partition_source(&self) -> crate::containment::PartitionSource<'_> {
        crate::containment::PartitionSource {
            postings: &self.postings,
        }
    }
}

impl Generation {
    pub(crate) fn new(parts: GenerationParts, shards: Vec<(ShardId, ShardParts)>) -> Generation {
        let mut shards: Vec<Arc<Shard>> = shards
            .into_iter()
            .map(|(id, shard)| {
                let data = parts
                    .bundle
                    .shard(id)
                    .unwrap_or_else(|| panic!("the bundle holds no shard {id}"));
                Arc::new(Shard::new(Arc::clone(data), shard))
            })
            .collect();
        shards.sort_by_key(|shard| shard.id());
        Generation { parts, shards }
    }

    pub(crate) fn shard(&self, id: ShardId) -> Option<&Shard> {
        self.shards
            .iter()
            .find(|shard| shard.id() == id)
            .map(Arc::as_ref)
    }

    /// Shard `id`. Panics where the generation holds no such shard: an item is read from the
    /// generation it was resolved against, and every shard of it.
    pub(crate) fn shard_of(&self, id: ShardId) -> &Shard {
        self.shard(id)
            .unwrap_or_else(|| panic!("the generation holds no shard {id}"))
    }

    /// The generation's one shard; panics if it holds any other number.
    pub fn only_shard(&self) -> &Shard {
        mosaica_types::only(&self.shards)
    }

    /// Every shard, ascending by id.
    pub(crate) fn shards(&self) -> impl Iterator<Item = &Shard> {
        self.shards.iter().map(Arc::as_ref)
    }

    /// The first shard's filter columns, which admit a filter and route a field's reads.
    /// Placement, uniqueness and filterability are declared, so the same in every shard; whether
    /// a column holds a value layer is the first shard's data.
    pub(crate) fn declared_columns(&self) -> &crate::filter::FilterColumns {
        &self.shards[0].filter_columns
    }

    /// The successor holding `parts`: the shard `changed` answers for is replaced, and every other
    /// is carried, re-derived where its data changed.
    fn succeeded_by(
        &self,
        parts: GenerationParts,
        mut changed: impl FnMut(&Shard, bool, Arc<ShardData>) -> Option<Shard>,
    ) -> Generation {
        let same_prefix = parts.prefix == self.parts.prefix;
        let shards = self
            .shards
            .iter()
            .map(|shard| {
                let id = shard.id();
                let data = Arc::clone(
                    parts
                        .bundle
                        .shard(id)
                        .unwrap_or_else(|| panic!("the bundle holds no shard {id}")),
                );
                if let Some(next) = changed(shard, same_prefix, Arc::clone(&data)) {
                    return Arc::new(next);
                }
                match same_prefix && Arc::ptr_eq(&data, &shard.data) {
                    true => Arc::clone(shard),
                    false => Arc::new(shard.next(same_prefix, data, shard.parts.clone())),
                }
            })
            .collect();
        Generation { parts, shards }
    }

    /// The successor with `change` applied to what is held once and to the parts of every shard
    /// `swaps` names, each of those shards then taking its swap and replaced by what its swap
    /// derives. Every other shard is carried.
    fn publish(
        &self,
        mut swaps: BTreeMap<ShardId, ShardSwap<'_>>,
        change: impl FnOnce(&mut GenerationParts, &mut ShardsParts),
    ) -> Generation {
        let mut parts = self.parts.clone();
        let mut shards = ShardsParts(
            swaps
                .keys()
                .map(|&id| {
                    let held = self.shard(id).unwrap_or_else(|| {
                        panic!("a publication names shard {id}, which is not held")
                    });
                    (id, held.parts.clone())
                })
                .collect(),
        );
        change(&mut parts, &mut shards);
        for (id, swap) in &swaps {
            let shard = shards.of(*id);
            match swap {
                ShardSwap::Parts => {}
                ShardSwap::Buffer(swap) => shard.buffer = Arc::clone(&swap.buffer),
                ShardSwap::Denies(swap) => {
                    shard.overlay = Arc::clone(&swap.overlay);
                    shard.buffer = Arc::clone(&swap.buffer.buffer);
                }
            }
        }
        let mut shards = shards.0;
        self.succeeded_by(parts, |held, same_prefix, data| {
            let swap = swaps.remove(&held.id())?;
            let parts = shards.remove(&held.id())?;
            Some(match swap {
                ShardSwap::Parts => held.next(same_prefix, data, parts),
                ShardSwap::Buffer(swap) => {
                    held.next_buffer(same_prefix, data, parts, swap.inserted)
                }
                ShardSwap::Denies(swap) => held.next_denies(
                    same_prefix,
                    data,
                    parts,
                    swap.newly_denied,
                    swap.buffer.inserted,
                ),
            })
        })
    }

    /// A copy with `change` applied to what is held once.
    pub(crate) fn with(&self, change: impl FnOnce(&mut GenerationParts)) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        self.succeeded_by(parts, |_, _, _| None)
    }

    /// A copy with `change` applied to what is held once and to each shard of `ids`. A
    /// buffer-only change should use [`Self::with_buffer`], which derives less.
    pub(crate) fn with_shards(
        &self,
        ids: impl IntoIterator<Item = ShardId>,
        change: impl FnOnce(&mut GenerationParts, &mut ShardsParts),
    ) -> Generation {
        let swaps = ids.into_iter().map(|id| (id, ShardSwap::Parts)).collect();
        self.publish(swaps, change)
    }

    /// [`Self::with_shards`] over shard `id` alone.
    pub(crate) fn with_shard(
        &self,
        id: ShardId,
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        self.with_shards([id], |parts, shards| change(parts, shards.of(id)))
    }

    /// [`Self::with_shards`] that also sets each named shard's buffer to its swap's, which must be
    /// its present one with the swap's `inserted` added and anything else only removed; the
    /// buffered-row lists are then updated at the cost of those lists and `inserted`, not the
    /// buffer.
    pub(crate) fn with_buffer(
        &self,
        buffers: BTreeMap<ShardId, BufferSwap<'_>>,
        change: impl FnOnce(&mut GenerationParts, &mut ShardsParts),
    ) -> Generation {
        let swaps = buffers
            .into_iter()
            .map(|(id, swap)| (id, ShardSwap::Buffer(swap)))
            .collect();
        self.publish(swaps, change)
    }

    /// [`Self::with_buffer`] that also sets each named shard's overlay to its swap's, which must
    /// differ from the shard's only by deletions and suppressions of the swap's `newly_denied`;
    /// their rows are added to the mask without walking the overlay. An unsuppress may not take
    /// this route: the entity may still be deleted.
    pub(crate) fn with_denies(
        &self,
        denies: BTreeMap<ShardId, DenySwap<'_>>,
        change: impl FnOnce(&mut GenerationParts, &mut ShardsParts),
    ) -> Generation {
        let swaps = denies
            .into_iter()
            .map(|(id, swap)| (id, ShardSwap::Denies(swap)))
            .collect();
        self.publish(swaps, change)
    }

    /// `shard`'s `segments_version` in the generation one below this one: its own where its row
    /// space did not change at this one. What a superseded row-space artefact of the shard is kept
    /// at, so a request may be served one generation stale while a refresh builds the live one.
    pub(crate) fn segments_version_before(&self, shard: &Shard) -> Option<u64> {
        match shard.segments_version == self.segments_version {
            true => shard.previous_segments_version,
            false => Some(shard.segments_version),
        }
    }

    /// One number that moves when any shard's overlay or any layer space's does: what a stamp
    /// over every deny compares.
    pub(crate) fn deny_state_version(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        for shard in self.shards() {
            shard.overlay_version.hash(&mut hasher);
        }
        for (space, parts) in self.layer_spaces.iter() {
            (space.raw(), parts.overlay_version).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// A generation over an empty bundle of one shard, for in-crate tests that read a few scalars
    /// off one.
    #[cfg(test)]
    pub(crate) fn synthetic(
        prefix: &str,
        segments_version: u64,
        watermark: u64,
        overlay: Overlay,
        buffer: IngestBuffer,
    ) -> Generation {
        let manifest = mosaica_store::manifest::Manifest {
            bundle_format: 3,
            created_at: String::new(),
            declared_scalars: vec![],
            vocabularies: vec![],
            small_term_threshold: 32,
            identity: mosaica_store::manifest::IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
            },
            groups: Vec::new(),
            views: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: std::collections::BTreeMap::new(),
        };
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let postings_path = dir.path().join("postings.arrow");
        mosaica_authz::write_postings(&postings_path, &[], 32).expect("an empty postings file");
        let bundle = Arc::new(Bundle {
            manifest,
            partitions: std::collections::HashMap::from([(
                "default".to_string(),
                partition_of(mosaica_store::manifest::SegmentsManifest::empty()),
            )]),
        });
        let shard = bundle.only_shard().record.id;
        Generation::new(
            GenerationParts {
                prefix: prefix.to_string(),
                segments_version,
                bundle,
                dict: Arc::new(Dict::load(&[]).expect("an empty dict needs no file")),
                edit_epoch: 0,
                fold_epoch: 0,
                free_epoch: 0,
                deny_epoch: 0,
                vocabularies: Arc::new(Vocabularies::default()),
                suggest: Arc::new(crate::suggest::SuggestIndexes::default()),
                layer_spaces: Arc::default(),
            },
            vec![(
                shard,
                ShardParts {
                    watermark,
                    postings: Arc::new(
                        PostingsReader::open(&postings_path, false).expect("it opens"),
                    ),
                    identity: [0u8; 32],
                    unique: Arc::default(),
                    unique_live: Arc::default(),
                    edited: Arc::default(),
                    edited_live: Arc::default(),
                    tenancy: Arc::default(),
                    delta_postings: Vec::new(),
                    segments_version,
                    previous_segments_version: segments_version.checked_sub(1),
                    overlay_version: 0,
                    overlay: Arc::new(overlay),
                    buffer: Arc::new(buffer),
                    filter_columns: Arc::new(crate::filter::FilterColumns::default()),
                },
            )],
        )
    }
}

/// `manifest` as a served partition, its shards carrying no view.
#[cfg(test)]
pub(crate) fn partition_of(
    manifest: mosaica_store::manifest::SegmentsManifest,
) -> mosaica_store::read::PartitionData {
    mosaica_store::read::PartitionData {
        manifest: manifest.partition,
        shard_size: manifest.shards.size,
        next_shard_id: manifest.shards.next_id,
        refs: Default::default(),
        segments_n: 0,
        highest_candidate_n: 0,
        shards: manifest
            .shards
            .records
            .into_iter()
            .map(|record| {
                Arc::new(ShardData {
                    record,
                    views: std::collections::HashMap::new(),
                })
            })
            .collect(),
        layer_spaces: manifest.layer_spaces,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generation of `count` shards, each holding what [`Generation::synthetic`]'s one does.
    fn shards_of(count: u32) -> Generation {
        let one = Generation::synthetic("v00000", 0, 0, Overlay::new(), IngestBuffer::new());
        let mut manifest = mosaica_store::manifest::SegmentsManifest::empty();
        manifest.shards.records = (0..count)
            .map(|id| mosaica_store::manifest::ShardRecord::empty(ShardId::new(id).unwrap()))
            .collect();
        let mut parts = one.parts.clone();
        parts.bundle = Arc::new(Bundle {
            manifest: one.bundle.manifest.clone(),
            partitions: std::collections::HashMap::from([(
                "default".to_string(),
                partition_of(manifest),
            )]),
        });
        let shard = one.only_shard().parts.clone();
        Generation::new(
            parts,
            (0..count)
                .map(|id| (ShardId::new(id).unwrap(), shard.clone()))
                .collect(),
        )
    }

    #[test]
    fn a_swap_over_two_of_three_shards_moves_those_two_once_each() {
        let generation = shards_of(3);
        let [first, second, third] = [0, 1, 2].map(|id| ShardId::new(id).unwrap());
        let mut moved = Vec::new();
        let next = generation.with_shards([first, third], |parts, shards| {
            parts.deny_epoch += 1;
            for id in [first, third] {
                shards.of(id).overlay_version += 1;
                moved.push(id);
            }
        });
        assert_eq!(moved, vec![first, third]);
        assert_eq!(next.deny_epoch, generation.deny_epoch + 1);
        for id in [first, third] {
            assert_eq!(
                next.shard_of(id).overlay_version,
                generation.shard_of(id).overlay_version + 1
            );
        }
        assert!(
            std::ptr::eq(next.shard_of(second), generation.shard_of(second)),
            "the shard the swap does not name is carried"
        );
        assert_eq!(
            next.shards().map(Shard::id).collect::<Vec<_>>(),
            vec![first, second, third]
        );
    }
}
