//! A generation: one immutable snapshot of engine state, and the deny mask derived from it.

use std::sync::Arc;

use tessera_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::read::Bundle;
use tessera_store::vocabulary::Vocabularies;
use tessera_types::EntityId;

use crate::DenyMask;

/// Everything a generation holds but its deny mask. [`Generation`] derefs to this, so
/// `generation.bundle` reads as it always did.
#[derive(Clone)]
pub struct GenerationParts {
    /// The bundle's `CURRENT` prefix (e.g. `"v00000"`) this generation was loaded from.
    pub prefix: String,
    /// Monotone counter identifying this generation's segment set — bumped only when a new
    /// bundle build is loaded, never by an overlay/buffer update.
    pub segments_version: u64,
    /// The SEGMENTS manifest's watermark: the highest entity id folded into the bundle's row
    /// geometry. Entities at or past this value live only in `buffer`, never in `bundle`'s
    /// permutations (I1 composition rule 4).
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
    /// The dictionary this generation's postings and buffered items resolve against.
    ///
    /// **Generation-scoped rather than process-scoped, because a flush promotes.** A novel
    /// descriptor buffers an item under an unsatisfiable extension id and becomes a durable
    /// ordinal only when the flush that carries it publishes a `dict_extents` entry (§3.2), so
    /// the dictionary grows with geometry and has to be republished alongside it. Ordinals are
    /// preserved across a promotion ([`tessera_authz::Dict::load_extending`]), so a session
    /// authorised against an older generation keeps evaluating the terms it was granted; what it
    /// does *not* get is the newly promoted one, which is fail-closed and is what §3.3's
    /// staleness hint exists to advertise.
    pub dict: Arc<Dict>,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// The mask-fragment cache, and through it **this generation's bundle identity** — the
    /// MANIFEST digest of the prefix `postings` was read from.
    ///
    /// **On the generation rather than beside it, and that is what makes Rule F's safety
    /// structural** (write-path §5.4, compaction §4). A fold rewrites the term index and publishes
    /// a new prefix, so every fragment built from the old one names entities the new postings no
    /// longer contain — and it advances no watermark, so nothing keyed on the watermark can see
    /// it. Bound at `Engine::open` for the process lifetime, as it was, nothing could rotate the
    /// fragment identity in-process at all, and a fold published through that seam would leave
    /// every pre-fold fragment reachable by key — *including* the persisted `.frag` files, across
    /// a restart. Here, a request loads one pointer and gets postings, identity and fragment cache
    /// that agree, exactly as I11's within-request rule already requires for geometry.
    ///
    /// **Swapping this is necessary and is not sufficient.** Two holders sit outside it — a
    /// `Session`'s own `Arc<FrozenFragment>`, and `SessionGeometry`'s — and a replaced container
    /// reaches neither. The comparison is therefore made at composition, against
    /// [`tessera_authz::FrozenFragment::identity`]: `Engine::fragment_for` for the first,
    /// `RowProjectionCache::freshest_fragment`'s prefix scoping for the second.
    pub(crate) fragments: Arc<FragmentCache>,
    /// External ids established by the bundle's runs — the reader half of contracts §2.4.
    ///
    /// **Per generation, because a fold is not content-preserving.** The entity-space coalesce
    /// that first made this swappable is: an old sidecar and a new generation answer identically
    /// for every key, so which one a request held could not be observed. A fold drops the retired
    /// entities' keys (compaction §3, pass 3) and rewrites the locator into a new prefix, so a
    /// request pairing the new geometry with the pre-fold sidecar would resolve through files the
    /// old prefix holds and reclamation is about to delete. One pointer, one answer.
    pub(crate) external_index: Arc<crate::engine::ExternalIdIndex>,
    /// One sparse delta postings tier per flush segment, in publication order.
    ///
    /// A fragment build unions the base with every live tier over the session's satisfied terms
    /// (§5.2). They live on the generation rather than on the engine for the same reason the
    /// dictionary does: a flush publishes one, and a merge coalesces several into one, so the set
    /// changes exactly when geometry does. Empty in a bundle straight out of `tessera build`.
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// Monotone counter bumped on every overlay/buffer swap (independent of `segments_version` —
    /// an overlay change never touches the bundle).
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// The live category bindings: key → code per vocabulary, plus the assigned-code set that makes
    /// never-reuse hold (per-point-attributes §3.4).
    ///
    /// **On the generation, because a mint publishes.** A novel key acquires its code at a commit
    /// window's close and becomes durable in the same fsync as the rows that use it, so the
    /// bindings grow exactly when the buffer does and have to be republished alongside it — the
    /// same reason `dict` lives here. A request that loads one generation pointer gets the buffer,
    /// the geometry and the bindings that agree.
    ///
    /// **Read-only here; the executor owns the authoritative copy.** Minting is serial by
    /// construction (write-path §1.1) and this is a published snapshot of it, exactly as `overlay`
    /// is of the live overlay. A handler resolving a key through this may find it bound or not; it
    /// must never mint, because two handlers racing one novel key would draw two codes for it and
    /// split its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// **The deny mask**: per view, the row-space image of `deleted ∪ suppressed`, subtracted
    /// from every composed mask (I1).
    ///
    /// The bundle's filter columns, opened once per generation.
    ///
    /// **Generation-scoped for the same reason the dictionary is**: the artefact belongs to the
    /// published prefix, so a new bundle brings new columns and a session reading the old
    /// generation keeps reading the old ones. Empty when the schema declares nothing filterable.
    pub filter_columns: Arc<crate::filter::FilterColumns>,
    /// Every category vocabulary's suggestion index (`value-suggestion.md` §6.1).
    ///
    /// **Carried across publications rather than rebuilt with them.** A flush changes which
    /// entities carry a value and changes nothing this index holds — it is over the value set, and
    /// the mask never enters it — so a per-generation rebuild would pay a sort measured in tens of
    /// seconds at 10⁷ values for a change it cannot see. It is on the generation all the same,
    /// because a *mint* publishes: a novel key acquires its code at a commit window's close and
    /// must be suggestible on the next keystroke, so the side map grows exactly when
    /// [`Generation::vocabularies`] does and travels with it.
    pub suggest: Arc<crate::suggest::SuggestIndexes>,
}


/// One immutable, atomically-swappable snapshot of engine state (lifecycle §1.1).
///
/// **⊘ No compaction exists**, but this type is what one would publish: the fields a fold rotates
/// — the base postings, the fragment cache and the bundle identity it keys, and the external-id
/// sidecar — are here rather than on `Engine`, which is what makes a prefix flip expressible at all
/// (compaction §4). Merge publishes through this type too: the entity-space coalesce without moving
/// `segments_version`, the row-space merge as its own swap (`crate::coalesce`, `crate::merge`).
///
/// The row-space deny mask is a function of the overlay and the bundle, so only this module can
/// set it: a generation is built with [`Generation::new`] or copied with [`Generation::with`],
/// and both derive the mask.
#[derive(Clone)]
pub struct Generation {
    parts: GenerationParts,
    /// **Derived, never persisted, never a second source of truth.** The three entity-space stores
    /// on [`Overlay`] remain authoritative, and `compose::verdict` remains the single answer for
    /// every entity-space verb — `visible_to`, label gating, cluster visibility. This exists
    /// because the *row-space* question was being answered by walking the deny sets and resolving
    /// `row_of` per denied entity on every request, which made per-request work grow with denies
    /// **ever accepted**. Folded in as a bitmap, the deny half of composition costs one `andnot`.
    ///
    /// **It cannot go stale, because it never outlives its generation.** Row ids mean something
    /// only within one `segments_version`, so the mask is rebuilt by every geometry publication and
    /// travels with the row space it addresses — a request that loads one generation pointer gets
    /// the overlay, the buffer and the mask that agree.
    ///
    /// **The derivation rule is in [`crate::compose::derive_denied`]**, and the trap it names —
    /// that an unsuppress may not subtract a row — is the one way this could silently re-expose a
    /// deleted item. No build site sets it: see [`Generation::with`].
    ///
    /// Keyed by view, because row space is. A view the bundle carries always has an entry, empty
    /// when nothing is denied; a missing entry means the mask and the bundle disagree about what
    /// this generation holds, and the read path treats that as fail-closed rather than as "nothing
    /// denied".
    denied: Arc<DenyMask>,
    /// **Derived**: per view, the buffered entities [`crate::compose`]'s walk has anything to say
    /// about, so a request pays for those rather than for the whole buffer. Entity ids, never row
    /// ids, because a merge, coalesce or fold rewrites rows. The rule is
    /// [`crate::compose::derive_buffered_rows`] and only this module sets it; a view with no entry
    /// sends the walk back over the buffer, which is slow and never wrong.
    buffered_rows: Arc<crate::BufferedRows>,
}

impl std::ops::Deref for Generation {
    type Target = GenerationParts;

    fn deref(&self) -> &GenerationParts {
        &self.parts
    }
}

impl Generation {
    /// Builds a generation and derives both its derived values.
    pub(crate) fn new(parts: GenerationParts) -> Generation {
        let denied = Arc::new(crate::compose::derive_denied(&parts.overlay, &parts.bundle));
        let buffered_rows = Arc::new(crate::compose::derive_buffered_rows(
            &parts.buffer,
            &parts.bundle,
        ));
        Generation {
            parts,
            denied,
            buffered_rows,
        }
    }

    /// A copy with `change` applied. The mask is derived again if the change replaced the bundle
    /// or the overlay, and the buffered-row lists if it replaced the bundle or the buffer; both
    /// are carried otherwise. A buffer-only change has [`Generation::with_buffer`], which derives
    /// neither, so a call site that forgets it is slow rather than wrong.
    pub(crate) fn with(&self, change: impl FnOnce(&mut GenerationParts)) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        let same_bundle = Arc::ptr_eq(&parts.bundle, &self.parts.bundle);
        let denied = match same_bundle && Arc::ptr_eq(&parts.overlay, &self.parts.overlay) {
            true => Arc::clone(&self.denied),
            false => Arc::new(crate::compose::derive_denied(&parts.overlay, &parts.bundle)),
        };
        let buffered_rows = match same_bundle && Arc::ptr_eq(&parts.buffer, &self.parts.buffer) {
            true => Arc::clone(&self.buffered_rows),
            false => Arc::new(crate::compose::derive_buffered_rows(
                &parts.buffer,
                &parts.bundle,
            )),
        };
        Generation {
            parts,
            denied,
            buffered_rows,
        }
    }

    /// A copy whose buffer is `buffer` and whose bundle is this one's, so the buffered-row lists
    /// are this one's minus the entities whose own row has left the buffer, plus those of
    /// `inserted` that hold one and have a row. Cost is the lists and `inserted`, not the buffer.
    pub(crate) fn with_buffer(
        &self,
        buffer: Arc<IngestBuffer>,
        inserted: &[EntityId],
        change: impl FnOnce(&mut GenerationParts),
    ) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        parts.buffer = buffer;
        let denied = match Arc::ptr_eq(&parts.overlay, &self.parts.overlay) {
            true => Arc::clone(&self.denied),
            false => Arc::new(crate::compose::derive_denied(&parts.overlay, &parts.bundle)),
        };
        let buffered_rows = self.next_buffered_rows(&parts, inserted);
        Generation {
            parts,
            denied,
            buffered_rows,
        }
    }

    /// A copy whose overlay differs from this one's only by deletions and suppressions of
    /// `newly_denied`, so their rows are added to the mask without walking the whole overlay. An
    /// unsuppress may not take this route: the entity may still be deleted. `buffer` is the deleted
    /// entities' rows removed, which inserts nothing, so the buffered-row lists lose those entities
    /// and gain none.
    pub(crate) fn with_denies(
        &self,
        overlay: Arc<Overlay>,
        newly_denied: &[EntityId],
        buffer: Arc<IngestBuffer>,
        change: impl FnOnce(&mut GenerationParts),
    ) -> Generation {
        let mut parts = self.parts.clone();
        change(&mut parts);
        parts.overlay = overlay;
        parts.buffer = buffer;
        let mut denied = (*self.denied).clone();
        for partition in parts.bundle.partitions.values() {
            for (view, view_data) in &partition.views {
                let Some(rows) = denied.get_mut(view) else {
                    continue;
                };
                for entity in newly_denied {
                    if let Some(row) = view_data.row_space.row_of(*entity) {
                        rows.add(row.raw());
                    }
                }
            }
        }
        debug_assert!(
            Arc::ptr_eq(&parts.bundle, &self.parts.bundle)
                && denied == crate::compose::derive_denied(&parts.overlay, &parts.bundle),
            "the incremental deny mask does not equal a fresh derivation"
        );
        let buffered_rows = self.next_buffered_rows(&parts, &[]);
        Generation {
            parts,
            denied: Arc::new(denied),
            buffered_rows,
        }
    }

    /// The buffered-row lists for `parts`, whose bundle is this generation's and whose buffer is
    /// this one's with `inserted` added and anything else only removed.
    fn next_buffered_rows(
        &self,
        parts: &GenerationParts,
        inserted: &[EntityId],
    ) -> Arc<crate::BufferedRows> {
        // The held lists are against this generation's row spaces; against any other they are
        // derived afresh, in a release build too.
        if !Arc::ptr_eq(&parts.bundle, &self.parts.bundle) {
            return Arc::new(crate::compose::derive_buffered_rows(
                &parts.buffer,
                &parts.bundle,
            ));
        }
        let mut next = crate::BufferedRows::default();
        for partition in parts.bundle.partitions.values() {
            for (view, view_data) in &partition.views {
                let mut entities: Vec<EntityId> = match self.buffered_rows.get(view) {
                    Some(held) => held
                        .iter()
                        .copied()
                        .filter(|entity| parts.buffer.get(*entity).is_some())
                        .collect(),
                    None => Vec::new(),
                };
                entities.extend(inserted.iter().copied().filter(|entity| {
                    parts.buffer.get(*entity).is_some()
                        && view_data.row_space.row_of(*entity).is_some()
                }));
                entities.sort_unstable();
                entities.dedup();
                next.insert(view.clone(), entities);
            }
        }
        debug_assert!(
            next == crate::compose::derive_buffered_rows(&parts.buffer, &parts.bundle),
            "the incremental buffered-row lists do not equal a fresh derivation"
        );
        Arc::new(next)
    }

    /// The row-space deny mask, per view.
    pub fn denied(&self) -> &DenyMask {
        &self.denied
    }

    /// One view's buffered entities that have a row there — [`compose`](crate::compose::compose)'s
    /// walk. `None` where this generation has no list for the view, which sends the walk over the
    /// whole buffer instead.
    pub fn buffered_rows(&self, view: &str) -> Option<&[EntityId]> {
        self.buffered_rows.get(view).map(Vec::as_slice)
    }

    /// The MANIFEST digest of the prefix this generation's postings, dictionary and term index
    /// were read from — see [`Generation::fragments`], which is where it is held so that it and
    /// the cache it keys cannot disagree.
    pub(crate) fn bundle_identity(&self) -> [u8; 32] {
        self.fragments.bundle_identity()
    }

    /// What a containment partition is composed from, for this generation — the base postings and
    /// the manifest's declared plugin, taken together so the gate cannot be applied to one
    /// generation's postings on another generation's manifest
    /// (see [`crate::containment::PartitionSource`]).
    pub(crate) fn partition_source(&self) -> crate::containment::PartitionSource<'_> {
        crate::containment::PartitionSource {
            postings: &self.postings,
            data_plugin_hash: &self.bundle.manifest.data_plugin_hash,
        }
    }

    /// A generation over an empty bundle, for in-crate tests that read a few scalars off one.
    #[cfg(test)]
    pub(crate) fn synthetic(
        prefix: &str,
        segments_version: u64,
        watermark: u64,
        overlay: Overlay,
        buffer: IngestBuffer,
    ) -> Generation {
        use tessera_plugin::Plugin;
        let manifest = tessera_store::manifest::Manifest {
            bundle_format: 3,
            created_at: String::new(),
            data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            vocabularies: vec![],
            small_term_threshold: 32,
            entity_id_high_water: 0,
            identity: tessera_store::manifest::IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            groups: Vec::new(),
            views: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: std::collections::BTreeMap::new(),
        };
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let postings_path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&postings_path, &[], 32).expect("an empty postings file");
        let external_index = crate::engine::ExternalIdIndex::open(
            &manifest,
            &tessera_store::manifest::SegmentsManifest::empty(),
            std::path::Path::new("fixture-prefix-never-read"),
        )
        .expect("a manifest naming no runs opens deferred");
        Generation::new(GenerationParts {
            prefix: prefix.to_string(),
            segments_version,
            watermark,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: std::collections::HashMap::new(),
            }),
            dict: Arc::new(Dict::load(&[]).expect("an empty dict needs no file")),
            postings: Arc::new(PostingsReader::open(&postings_path, false).expect("it opens")),
            fragments: Arc::new(FragmentCache::new(
                std::path::Path::new("fixture-fragment-cache-never-written"),
                [0u8; 32],
                [0u8; 32],
            )),
            external_index: Arc::new(external_index),
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
            vocabularies: Arc::new(Vocabularies::default()),
            filter_columns: Arc::new(crate::filter::FilterColumns::default()),
            suggest: Arc::new(crate::suggest::SuggestIndexes::default()),
        })
    }
}
