//! A generation: one immutable snapshot of engine state, held once for the bundle and per shard.

use std::sync::Arc;

use mosaica_authz::{
    DeltaTier, Dict, FragmentCache, FragmentCacheError, FrozenFragment, Grant, PostingsReader,
};
use mosaica_lifecycle::{IngestBuffer, Overlay};
use mosaica_store::read::{Bundle, ShardData};
use mosaica_store::vocabulary::Vocabularies;
use mosaica_types::{EntityId, ShardId};

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
}

/// A point shard's state in a generation, less what is derived from it. [`Shard`] derefs to this.
#[derive(Clone)]
pub struct ShardParts {
    /// Entities at or past this live only in `buffer`, never in the shard's permutations.
    pub watermark: u64,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// The mask-fragment cache, keyed by the MANIFEST digest of the prefix `postings` was read
    /// from, so a fold's new prefix is never answered from the old one's fragments.
    pub(crate) fragments: Arc<FragmentCache>,
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
    /// Bumped on every overlay or buffer swap; independent of `segments_version`.
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// The shard's filter columns; empty when the schema declares nothing filterable.
    pub filter_columns: Arc<crate::filter::FilterColumns>,
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

    /// The fragment for `grant` at this shard's watermark, waiting on a build already running.
    pub(crate) fn fragment(
        &self,
        grant: &Grant,
    ) -> Result<Arc<FrozenFragment>, FragmentCacheError> {
        self.fragments.get_or_build_grant_waiting(
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

    /// The MANIFEST digest of the prefix this shard's postings were read from.
    pub(crate) fn bundle_identity(&self) -> [u8; 32] {
        self.fragments.bundle_identity()
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

    /// The generation's one shard; panics if it holds any other number.
    pub fn only_shard(&self) -> &Shard {
        mosaica_types::only(&self.shards)
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

    /// The successor with `change` applied to what is held once and to shard `id`'s parts, and
    /// that shard replaced by what `next` makes of them.
    fn publish(
        &self,
        id: ShardId,
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
        next: impl FnOnce(&Shard, bool, Arc<ShardData>, ShardParts) -> Shard,
    ) -> Generation {
        let mut parts = self.parts.clone();
        let mut shard = self
            .shard(id)
            .unwrap_or_else(|| panic!("a publication names shard {id}, which is not held"))
            .parts
            .clone();
        change(&mut parts, &mut shard);
        let mut pending = Some((shard, next));
        self.succeeded_by(parts, |held, same_prefix, data| {
            let (parts, next) = pending.take_if(|_| held.id() == id)?;
            Some(next(held, same_prefix, data, parts))
        })
    }

    /// A copy with `change` applied to what is held once.
    pub(crate) fn with(&self, change: impl FnOnce(&mut GenerationParts)) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        self.succeeded_by(parts, |_, _, _| None)
    }

    /// A copy with `change` applied to what is held once and to shard `id`. A buffer-only change
    /// should use [`Self::with_buffer`], which derives less.
    pub(crate) fn with_shard(
        &self,
        id: ShardId,
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        self.publish(id, change, Shard::next)
    }

    /// [`Self::with_shard`] that also sets shard `id`'s buffer to `buffer`, which must be its
    /// present one with `inserted` added and anything else only removed; the buffered-row lists
    /// are then updated at the cost of those lists and `inserted`, not the buffer.
    pub(crate) fn with_buffer(
        &self,
        id: ShardId,
        buffer: Arc<IngestBuffer>,
        inserted: &[EntityId],
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        self.publish(
            id,
            |parts, shard| {
                change(parts, shard);
                shard.buffer = buffer;
            },
            |held, same_prefix, data, parts| held.next_buffer(same_prefix, data, parts, inserted),
        )
    }

    /// [`Self::with_buffer`] that also sets the overlay to `overlay`, which must differ from the
    /// shard's only by deletions and suppressions of `newly_denied`; their rows are added to the
    /// mask without walking the overlay. An unsuppress may not take this route: the entity may
    /// still be deleted.
    pub(crate) fn with_denies(
        &self,
        id: ShardId,
        overlay: Arc<Overlay>,
        newly_denied: &[EntityId],
        buffer: Arc<IngestBuffer>,
        inserted: &[EntityId],
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        self.publish(
            id,
            |parts, shard| {
                change(parts, shard);
                shard.overlay = overlay;
                shard.buffer = buffer;
            },
            |held, same_prefix, data, parts| {
                held.next_denies(same_prefix, data, parts, newly_denied, inserted)
            },
        )
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
            },
            vec![(
                shard,
                ShardParts {
                    watermark,
                    postings: Arc::new(
                        PostingsReader::open(&postings_path, false).expect("it opens"),
                    ),
                    fragments: Arc::new(FragmentCache::new(
                        std::path::Path::new("fixture-fragment-cache-never-written"),
                        [0u8; 32],
                        [0u8; 32],
                    )),
                    unique: Arc::default(),
                    unique_live: Arc::default(),
                    edited: Arc::default(),
                    edited_live: Arc::default(),
                    tenancy: Arc::default(),
                    delta_postings: Vec::new(),
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
    }
}
