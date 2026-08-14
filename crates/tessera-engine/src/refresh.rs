//! **The background refresh** — the mechanism decision 0044's D1 rules, and the whole of what
//! keeps a geometry publication off the request thread.
//!
//! ## The budget, and why nothing inline meets it
//!
//! 0044's D1, quoted from the owner: a client pays *"only a small penalty (< 0.2 ms), none, or we
//! need to ensure that longer (multisecond) delays are much rarer than the flush/merge rate"*.
//! Under continuous ingest the tick fires forever, so anything charged to a request thread per
//! publication is a **steady-state** cost. The two candidates were measured at 10⁹ over a wide
//! grant (`probes/2026-08-04-refresh-ladder/`):
//!
//! | | measured |
//! |---|---|
//! | full projection rebuild | 4 550 ms |
//! | the patch's bitmap clone alone | 40.9 ms |
//! | the union the patch then does | 0.24 ms |
//! | span rebase over a 4-extent merge | 44.6 ms |
//! | fragment rebuild per credential | ~200 ms |
//!
//! The patch is 40.9 ms because the cached value is immutable (lifecycle §7 — "invalidation is key
//! rotation, never mutation"), so a patch must **copy** before it unions; the copy is the cost and
//! no arrangement of an inline patch escapes it. Two orders over the budget. So the work moves
//! here, and the request path's face is [`crate::viewport`]'s three-rung ladder.
//!
//! ## What this pass does, and what bounds it
//!
//! One pool task per geometry publication, over **resident cache keys** — O(cache residency),
//! never O(sessions) (decision 0035's shape). At the ~16 wide-grant entries a 2 GiB bound holds,
//! one round is ~0.7 s of pool time. A session with no resident entry is not refreshed and builds
//! on its next request, which is establishment rather than update-induced work.
//!
//! **It needs no session registry, and the engine has none** — a `Session` is a value the server
//! holds. Everything a refresh needs is in the entry it is refreshing: the fragment cache's key
//! halves ride on [`crate::cache::SessionGeometry`], and the projection comes from the previous
//! entry by the same `extend` the request path used to do.
//!
//! **The fragment cache comes from the generation being refreshed, never from a handle taken at
//! startup.** A fold rotates it (`Generation::fragments`), so a pass holding the cache it was
//! constructed with would rebuild every entry's fragment under the *superseded* bundle identity —
//! every folded-away entity back in every refreshed mask, published under the new geometry's key.
//!
//! ## The two orderings that are load-bearing
//!
//! **`refresh_in_flight` is set before the swap**, by the executor, not here. A racer landing
//! between the swap and this task's first insert must see it set, or after a merge it pays the
//! measured 4 550 ms rebuild inline — which is the 429 residual's whole point (review finding F5,
//! 2026-08-04).
//!
//! **The fragment and the projection are produced together, into one entry.** Resolving them
//! separately is what stale-serve breaks: a projection built over a live fragment but published
//! under a key a stale-serving request will later read pairs two artefacts from different
//! watermarks. See [`crate::cache::SessionGeometry`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tessera_authz::FragmentCacheError;

use crate::cache::{RowProjectionCache, RowProjectionKey, SessionGeometry};
use crate::compose::RowProjection;
use crate::Generation;

/// What a resident entry may contribute to its own replacement at `generation`.
///
/// **Three answers rather than two, because a prefix change is neither "skip" nor "derive".** A
/// compaction publishes into a new prefix and rewrites `permutation.bin`, so every row-space
/// artefact in the process is invalid — but the *session* is not, and its entry still names the
/// grant, the auth hash and the slice a rebuild needs. Refusing to refresh it is the failure
/// compaction §6.2 records: the pass produces nothing, clears `refresh_in_flight`, and every
/// resident session takes an inline 4 550 ms rebuild instead of the bounded 429 the ladder's rung 3
/// exists to give — the stampede decision 0043 forbids, arriving through the mechanism written to
/// honour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carry {
    /// Nothing to do: this entry already addresses `generation`'s geometry, or a later one.
    Skip,
    /// Rebuild from the fragment alone. The entry's projection addresses another prefix's row
    /// space and none of it transfers.
    Rebuild,
    /// The ordinary within-prefix case: the previous projection may be extended or rebased.
    Derive,
}

/// [`Carry`]'s rule, as a function so it has a test — the guard this replaced was one boolean
/// expression at the head of the loop and its prefix half was silently a no-op pass.
fn carry_for(key: &RowProjectionKey, generation: &Generation) -> Carry {
    if key.segments_version >= generation.segments_version {
        // Includes the entry the live geometry already produced. `>=` rather than `==` because a
        // key from a *newer* generation means a publication raced this pass, and rebuilding
        // backwards onto older geometry is the one direction that could hand a session a mask over
        // a row space that has already been superseded.
        return Carry::Skip;
    }
    if key.prefix == generation.prefix {
        Carry::Derive
    } else {
        Carry::Rebuild
    }
}

/// Refresh every resident entry to `generation`, returning how many were produced.
///
/// **Most-recently-used first** (`SingleFlightCache::ready_entries`). This is a serial loop over a
/// rebuild that costs a *measured* 4 550 ms at 10⁹, so across a compaction it runs for minutes and
/// the keys it has not reached are shed 429; ordering it does not shorten that window, it puts the
/// tail of it on the sessions least likely to be asking.
///
/// **Every failure is per key and silent-but-counted.** A fragment build that cannot run leaves
/// that session's entry stale, and its next request takes rung 3 of the ladder — which, with the
/// pass finished and `refresh_in_flight` cleared, is a build rather than a 429. That is the
/// liveness floor under the whole mechanism: a refresh that never succeeds degrades to the
/// pre-0044 behaviour rather than wedging a session at 429 for ever.
pub(crate) fn refresh_resident(
    cache: &RowProjectionCache,
    pool: &rayon::ThreadPool,
    generation: &Generation,
) -> usize {
    let mut produced = 0usize;
    for (key, previous) in cache.resident() {
        let carry = carry_for(&key, generation);
        if carry == Carry::Skip {
            continue;
        }
        let Some(slice_data) = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.slices.get(&key.slice))
        else {
            continue;
        };

        // **Built before the cache call, because `make` must be infallible** — the single-flight
        // state machine has no way to carry a failure out of a slot, and stuffing one into the
        // value would cache it (I13a).
        let fragment = match generation.fragments.get_or_build(
            &previous.satisfied_sorted,
            previous.auth_data_hash,
            // The generation that term set was resolved against, never this one — see
            // `SessionGeometry::satisfied_at`, and `Engine::fragment_for` for the same rule at the
            // request-path call site.
            previous.satisfied_at,
            &generation.postings,
            &generation.delta_postings,
            generation.watermark,
        ) {
            Ok(fragment) => fragment,
            Err(FragmentCacheError::Building) => continue,
            Err(FragmentCacheError::Io(e)) => {
                tracing::warn!(
                    error = %e,
                    "a background fragment refresh failed; that session rebuilds on its next \
                     request rather than being served stale for ever"
                );
                continue;
            }
        };

        // **The prefix moves with the geometry.** Carrying the entry's own would publish a
        // compaction's refresh under the superseded prefix, where no request will ever look for
        // it: the pass would report work it did and every session would still rebuild inline.
        let next_key = RowProjectionKey {
            segments_version: generation.segments_version,
            prefix: generation.prefix.clone(),
            ..key.clone()
        };
        let space = &slice_data.row_space;
        let previous_projection = Arc::clone(&previous.projection);
        // **Does not wait, and this is the one caller for which that is not a policy choice**
        // (decision 0058 gives the request path the waiting entry point). This pass runs on a
        // rayon worker — `RefreshHandle::spawn` submits it to the same pool — and the build it
        // would park behind calls `pool.install`. Parking workers on work that needs workers is a
        // starvation deadlock, not merely a wasted refresh slot. It also delays the publication
        // every other session is waiting on, to duplicate a value a request thread is already
        // producing. A key found `Building` is skipped: the request that owns it will publish it.
        let built = cache.get_or_derive(next_key, None, |_| {
            // **Three rungs, cheapest first, each exact rather than approximate.**
            //
            // 1. `extends_to` — every extent the previous projection covers is still the same
            //    segment, which is what an append leaves. Union the new extents' rows only: a
            //    *measured* 0.24 ms per extent.
            // 2. `can_rebase_extents` — the base permutation is the same file, which no flush and
            //    no merge rewrites within one prefix. Keep the base's contribution, re-project
            //    every extent. This is the rung a **merge** falls to, and it is what keeps a merge
            //    publication off the 4 550 ms rebuild.
            // 3. Otherwise the whole thing.
            //
            // **Rungs 1 and 2 are gated on the prefix by [`Carry`], not by their own predicates,
            // and that is what makes a compaction safe here.** Both take a `&RowSpace`, which
            // carries no prefix, so neither can see that the base under it is a different file:
            // `extends_to` answers `true` unconditionally for a projection covering no extents,
            // and would then union a new prefix's extents onto rows projected through the old
            // prefix's `permutation.bin`. That is a mask over the wrong row space — served, not
            // refused.
            let projection = pool.install(|| {
                if carry == Carry::Derive && previous_projection.extends_to(space) {
                    previous_projection.extend(&fragment, space)
                } else if carry == Carry::Derive && previous_projection.can_rebase_extents(space) {
                    previous_projection.rebase_extents(&fragment, space)
                } else {
                    RowProjection::new(&fragment, space)
                }
            });
            SessionGeometry {
                fragment: Arc::clone(&fragment),
                projection: Arc::new(projection),
                satisfied_sorted: Arc::clone(&previous.satisfied_sorted),
                auth_data_hash: previous.auth_data_hash,
                satisfied_at: previous.satisfied_at,
            }
        });
        if built.is_ok() {
            produced += 1;
        }
    }
    produced
}

/// No refresh pass is in flight. `u64::MAX` rather than 0, because 0 is a real `segments_version`
/// — the one a freshly built bundle opens at.
pub(crate) const NO_REFRESH: u64 = u64::MAX;

/// Release the in-flight claim, but **only if this pass still holds it**.
///
/// An unconditional store is what made two overlapping passes unsafe: the first to finish cleared
/// a flag the second was relying on. The compare-exchange makes the release belong to the pass
/// that took it, so a superseded pass finishing late is a no-op rather than a hole in the shed.
fn clear_if_current(in_flight: &std::sync::atomic::AtomicU64, mine: u64) {
    let _ = in_flight.compare_exchange(mine, NO_REFRESH, Ordering::SeqCst, Ordering::SeqCst);
}

/// Everything the executor hands a refresh task. Taken on the executor thread and then immutable,
/// exactly as a flush's context is.
pub(crate) struct RefreshDeps {
    pub(crate) cache: Arc<RowProjectionCache>,
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// The `segments_version` whose refresh pass is in flight, or [`NO_REFRESH`].
    ///
    /// **⊘ A fold's publication must not arm this** (decision 0053). The rule, so a future
    /// publication kind does not have to re-litigate it: *shed only while the refresh pass is
    /// shorter than the rebuild it would save.* Flush and merge satisfy it — a ~0.7 s pass against a
    /// measured 4 550 ms rebuild, so shedding turns a 4.5 s inline build into a 1 s retry. A fold
    /// inverts it by two orders (a ~180 s pass against a 10.7 s build), so arming this would refuse
    /// every session for minutes to avoid a burst that clears in seconds — and the burst is already
    /// bounded by `ComputeGate`, by `single_flight`, and by `RowProjection::new` fanning out across
    /// the whole pool so concurrent rebuilds contend rather than multiply. After a fold, a missing
    /// projection is an ordinary cache miss.
    ///
    /// **A generation, not a boolean, and the difference only became reachable at compaction.**
    /// A flush's pass is ~0.7 s against a 90 s tick, so two never overlapped; a fold's is 76 s–3
    /// minutes against that same tick (compaction §6.2), so one or two flushes publish *inside*
    /// it, each arming the flag and each spawning its own pass. With a boolean, whichever pass
    /// finished first cleared it while the other still had un-refreshed keys — and every request
    /// for a key neither had reached fell past rung 3 to an inline `RowProjection::new`, the
    /// measured 4 550 ms, concurrently, across sessions. That is exactly the unbounded
    /// inline-rebuild herd decision 0044's D2 withdrew the pre-swap refresh to avoid, arriving
    /// through duration instead of through omission.
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) refreshes: Arc<std::sync::atomic::AtomicU64>,
    /// Whether the pass runs at all. Always `true` in a shipped build; a test disables it to model
    /// a refresh that **produces nothing and finishes** — the degraded case, where the in-flight
    /// flag clears and the ladder's rung 3 becomes a build rather than a 429.
    pub(crate) enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the pass **holds**. Always `false` in a shipped build; a test sets it to model a
    /// refresh that is merely *slow* — the in-flight flag stays set for as long as it is held,
    /// which is the window rung 3 sheds a racer in. The two hooks are different states and a test
    /// that used one for the other would assert the wrong thing.
    pub(crate) paused: Arc<std::sync::atomic::AtomicBool>,
}

impl RefreshDeps {
    /// Run one refresh round on the pool for `generation`, clearing the in-flight flag when it
    /// ends however it ends.
    ///
    /// **The caller sets the flag before the swap; this only clears it.** Setting it here would
    /// leave the window this exists to close — see the module doc.
    pub(crate) fn spawn(&self, generation: Arc<Generation>) {
        let cache = Arc::clone(&self.cache);
        let pool = Arc::clone(&self.pool);
        let in_flight = Arc::clone(&self.in_flight);
        let refreshes = Arc::clone(&self.refreshes);
        // The generation this pass is for. Taken before the early return so both exit paths
        // release only their own claim — see [`clear_if_current`].
        let mine = generation.segments_version;
        if !self.enabled.load(Ordering::SeqCst) {
            // Nothing will produce the live key, so the flag must not stay set: rung 3 of the
            // ladder would 429 for ever instead of building.
            clear_if_current(&in_flight, mine);
            return;
        }
        let spawn_on = Arc::clone(&self.pool);
        let paused = Arc::clone(&self.paused);
        spawn_on.spawn(move || {
            while paused.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let produced = refresh_resident(&cache, &pool, &generation);
            refreshes.fetch_add(produced as u64, Ordering::Relaxed);
            clear_if_current(&in_flight, mine);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

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

    /// A `Generation` over an empty synthetic bundle — [`carry_for`] reads two scalars off it, so a
    /// real one would make this a test about the fixture. Same shape as `geometry::tests`'.
    fn generation_at(prefix: &str, segments_version: u64) -> Generation {
        let manifest = Manifest {
            bundle_format: 2,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            data_plugin_hash: "builtin:passthrough:1".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            vocabularies: vec![],
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
        let (fragments, external_index) = crate::synthetic_generation_parts();
        Generation {
            // A test fixture's schema declares nothing filterable, so there is nothing to open.
            filter_columns: Arc::new(crate::filter::FilterColumns::default()),
            prefix: prefix.to_string(),
            vocabularies: Arc::new(tessera_store::vocabulary::Vocabularies::default()),
            segments_version,
            watermark: 0,
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
            denied: Arc::new(crate::DenyMask::default()),
        }
    }

    fn key_at(prefix: &str, segments_version: u64) -> RowProjectionKey {
        RowProjectionKey {
            token_id: 1,
            slice: "s0".to_string(),
            segments_version,
            prefix: prefix.to_string(),
        }
    }

    /// **A resident entry from a superseded prefix is rebuilt, never skipped.**
    ///
    /// This is the guard compaction §6.2 names: as it stood, `refresh_resident` skipped every key
    /// whose prefix differed from the generation's, so a fold's refresh would produce *nothing*,
    /// clear `refresh_in_flight`, and leave every resident session taking an inline rebuild — a
    /// *measured* 4 550 ms at 10⁹, concurrently, which is the stampede decision 0043 forbids
    /// arriving through the mechanism written to honour it.
    ///
    /// **And it is `Rebuild`, not `Derive`, which is the other half of the fix.** Neither
    /// `RowProjection::extends_to` nor `can_rebase_extents` can see a prefix — both take a bare
    /// `&RowSpace` — and `extends_to` answers `true` unconditionally for a projection covering no
    /// extents. Simply deleting the guard would therefore union a new prefix's extents onto rows
    /// projected through the old prefix's `permutation.bin`: a mask over the wrong row space,
    /// served rather than refused.
    ///
    /// **Mutations this kills:** restoring the prefix skip (leg 2 answers `Skip`); collapsing
    /// `Rebuild` into `Derive` (leg 2 answers `Derive`); relaxing the version test to `>` (leg 3
    /// answers `Rebuild`).
    #[test]
    fn a_superseded_prefix_is_rebuilt_rather_than_skipped_or_derived() {
        let generation = generation_at("v00001", 9);

        // 1. The ordinary case: same prefix, older geometry — the flush and merge path.
        assert_eq!(carry_for(&key_at("v00001", 8), &generation), Carry::Derive);

        // 2. A compaction: the entry names the superseded prefix.
        assert_eq!(carry_for(&key_at("v00000", 8), &generation), Carry::Rebuild);

        // 3. Nothing owed — the live geometry's own entry, and one from a publication that raced
        //    this pass. Rebuilding backwards onto older geometry is the direction that hands a
        //    session a mask over a row space already superseded.
        assert_eq!(carry_for(&key_at("v00001", 9), &generation), Carry::Skip);
        assert_eq!(carry_for(&key_at("v00001", 10), &generation), Carry::Skip);
        assert_eq!(carry_for(&key_at("v00000", 9), &generation), Carry::Skip);
    }
}
