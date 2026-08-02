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

use crate::Generation;

/// Why [`check_publishable`] refused a geometry publication.
///
/// An enum rather than a `&'static str` while there is exactly one reason: a caller that wants to
/// branch (an HTTP mapping, say) can, and adding a second reason is then a compile-time obligation
/// on every match rather than a new sentence nobody notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryRefusedReason {
    /// The offered `segments_version` does not strictly exceed the live one.
    SegmentsVersionNotIncreasing,
}

impl std::fmt::Display for GeometryRefusedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeometryRefusedReason::SegmentsVersionNotIncreasing => f.write_str(
                "segments_version must strictly increase; the row-projection cache keys on it, so \
                 a bundle swap that left it unchanged would serve a projection built against the \
                 old row space to a request answered from the new one (I11)",
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
pub(crate) fn check_publishable(
    live: &Generation,
    prefix: &str,
    segments_version: u64,
) -> Result<(), GeometryRefused> {
    if segments_version > live.segments_version {
        return Ok(());
    }
    Err(GeometryRefused {
        live_prefix: live.prefix.clone(),
        live_segments_version: live.segments_version,
        offered_prefix: prefix.to_string(),
        offered_segments_version: segments_version,
        reason: GeometryRefusedReason::SegmentsVersionNotIncreasing,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    use tessera_lifecycle::{IngestBuffer, Overlay};
    use tessera_store::manifest::{IdentityDescriptor, Manifest, Quantisation};
    use tessera_store::Bundle;

    use super::*;

    fn empty_postings() -> tessera_authz::PostingsReader {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&path, &[], 32).expect("an empty postings file");
        tessera_authz::PostingsReader::open(&path, false).expect("it opens")
    }

    /// A `Generation` over an empty synthetic bundle. `check_publishable` reads two scalars off
    /// it, so an empty partition map is enough and building a real one would make this test about
    /// the fixture instead.
    fn generation_at(prefix: &str, segments_version: u64) -> Generation {
        let manifest = Manifest {
            bundle_format: 1,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            data_plugin_hash: "builtin:passthrough:1".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            small_term_threshold: 32,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            entity_id_high_water: 0,
            identity: IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            slices: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: BTreeMap::new(),
        };
        Generation {
            prefix: prefix.to_string(),
            segments_version,
            watermark: 0,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: HashMap::new(),
            }),
            dict: Arc::new(tessera_authz::Dict::load(&[]).expect("an empty dict needs no file")),
            postings: Arc::new(empty_postings()),
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(Overlay::new()),
            buffer: Arc::new(IngestBuffer::new()),
        }
    }

    #[test]
    fn a_publication_must_strictly_increase_the_segments_version() {
        let live = generation_at("p-1", 7);
        assert!(check_publishable(&live, "p-1", 8).is_ok());
        assert!(check_publishable(&live, "p-2", 8).is_ok(), "a new prefix");

        for offered in [7u64, 6, 0] {
            let refused = check_publishable(&live, "p-1", offered)
                .expect_err("a non-increasing segments_version is refused");
            assert_eq!(
                refused.reason,
                GeometryRefusedReason::SegmentsVersionNotIncreasing
            );
            assert_eq!(refused.live_segments_version, 7);
        }
    }

    /// The refusal message names the mechanism that would break, not the one that used to. A
    /// message citing pins would send a future reader to a module that no longer exists.
    #[test]
    fn the_refusal_names_the_row_projection_cache() {
        let text = GeometryRefusedReason::SegmentsVersionNotIncreasing.to_string();
        assert!(text.contains("row-projection cache"), "{text}");
    }
}
