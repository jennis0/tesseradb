//! A generation: one immutable snapshot of engine state. What the bundle holds once is on the
//! generation; what a point shard holds is on that shard's [`Shard`], with the deny mask and the
//! buffered-row lists derived from it.

use std::sync::Arc;

use mosaica_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use mosaica_lifecycle::{IngestBuffer, Overlay};
use mosaica_store::read::{Bundle, ShardData};
use mosaica_store::vocabulary::Vocabularies;
use mosaica_types::{EntityId, ShardId};

use crate::DenyMask;

/// What a generation holds once for the bundle. [`Generation`] derefs to this, so
/// `generation.bundle` reads as it always did.
#[derive(Clone)]
pub struct GenerationParts {
    /// The bundle's `CURRENT` prefix (e.g. `"v00000"`) this generation was loaded from.
    pub prefix: String,
    /// Monotone counter identifying this generation's segment set — bumped only when a new
    /// bundle build is loaded, never by an overlay/buffer update.
    pub segments_version: u64,
    pub bundle: Arc<Bundle>,
    /// The dictionary this generation's postings and buffered items resolve against.
    ///
    /// **Generation-scoped rather than process-scoped, because a flush promotes.** A novel
    /// descriptor buffers an item under an unsatisfiable extension id and becomes a durable
    /// ordinal only when the flush that carries it publishes a `dict_extents` entry, so the
    /// dictionary grows with geometry and has to be republished alongside it. Ordinals are
    /// preserved across a promotion ([`mosaica_authz::Dict::load_extending`]), so a session
    /// authorised against an older generation keeps evaluating the terms it was granted; what it
    /// does *not* get is the newly promoted one, which is fail-closed.
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
    /// The live category bindings: key → code per vocabulary, plus the assigned-code set that makes
    /// never-reuse hold.
    ///
    /// **On the generation, because a mint publishes.** A novel key acquires its code at a commit
    /// window's close and becomes durable in the same fsync as the rows that use it, so the
    /// bindings grow exactly when the buffer does and have to be republished alongside it — the
    /// same reason `dict` lives here. A request that loads one generation pointer gets the buffer,
    /// the geometry and the bindings that agree.
    ///
    /// **Read-only here; the executor owns the authoritative copy.** Minting is serial by
    /// construction and this is a published snapshot of it, exactly as a shard's `overlay` is of
    /// the live overlay. A handler resolving a key through this may find it bound or not; it must
    /// never mint, because two handlers racing one novel key would draw two codes for it and split
    /// its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// Every category vocabulary's suggestion index.
    ///
    /// **Carried across publications rather than rebuilt with them.** A flush changes which
    /// entities carry a value and changes nothing this index holds — it is over the value set, and
    /// the mask never enters it — so a per-generation rebuild would pay a sort measured in tens of
    /// seconds at 10⁷ values for a change it cannot see. It is on the generation all the same,
    /// because a *mint* publishes: a novel key acquires its code at a commit window's close and
    /// must be suggestible on the next keystroke, so the side map grows exactly when
    /// [`GenerationParts::vocabularies`] does and travels with it.
    pub suggest: Arc<crate::suggest::SuggestIndexes>,
}

/// What a point shard holds in a generation but what is derived from it. [`Shard`] derefs to this.
#[derive(Clone)]
pub struct ShardParts {
    /// The highest entity id folded into the shard's row geometry. Entities at or past this value
    /// live only in `buffer`, never in the shard's permutations.
    pub watermark: u64,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// The mask-fragment cache, and through it the MANIFEST digest of the prefix `postings` was
    /// read from.
    ///
    /// A fold rewrites the term index and publishes a new prefix, so every fragment built from the
    /// old one names entities the new postings no longer contain, and it advances no watermark.
    /// Held beside `postings`, a request loads one pointer and gets postings, identity and fragment
    /// cache that agree.
    ///
    /// Two holders sit outside it — a `Session`'s own `Arc<FrozenFragment>`, and
    /// `SessionGeometry`'s — and a replaced container reaches neither. The comparison is therefore
    /// made at composition, against [`mosaica_authz::FrozenFragment::identity`]:
    /// `Engine::fragment_for` for the first, `RowProjectionCache::freshest_fragment`'s prefix
    /// scoping for the second.
    pub(crate) fragments: Arc<FragmentCache>,
    /// Every unique column's index runs, as the shard's record lists them. Replaced by every
    /// publication that changes the list, sharing the runs it already opened.
    pub(crate) unique: Arc<mosaica_store::unique::UniqueIndexes>,
    /// The unique index entries of the rows the buffer holds, published with the buffer.
    pub(crate) unique_live: Arc<crate::unique::UniqueLive>,
    /// The edited-items map's runs, as the shard's record lists them. Replaced by every
    /// publication that changes the list, sharing the runs it already opened.
    pub(crate) edited: Arc<mosaica_store::edited::EditedIndex>,
    /// The edited-items pairs no run holds yet, published with the buffer.
    pub(crate) edited_live: Arc<crate::edited::EditedLive>,
    /// Each number's tenancy, from the files the shard's record names: what an item's
    /// `mosaica_id` is formed and resolved at ([`crate::edited`]). Replaced only by a publication
    /// into a new prefix.
    pub(crate) tenancy: Arc<mosaica_store::tenancy::TenancyIndex>,
    /// One sparse delta postings tier per flush segment, in publication order.
    ///
    /// A fragment build unions the base with every live tier over the session's satisfied terms.
    /// A flush publishes one and the entity-space coalesce merges several into one, so the set
    /// changes with each publication. Empty in a bundle straight out of `mosaica build`.
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// Monotone counter bumped on every overlay/buffer swap (independent of `segments_version` —
    /// an overlay change never touches the bundle).
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// The shard's filter columns, opened once per generation. A new prefix brings new columns,
    /// and a session reading the old generation keeps reading the old ones. Empty when the schema
    /// declares nothing filterable.
    pub filter_columns: Arc<crate::filter::FilterColumns>,
}

/// One point shard of a generation: the bundle's data for it, what the engine holds for it, and
/// what is derived from the two.
///
/// The derived values are functions of the parts and the data, so only this module sets them: a
/// shard is built with [`Generation::new`] or carried into a successor by one of
/// [`Generation`]'s publications, and each derives them.
pub struct Shard {
    /// The shard's data in the generation's bundle.
    pub data: Arc<ShardData>,
    parts: ShardParts,
    /// **The deny mask**: per view, the row-space image of `deleted ∪ suppressed`, subtracted from
    /// every composed mask.
    ///
    /// **Derived, never persisted, never a second source of truth.** The three entity-space stores
    /// on [`Overlay`] remain authoritative, and `compose::verdict` remains the single answer for
    /// every entity-space verb. Folded in as a bitmap, the deny half of composition costs one
    /// `andnot` rather than a walk over every deny ever accepted.
    ///
    /// Row ids mean something only within one `segments_version`, so the mask is rebuilt by every
    /// geometry publication and travels with the row space it addresses. The derivation rule is in
    /// [`crate::compose::derive_denied`]; an unsuppress may not subtract a row.
    ///
    /// Keyed by view. A view the shard carries always has an entry, empty when nothing is denied;
    /// a missing entry means the mask and the data disagree, and the read path treats that as
    /// fail-closed rather than as "nothing denied".
    denied: Arc<DenyMask>,
    /// **Derived**: per view, a version of [`Self::denied`]'s rows below the view's base, which
    /// moves exactly when those rows change: at a deny, a lift or a compaction that touches them,
    /// and never at an ingest or a flush. What a correction over the denied base rows is cached
    /// under ([`crate::figures`]). Drawn from one counter for the process, so a version is never
    /// reused for another set.
    deny_versions: Arc<DenyVersions>,
    /// **Derived**: per view, the buffered entities [`crate::compose`]'s walk has anything to say
    /// about, so a request pays for those rather than for the whole buffer. Entity ids, never row
    /// ids, because a merge, coalesce or fold rewrites rows. The rule is
    /// [`crate::compose::derive_buffered_rows`]; a view with no entry sends the walk back over the
    /// buffer, which is slow and never wrong.
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
    /// A shard over `data` holding `parts`, with everything derived from them.
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

    /// The deny versions of a successor whose mask is `denied` over `data`. A fold publishes a new
    /// prefix and renumbers the base, so nothing is carried across one.
    fn next_deny_versions(
        &self,
        same_prefix: bool,
        denied: &Arc<DenyMask>,
        data: &ShardData,
    ) -> Arc<DenyVersions> {
        if Arc::ptr_eq(denied, &self.denied) && same_prefix {
            return Arc::clone(&self.deny_versions);
        }
        deny_versions(same_prefix.then_some(self), denied, data)
    }

    /// The successor over `data` holding `parts`. The mask is derived again if the data or the
    /// overlay changed, and the buffered-row lists if the data or the buffer did; both are carried
    /// otherwise.
    fn next(&self, same_prefix: bool, data: Arc<ShardData>, parts: ShardParts) -> Shard {
        let same_data = Arc::ptr_eq(&data, &self.data);
        let denied = match same_data && Arc::ptr_eq(&parts.overlay, &self.overlay) {
            true => Arc::clone(&self.denied),
            false => Arc::new(crate::compose::derive_denied(&parts.overlay, &data)),
        };
        let buffered_rows = match same_data && Arc::ptr_eq(&parts.buffer, &self.buffer) {
            true => Arc::clone(&self.buffered_rows),
            false => Arc::new(crate::compose::derive_buffered_rows(&parts.buffer, &data)),
        };
        let deny_versions = self.next_deny_versions(same_prefix, &denied, &data);
        Shard {
            data,
            parts,
            denied,
            deny_versions,
            buffered_rows,
        }
    }

    /// The successor over `data` holding `parts`, whose buffer is this one's with `inserted` added
    /// and anything else only removed. The mask is carried if the data and the overlay are this
    /// one's.
    fn next_buffer(
        &self,
        same_prefix: bool,
        data: Arc<ShardData>,
        parts: ShardParts,
        inserted: &[EntityId],
    ) -> Shard {
        let denied =
            match Arc::ptr_eq(&data, &self.data) && Arc::ptr_eq(&parts.overlay, &self.overlay) {
                true => Arc::clone(&self.denied),
                false => Arc::new(crate::compose::derive_denied(&parts.overlay, &data)),
            };
        let buffered_rows = self.next_buffered_rows(&data, &parts.buffer, inserted);
        let deny_versions = self.next_deny_versions(same_prefix, &denied, &data);
        Shard {
            data,
            parts,
            denied,
            deny_versions,
            buffered_rows,
        }
    }

    /// [`Self::next_buffer`] where the overlay differs from this one's only by deletions and
    /// suppressions of `newly_denied`, so their rows are added to the mask without walking the
    /// whole overlay.
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
        let denied = Arc::new(denied);
        let deny_versions = self.next_deny_versions(same_prefix, &denied, &data);
        Shard {
            data,
            parts,
            denied,
            deny_versions,
            buffered_rows,
        }
    }

    /// The buffered-row lists over `data` for `buffer`, which is this shard's with `inserted` added
    /// and anything else only removed.
    fn next_buffered_rows(
        &self,
        data: &Arc<ShardData>,
        buffer: &IngestBuffer,
        inserted: &[EntityId],
    ) -> Arc<crate::BufferedRows> {
        // The held lists are against this shard's row spaces; against any other they are derived
        // afresh, in a release build too.
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

    pub fn id(&self) -> ShardId {
        self.data.record.id
    }

    /// The row-space deny mask, per view.
    pub fn denied(&self) -> &DenyMask {
        &self.denied
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

    /// The MANIFEST digest of the prefix this shard's postings and term index were read from — see
    /// [`ShardParts::fragments`], which is where it is held so that it and the cache it keys cannot
    /// disagree.
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
    /// A generation holding `parts` and, for each `(id, parts)` of `shards`, a shard over the
    /// bundle's shard `id`.
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

    /// The shard numbered `id`.
    pub fn shard(&self, id: ShardId) -> Option<&Shard> {
        self.shards
            .iter()
            .find(|shard| shard.id() == id)
            .map(Arc::as_ref)
    }

    /// Every shard, ascending by id.
    pub fn shards(&self) -> impl Iterator<Item = &Shard> {
        self.shards.iter().map(Arc::as_ref)
    }

    /// The one shard a generation holds until several are served.
    pub fn only_shard(&self) -> &Shard {
        mosaica_types::only(&self.shards)
    }

    /// The parts of shard `id`, which a publication into it starts from.
    fn parts_of(&self, id: ShardId) -> ShardParts {
        self.shard(id)
            .unwrap_or_else(|| panic!("a publication names shard {id}, which is not held"))
            .parts
            .clone()
    }

    /// The successor holding `parts`, each shard over the data `parts`' bundle holds for it: the
    /// one `changed` answers for, and every other carried with its derived values derived again
    /// where its data changed.
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

    /// A copy with `change` applied to what is held once.
    pub(crate) fn with(&self, change: impl FnOnce(&mut GenerationParts)) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        self.succeeded_by(parts, |_, _, _| None)
    }

    /// A copy with `change` applied to what is held once and to shard `id`. A buffer-only change
    /// has [`Generation::with_buffer`], which derives less, so a call site that forgets it is slow
    /// rather than wrong.
    pub(crate) fn with_shard(
        &self,
        id: ShardId,
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        let (mut parts, mut shard) = (self.parts.clone(), self.parts_of(id));
        change(&mut parts, &mut shard);
        let mut shard = Some(shard);
        self.succeeded_by(parts, |held, same_prefix, data| {
            if held.id() != id {
                return None;
            }
            shard
                .take()
                .map(|parts| held.next(same_prefix, data, parts))
        })
    }

    /// A copy whose shard `id` has buffer `buffer`, with `change` applied. The buffered-row lists
    /// are the shard's minus the entities whose own row has left the buffer, plus those of
    /// `inserted` that hold one and have a row. Cost is the lists and `inserted`, not the buffer.
    pub(crate) fn with_buffer(
        &self,
        id: ShardId,
        buffer: Arc<IngestBuffer>,
        inserted: &[EntityId],
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        let (mut parts, mut shard) = (self.parts.clone(), self.parts_of(id));
        change(&mut parts, &mut shard);
        shard.buffer = buffer;
        let mut shard = Some(shard);
        self.succeeded_by(parts, |held, same_prefix, data| {
            if held.id() != id {
                return None;
            }
            shard
                .take()
                .map(|parts| held.next_buffer(same_prefix, data, parts, inserted))
        })
    }

    /// A copy whose shard `id`'s overlay differs from its present one only by deletions and
    /// suppressions of `newly_denied`, so their rows are added to the mask without walking the
    /// whole overlay. An unsuppress may not take this route: the entity may still be deleted.
    /// `buffer` is the shard's with the deleted entities' rows removed and the rows of `inserted`
    /// added, so the buffered-row lists lose the one and gain the other.
    pub(crate) fn with_denies(
        &self,
        id: ShardId,
        overlay: Arc<Overlay>,
        newly_denied: &[EntityId],
        buffer: Arc<IngestBuffer>,
        inserted: &[EntityId],
        change: impl FnOnce(&mut GenerationParts, &mut ShardParts),
    ) -> Generation {
        let (mut parts, mut shard) = (self.parts.clone(), self.parts_of(id));
        change(&mut parts, &mut shard);
        shard.overlay = overlay;
        shard.buffer = buffer;
        let mut shard = Some(shard);
        self.succeeded_by(parts, |held, same_prefix, data| {
            if held.id() != id {
                return None;
            }
            shard
                .take()
                .map(|parts| held.next_denies(same_prefix, data, parts, newly_denied, inserted))
        })
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
        Generation::new(
            GenerationParts {
                prefix: prefix.to_string(),
                segments_version,
                bundle: Arc::new(Bundle {
                    manifest,
                    partitions: std::collections::HashMap::from([(
                        "default".to_string(),
                        mosaica_store::read::PartitionData {
                            manifest: mosaica_store::manifest::PartitionManifest::empty(),
                            shard_size: mosaica_types::SHARD_SIZE_DEFAULT,
                            segments_n: 0,
                            highest_candidate_n: 0,
                            shards: vec![Arc::new(ShardData {
                                record: mosaica_store::manifest::ShardRecord::empty(ShardId::ZERO),
                                views: std::collections::HashMap::new(),
                            })],
                        },
                    )]),
                }),
                dict: Arc::new(Dict::load(&[]).expect("an empty dict needs no file")),
                edit_epoch: 0,
                fold_epoch: 0,
                free_epoch: 0,
                deny_epoch: 0,
                vocabularies: Arc::new(Vocabularies::default()),
                suggest: Arc::new(crate::suggest::SuggestIndexes::default()),
            },
            vec![(
                ShardId::ZERO,
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
