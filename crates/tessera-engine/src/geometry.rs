//! What survives of geometry identity: the publication guard, and the staleness comparison.
//!
//! This module used to be `pins`, and it used to be nine hundred lines: a drain list of superseded
//! generations, a TTL, a per-session cap, a depth ceiling, a reclaim pass and the `410`/`422` a
//! client got when any of them bit. All of it existed for **one** thing — answering a later request
//! against an *earlier* generation — and `geometry-pinning.md` is the argument that nothing a
//! client holds needs that. A tile is a Morton prefix and a depth, resolvable against any
//! generation's own sorted codes; an item is a `tessera_id`, invertible to an entity independently
//! of geometry. So a re-issued request re-locates everything it needs, and the retention was paying
//! ~94 GB of mapped files (lifecycle §2.2's depth-2 sizing against a *measured* 47.02 GB bundle) for
//! a capability nothing exercised.
//!
//! # I11, and the half of it that is real
//!
//! I11 reads: *"Any cached structure expressed in row IDs carries the (segment-set version,
//! watermark) pin it was built against, and a request resolves the segment-set version once and
//! uses it throughout."*
//!
//! **The within-request claim is kept and is free.** `Engine::viewport` loads the generation
//! pointer once at request start and holds the `Arc` for the request's duration, so tile ranges,
//! columns, row space and mask all come from one generation and every file stays mapped by
//! refcount. Mixing generations within a request is I11's *"not stale-restrictive but simply
//! wrong"* — it returns a `200` over unrelated rows — and one `load_full()` is the whole of the
//! defence.
//!
//! **The cross-request claim is gone.** Nothing is retained; a stamp is advisory.
//!
//! # The one guard that survives, and why its justification changed rather than weakened
//!
//! [`check_publishable`] refuses a publication whose `segments_version` does not strictly increase.
//! It used to be justified by pins — *"outstanding pins would answer against new geometry"*. It is
//! now justified by the **row-projection cache**, which keys on `segments_version`: a non-increasing
//! version serves a row-space projection built against one geometry to a request answered from
//! another. Same mixing hazard, reached from the cache instead of from a pin, and the same refusal.
//!
//! This is also why `segments_version` — not the prefix — is the only safe discriminator for any
//! row-space artefact. A merge is row-count preserving in the sense that no *later* extent's
//! `row_base` moves, but inside the merged span it is a linear merge-sort producing globally sorted
//! output, so entities interleave and a row id there names a different entity afterwards
//! (`geometry-pinning.md` §4). A cache keyed on the prefix would survive that and serve one
//! entity's rows under another's mask.

use std::sync::Arc;

use croaring::Bitmap;

use tessera_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use tessera_store::Bundle;

use crate::session::ExternalIdIndex;
use crate::Generation;

/// One geometry publication, whole — **the single seam a generation swap may go through**, and the
/// only thing in this process that moves `segments_version`.
///
/// A value rather than an argument list because it grew past the point where a caller could get it
/// right positionally: three of its scalars are `u64` or `String`, and the widening compaction §4
/// requires adds four more artefacts. `MaintenanceDeps` made the same move for the same reason.
///
/// # What a caller supplies, and what is carried forward
///
/// `prefix`, `segments_version` and `watermark` are the new prefix's own SEGMENTS manifest values.
/// They are taken separately from `bundle` because the caller — a flush, a merge, or a fold's
/// publication — is what decides the `n` the new manifest carries. `segments_version` must strictly
/// increase; see [`GeometryRefused`] and [`check_publishable`].
///
/// `dict` and `delta_postings` are published with the geometry because a flush produces both: it
/// promotes novel descriptors to durable ordinals and publishes the assignment as a `dict_extents`
/// entry (§3.2), and it publishes one sparse delta tier per segment (§5.2). A caller with neither
/// passes the live generation's own.
///
/// **`overlay`, `buffer` and `overlay_version` have no field here, and that is load-bearing.** They
/// are carried forward from whatever generation is live at the instant of the swap, so this type
/// structurally cannot regress authorisation state — there is no parameter that could carry a stale
/// one, which is what lets it exist as a public API at all. The single exception is
/// [`PrefixRotation::retired`], which subtracts and never adds.
pub struct GeometryPublication {
    pub prefix: String,
    pub segments_version: u64,
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
    pub dict: Arc<Dict>,
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// `None` for a publication that stays within the live prefix — every flush and every merge.
    /// See [`PrefixRotation`].
    pub(crate) rotation: Option<PrefixRotation>,
}

/// The four things a publication into a **new prefix** must carry, and they travel together
/// because separating them is the fail-open.
///
/// A fold rewrites the term index, the fragment identity and the external-id runs, and retires the
/// deletions it executed. Each of those alone is wrong:
///
/// - new postings with the old fragment identity serves every session a mask built from a term
///   index that no longer exists, under a key nothing invalidates — a fold advances no watermark;
/// - a rotated identity with the old postings makes every fragment rebuild from the superseded
///   prefix's file;
/// - a new prefix with the old external-id sidecar resolves through files reclamation is about to
///   delete, and answers for keys the fold dropped;
/// - and **retirement without the identity rotation is Rule F's fail-open in its pure form**
///   (write-path §5.4): withdrawing the tombstone while a pre-fold fragment is still reachable
///   re-exposes the item the deletion hid. That is why `retired` lives *here* rather than beside
///   the rotation — a caller cannot ask for one without the other.
pub(crate) struct PrefixRotation {
    /// The new prefix's base postings.
    pub(crate) postings: Arc<PostingsReader>,
    /// The fragment cache under the new prefix's MANIFEST digest — [`FragmentCache::rotate`].
    pub(crate) fragments: Arc<FragmentCache>,
    /// The new prefix's external-id sidecar.
    pub(crate) external_index: Arc<ExternalIdIndex>,
    /// The new prefix's filter columns — **opened over it, never cloned from the live
    /// generation**, whose mappings are of the superseded prefix's files. A fold rewrites this
    /// artefact: it blanks the deleted entities' slots, folds every snapshot extent into the base
    /// and rebuilds the derived postings (`filter-index.md` §6.2), so a cloned column would serve
    /// pre-fold values out of files the reclamation is about to unlink — safe to hold on POSIX,
    /// wrong to serve. It travels with the other three for the reason they travel together: a
    /// request must never see a geometry from one publication and an artefact from another.
    pub(crate) filter_columns: Arc<crate::filter::FilterColumns>,
    /// The executed deletions leaving `deleted` in this swap — Rule F, and empty for a rotation
    /// that retires nothing. **The caller's obligation is compaction §5's rule**, restated at
    /// `tessera_lifecycle::Overlay::retire`: only entities whose row *and* postings this
    /// publication demonstrably removed, derived from what it carried forward and never from what
    /// the plan predicted.
    pub(crate) retired: Bitmap,
}

impl GeometryPublication {
    /// A publication **within the live prefix** — what a flush and a merge make. The term index,
    /// the fragment identity and the external-id sidecar all carry forward from the live
    /// generation.
    pub fn within_prefix(
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
    ) -> Self {
        GeometryPublication {
            prefix,
            segments_version,
            watermark,
            bundle,
            dict,
            delta_postings,
            rotation: None,
        }
    }

    /// Carry a [`PrefixRotation`] — what a fold's publication makes, and nothing else. Assembled
    /// by [`crate::session::Engine::publish_rotated_prefix_for_test`], which is the only producer.
    pub(crate) fn rotating(mut self, rotation: PrefixRotation) -> Self {
        self.rotation = Some(rotation);
        self
    }
}

/// Why [`check_publishable`] refused a geometry publication.
///
/// An enum rather than a `&'static str` while there is exactly one reason: a caller that wants to
/// branch (an HTTP mapping, say) can, and adding a second reason is then a compile-time obligation
/// on every match rather than a new sentence nobody notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryRefusedReason {
    /// The offered `segments_version` does not strictly exceed the live one.
    SegmentsVersionNotIncreasing,
    /// The offered `watermark` is behind the live one.
    WatermarkRegresses,
}

impl std::fmt::Display for GeometryRefusedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeometryRefusedReason::SegmentsVersionNotIncreasing => f.write_str(
                "segments_version must strictly increase; the row-projection cache keys on it, so \
                 a bundle swap that left it unchanged would serve a projection built against the \
                 old row space to a request answered from the new one (I11)",
            ),
            GeometryRefusedReason::WatermarkRegresses => f.write_str(
                "the watermark may not move backwards: composition treats every entity at or above \
                 it as buffered rather than rowed, so lowering it hides every entity between the \
                 two values from every principal until the next flush raises it again",
            ),
        }
    }
}

/// A geometry publication this engine refuses — see [`check_publishable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryRefused {
    pub live_prefix: String,
    pub live_segments_version: u64,
    pub offered_prefix: String,
    pub offered_segments_version: u64,
    pub reason: GeometryRefusedReason,
}

impl GeometryRefused {
    fn new(
        live: &Generation,
        prefix: &str,
        segments_version: u64,
        reason: GeometryRefusedReason,
    ) -> Self {
        GeometryRefused {
            live_prefix: live.prefix.clone(),
            live_segments_version: live.segments_version,
            offered_prefix: prefix.to_string(),
            offered_segments_version: segments_version,
            reason,
        }
    }
}

impl std::fmt::Display for GeometryRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to publish geometry ('{}', {}) over live ('{}', {}): {}",
            self.offered_prefix,
            self.offered_segments_version,
            self.live_prefix,
            self.live_segments_version,
            self.reason
        )
    }
}

impl std::error::Error for GeometryRefused {}

/// Whether `(prefix, segments_version)` may be published over `live` — the identity guard
/// [`crate::session::Engine::publish_geometry`] runs inside its compare-and-swap loop.
///
/// **`segments_version` must strictly increase, and this refuses rather than warns**, for the
/// reason this module's doc gives. The mirror case is equally refused: republishing an *older*
/// `segments_version` (a rollback by pointer flip, design §10.2) would make the live generation's
/// own projections indistinguishable from a superseded generation's by their cache key.
///
/// `seg_id`s are never reused across compactions or prefixes (lifecycle §5.2, contracts §2.1), so
/// strict monotonicity is what the format already promises; this only refuses to be the place it is
/// broken.
///
/// **The watermark may not regress**, and that guard is here rather than at any one caller because
/// each of the three reaches it differently. A flush's watermark is its own new, strictly greater
/// value; a merge passes the live one through; and a fold must pass the live one *untouched* —
/// compaction §4 step 2 names deriving it from the fold's inputs as the way to move it backwards
/// past every entity accepted since the fold's snapshot. Composition treats an entity at or above
/// the watermark as buffered rather than rowed, so a lowered watermark makes every entity in the
/// gap invisible to every principal: fail-closed, silent, and cleared only by the next flush.
///
/// **This guard sees only the generation.** The side-manifest a publication writes *before* the
/// swap is assembled separately, by editing a clone of the live one, and a stale value stamped
/// into the clone passes here untouched — [`check_manifest_publishable`] is the durable twin that
/// covers that seam.
pub(crate) fn check_publishable(
    live: &Generation,
    prefix: &str,
    segments_version: u64,
    watermark: u64,
) -> Result<(), GeometryRefused> {
    if segments_version <= live.segments_version {
        return Err(GeometryRefused::new(
            live,
            prefix,
            segments_version,
            GeometryRefusedReason::SegmentsVersionNotIncreasing,
        ));
    }
    if watermark < live.watermark {
        return Err(GeometryRefused::new(
            live,
            prefix,
            segments_version,
            GeometryRefusedReason::WatermarkRegresses,
        ));
    }
    Ok(())
}

/// A durable scalar a side-manifest write would move backwards — why
/// [`check_manifest_publishable`] refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManifestRegression {
    pub(crate) field: &'static str,
    pub(crate) live: u64,
    pub(crate) offered: u64,
}

impl std::fmt::Display for ManifestRegression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing a side-manifest whose `{}` ({}) is behind the live manifest's ({}): the \
             disc is what a restart believes, and a regressed value there under-reports durably \
             while the running process keeps serving the right one",
            self.field, self.offered, self.live
        )
    }
}

/// Whether `next` may be committed to disc over the state `live` — the live partition manifest at
/// the instant of publication — already records. The durable twin of [`check_publishable`], run by
/// `Executor::commit_side_manifest` at the one place a side-manifest becomes durable.
///
/// **Why the swap guard alone was not enough.** A publication writes its manifest *before* it
/// swaps, and assembles it by editing a clone of the live one — while the generation it will offer
/// [`check_publishable`] is built directly from live values. So a field the edit stamps with a
/// plan-time value regresses on disc with nothing in its way: the generation sails through the
/// swap guard, and every later publication clones the regressed manifest forward. That is not
/// hypothetical — the merge's rebase did exactly it to `watermark` when a flush shared its flight
/// (caught by the endurance tier, 2026-08-15), and what contained it was an accident: the boot
/// rebuilds the buffer by `has_row`, not by the watermark, so the under-report never miscounted.
///
/// **What is compared: every ordered scalar the manifest carries** — `watermark` and
/// `entity_id_high_water`; everything else in a `SegmentsManifest` is a list or map with
/// per-field replacement rules no total order describes. A new ordered scalar joins this
/// comparison when it is added, or it inherits the silent version of the defect above.
pub(crate) fn check_manifest_publishable(
    live: &tessera_store::manifest::SegmentsManifest,
    next: &tessera_store::manifest::SegmentsManifest,
) -> Result<(), ManifestRegression> {
    if next.watermark < live.watermark {
        return Err(ManifestRegression {
            field: "watermark",
            live: live.watermark,
            offered: next.watermark,
        });
    }
    if next.entity_id_high_water < live.entity_id_high_water {
        return Err(ManifestRegression {
            field: "entity_id_high_water",
            live: live.entity_id_high_water,
            offered: next.entity_id_high_water,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    use tessera_lifecycle::{IngestBuffer, Overlay};
    use tessera_plugin::Plugin;
    use tessera_store::manifest::{IdentityDescriptor, Manifest};
    use tessera_store::Bundle;

    use super::*;

    fn empty_postings() -> tessera_authz::PostingsReader {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&path, &[], 32).expect("an empty postings file");
        tessera_authz::PostingsReader::open(&path, false).expect("it opens")
    }

    /// A `Generation` over an empty synthetic bundle. `check_publishable` reads three scalars off
    /// it, so an empty partition map is enough and building a real one would make this test about
    /// the fixture instead.
    fn generation_at(prefix: &str, segments_version: u64, watermark: u64) -> Generation {
        let manifest = Manifest {
            bundle_format: 3,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            vocabularies: vec![],
            small_term_threshold: 32,
            entity_id_high_water: 0,
            identity: IdentityDescriptor {
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
            files: BTreeMap::new(),
        };
        let (fragments, external_index) = crate::synthetic_generation_parts();
        Generation {
            // A test fixture's schema declares nothing filterable, so there is nothing to open.
            filter_columns: Arc::new(crate::filter::FilterColumns::default()),
            // And nothing categorical, so no vocabulary has an index.
            suggest: Arc::new(crate::suggest::SuggestIndexes::default()),
            prefix: prefix.to_string(),
            vocabularies: Arc::new(tessera_store::vocabulary::Vocabularies::default()),
            segments_version,
            watermark,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: HashMap::new(),
            }),
            dict: Arc::new(tessera_authz::Dict::load(&[]).expect("an empty dict needs no file")),
            postings: Arc::new(empty_postings()),
            fragments,
            external_index,
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(Overlay::new()),
            buffer: Arc::new(IngestBuffer::new()),
            // The fixture bundle carries no views, so a fresh derivation is empty.
            denied: Arc::new(crate::DenyMask::default()),
        }
    }

    #[test]
    fn a_publication_must_strictly_increase_the_segments_version() {
        let live = generation_at("p-1", 7, 100);
        assert!(check_publishable(&live, "p-1", 8, 100).is_ok());
        assert!(
            check_publishable(&live, "p-2", 8, 100).is_ok(),
            "a new prefix"
        );

        for offered in [7u64, 6, 0] {
            let refused = check_publishable(&live, "p-1", offered, 100)
                .expect_err("a non-increasing segments_version is refused");
            assert_eq!(
                refused.reason,
                GeometryRefusedReason::SegmentsVersionNotIncreasing
            );
            assert_eq!(refused.live_segments_version, 7);
        }
    }

    /// **A publication may raise the watermark or leave it, never lower it.**
    ///
    /// A flush raises it; a merge and a fold pass the live value through untouched. The refusal
    /// exists for the third of those: compaction §4 step 2 names deriving the watermark from the
    /// *fold's inputs* as the way it moves backwards, past every entity accepted since the fold's
    /// snapshot — and composition treats an entity at or above the watermark as buffered rather
    /// than rowed, so the gap goes invisible to every principal with no error until the next flush.
    ///
    /// **Mutations this kills:** dropping the check; making it strict (a merge and a fold both pass
    /// the live value, so `>` would refuse every one of them).
    #[test]
    fn a_publication_may_not_lower_the_watermark() {
        let live = generation_at("p-1", 7, 100);

        assert!(
            check_publishable(&live, "p-1", 8, 100).is_ok(),
            "a merge or a fold passes it through"
        );
        assert!(
            check_publishable(&live, "p-1", 8, 101).is_ok(),
            "a flush raises it"
        );

        let refused = check_publishable(&live, "p-2", 8, 99)
            .expect_err("a watermark behind the live one is refused");
        assert_eq!(refused.reason, GeometryRefusedReason::WatermarkRegresses);
    }

    /// The refusal message names the mechanism that would break, not the one that used to. A
    /// message citing pins would send a future reader to a module that no longer exists.
    #[test]
    fn the_refusal_names_the_row_projection_cache() {
        let text = GeometryRefusedReason::SegmentsVersionNotIncreasing.to_string();
        assert!(text.contains("row-projection cache"), "{text}");
    }

    /// A side-manifest with only its two ordered scalars set — [`check_manifest_publishable`]
    /// reads nothing else off it.
    fn manifest_at(
        watermark: u64,
        entity_id_high_water: u64,
    ) -> tessera_store::manifest::SegmentsManifest {
        tessera_store::manifest::SegmentsManifest {
            watermark,
            entity_id_high_water,
            ..tessera_store::manifest::SegmentsManifest::empty()
        }
    }

    /// **A side-manifest may raise a durable scalar or leave it, never lower it.**
    ///
    /// Equal is the shape every clone-and-edit publication (merge, coalesce, deny, fold) commits;
    /// higher is a flush. Lower is the merge-rebase defect this guard exists for: a plan-time
    /// value stamped into a clone of a manifest a flush advanced during the flight.
    ///
    /// **Mutations this kills:** dropping either comparison; making one strict (every non-flush
    /// publication commits the live values unchanged, so `>` would refuse them all).
    #[test]
    fn a_side_manifest_may_not_regress_a_durable_scalar() {
        let live = manifest_at(100, 200);

        assert!(check_manifest_publishable(&live, &manifest_at(100, 200)).is_ok());
        assert!(check_manifest_publishable(&live, &manifest_at(101, 201)).is_ok());

        let refused = check_manifest_publishable(&live, &manifest_at(99, 200))
            .expect_err("a plan-time watermark behind the live manifest's is refused");
        assert_eq!(refused.field, "watermark");
        assert_eq!((refused.live, refused.offered), (100, 99));

        let refused = check_manifest_publishable(&live, &manifest_at(100, 199))
            .expect_err("a regressed high-water is refused");
        assert_eq!(refused.field, "entity_id_high_water");
    }
}
