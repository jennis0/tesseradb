//! Cache eviction and the two pruners, driven through `Engine::viewport` over the same synthetic
//! bundle `tests/pins.rs` uses.
//!
//! **These cases go through the real request path on purpose.** The single-flight state machine,
//! the four eviction rules and the lock accounting are unit-tested in
//! `crates/tessera-engine/src/single_flight.rs`, where
//! a synthetic `V` makes every interleaving schedulable. What cannot be tested there is the thing
//! that matters most here: that a *rebuilt* projection is the same projection. A test that
//! constructed two `RowProjection`s in-process and compared them would never exercise the hit path
//! at all, and would therefore pass under a hit path that widened — which is the disclosure
//! `eviction_never_widens_a_mask` is named for. So the assertions below compare whole
//! `ViewportOut`s across an eviction, and read `Engine::row_projection_cache_stats` only to prove
//! the eviction really happened.
//!
//! Two of the cases need a **geometry swap** that the pin then outlives. Without one, "the miss
//! path built from the live generation instead of the pinned one" is a no-op mutation — live and
//! pinned name the same bundle — and the test cannot fail for the reason it claims.

mod common;

use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig, Session};
use tessera_store::read::open_bundle;
use tessera_store::Bundle;

use common::*;
use tessera_engine::GeometryPublication;

/// The whole-extent, depth-0 request every count assertion below uses: one tile, so
/// `tiles[0].visible` is the session's total visible count (θ is saturated by `common::config`).
fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5)
}

/// **The executor is started here, because publication is a submission to it** (lifecycle §1.3):
/// `Engine::publish_geometry` hands the swap to the writer thread, so an engine without one
/// answers `NoExecutor` rather than publishing. Every test in this file publishes.
fn open_with(config: EngineConfig, tmp: &TempDir, bundle_root: &Path) -> Engine {
    let mut engine = Engine::open(
        bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("engine should open against a freshly built bundle");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    engine
}

fn reopen(bundle_root: &Path) -> Arc<Bundle> {
    Arc::new(open_bundle(bundle_root).expect("the fixture bundle re-opens"))
}

fn watermark_of(bundle: &Bundle) -> u64 {
    bundle
        .partitions
        .values()
        .next()
        .expect("the fixture bundle has one partition")
        .manifest
        .watermark
}

/// Publish a second, materially different geometry over `engine`, and return its
/// `segments_version`. The corpus size differs so the swap genuinely moves counts — otherwise every
/// assertion that a pin still sees the *old* geometry is vacuous.
fn publish_second_geometry(engine: &Engine, tmp: &TempDir, prefix: &str, n: u64) -> u64 {
    let second_root = tmp.path().join(format!("bundle-{prefix}"));
    build_fixture_n(
        &second_root,
        &tmp.path().join(format!("points-{prefix}.parquet")),
        &tmp.path().join(format!("pairs-{prefix}.parquet")),
        n,
    );
    let second = reopen(&second_root);
    // From the **live** generation, not the freshly-built bundle's own (always 0): a second
    // publication must strictly increase what is live, not what it was built from.
    let next_version = engine.generation().segments_version + 1;
    // **The live watermark, not this fixture's own**, for the same reason. This bundle is a
    // *different corpus* rather than a successor to the live one — that is the whole trick that
    // makes the geometry move — so its watermark is smaller, and `check_publishable` refuses a
    // regression: composition treats every entity at or above the watermark as buffered rather
    // than rowed, so lowering it would hide the gap from every principal. What these cases need
    // is a new row space under a new `segments_version`, which is exactly what they still get.
    let live_watermark = engine.generation().watermark;
    assert!(
        watermark_of(&second) <= live_watermark,
        "the fixture's premise: a smaller second corpus is not a successor"
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            prefix.to_string(),
            next_version,
            live_watermark,
            second,
            engine.generation().dict.clone(),
            Vec::new(),
        ))
        .expect("a strictly-increasing segments_version publishes");
    next_version
}

/// Squeeze the bound to what **one** entry occupies, so any second distinct key evicts the first,
/// and leave the cache empty so the caller populates it in the order its own assertion needs.
///
/// Derived from the cache's own accounting rather than hard-coded, which keeps it independent of
/// the fixture's serialised size. Two things it has to get right, and both are silent when it does
/// not: the bound is *one* entry's charge and not the resident total, and it is the **largest**
/// charge among the principals the caller is about to round-robin. Coverage does not order the
/// charges — a projection is run-optimised, so a broad grant covering a run of row space is charged
/// less than a sparse grant scattered over the same space — so a bound taken from whichever
/// principal happened to be resident can sit below another's entry, which is then served without
/// being retained and evicts nothing. Either mistake ends at a "the eviction must have happened"
/// guard rather than at the real assertion, which is why those guards are there.
fn tighten_to_one_entry(engine: &Engine, sessions: &[&Session]) -> u64 {
    let mut largest = 0u64;
    for session in sessions {
        for resident in sessions {
            engine.prune_token(resident.token_id);
        }
        engine
            .viewport(session, whole_extent())
            .expect("a viewport populates this principal's entry");
        let stats = engine.row_projection_cache_stats();
        assert_eq!(
            stats.entries, 1,
            "one principal resident at a time, so `bytes` is that principal's own charge"
        );
        largest = largest.max(stats.bytes);
    }
    for resident in sessions {
        engine.prune_token(resident.token_id);
    }
    assert!(largest > 0, "an entry must be charged something");
    engine.set_cache_bounds(largest, u64::MAX);
    largest
}

/// A revoke drops that session's projections and nobody else's.
///
/// The *disclosure* control for a revoked session is the registry removal in `tessera-server`,
/// not this — see
/// `RowProjectionCache::prune_token`; what this asserts is that the memory is actually released,
/// and released selectively.
#[test]
fn revoke_prunes_the_token() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let doomed = engine.authorise(&full_coverage_credential()).unwrap();
    let survivor = engine.authorise(&subset_credential()).unwrap();
    engine.viewport(&doomed, whole_extent()).unwrap();
    engine.viewport(&survivor, whole_extent()).unwrap();
    assert_eq!(engine.row_projection_cache_stats().entries, 2);

    assert_eq!(
        engine.prune_token(doomed.token_id),
        1,
        "exactly the revoked session's entry"
    );
    assert_eq!(engine.row_projection_cache_stats().entries, 1);

    // The survivor is still warm — a prune that removed both would pass an entries-count check
    // that only looked at the total, so assert the survivor specifically by its hit count.
    let hits_before = engine.row_projection_cache_stats().hits;
    engine.viewport(&survivor, whole_extent()).unwrap();
    assert_eq!(
        engine.row_projection_cache_stats().hits,
        hits_before + 1,
        "the surviving session must still hit, not rebuild"
    );
}

/// **The retention depth, both sides.** One publication must *keep* the superseded generation's
/// projections, and a second must drop the first.
///
/// The keep half is the one a mutation breaks silently. A flush extends row space, so the
/// superseded entry is the input the next request's projection patch derives from — pruning at the
/// swap deletes that input before anything can use it, and every session pays the full rebuild
/// (a *measured* 1 277 ms at 10⁹) at every tick. There is no error and no wrong answer, only the
/// steady-state cost write-path §4.6 names. So the assertion is on the entry still being *there*.
///
/// This replaces a test that asserted the same coupling through a pinned request being a cache
/// hit. Pins are gone (`geometry-pinning.md`); the coupling they stood in for is not, and
/// `KEEP_SUPERSEDED_GENERATIONS` is what expresses it now.
#[test]
fn a_publication_keeps_one_superseded_generation_and_drops_the_one_before_it() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.row_projection_cache_stats().entries,
        1,
        "the first viewport populates the cache"
    );

    let second = publish_second_geometry(&engine, &tmp, "v00001", N_ITEMS / 2);
    assert_eq!(
        engine.row_projection_cache_stats().entries,
        1,
        "one publication back is retained -- it is the input to the projection patch, and \
         pruning it at the swap costs every session a full rebuild at every tick"
    );

    // A second viewport populates the live generation's key too, so the cache now holds both.
    engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(engine.row_projection_cache_stats().entries, 2);

    let third = publish_second_geometry(&engine, &tmp, "v00002", N_ITEMS / 4);
    assert!(
        third > second,
        "the versions must advance, or this is vacuous"
    );
    assert_eq!(
        engine.row_projection_cache_stats().entries,
        1,
        "and the generation two back is released -- retention is a depth, not an accumulation"
    );
}

/// A projection evicted for capacity and then rebuilt is the same projection, byte for byte.
///
/// **This used to re-read across a geometry swap through a pin**, which made "the miss path built
/// from the live generation rather than the pinned one" a mutation it could catch. With pins gone
/// there is no way to address a superseded generation at all — which is the point of deleting them
/// — so the mutation the current shape catches is narrower: a miss path that composes against
/// anything other than the session's own frozen fragment and the live row space.
#[test]
fn an_evicted_then_rebuilt_projection_is_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let other = engine.authorise(&subset_credential()).unwrap();

    // Force this session's entry out by round-robinning another principal through a bound that
    // holds the larger of the two entries and not both.
    tighten_to_one_entry(&engine, &[&session, &other]);
    let first = engine.viewport(&session, whole_extent()).unwrap();
    engine.viewport(&other, whole_extent()).unwrap();
    let evictions = engine.row_projection_cache_stats().evictions;
    assert!(
        evictions >= 1,
        "the bound must actually have evicted, or this test asserts nothing"
    );

    let misses_before = engine.row_projection_cache_stats().misses;
    let cold = engine.viewport(&session, whole_extent()).unwrap();
    assert!(
        engine.row_projection_cache_stats().misses > misses_before,
        "the re-read must be a genuine rebuild, not a hit"
    );
    assert_eq!(
        cold, first,
        "an evicted-then-rebuilt projection must reproduce the response exactly"
    );
}

/// A cache whose miss path is more permissive than its hit path is a disclosure with a performance
/// explanation (I3's cache half). A sparse principal's counts must not move across an eviction —
/// in either direction, but the one that matters is upward.
#[test]
fn eviction_never_widens_a_mask() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let sparse = engine.authorise(&subset_credential()).unwrap();
    let broad = engine.authorise(&full_coverage_credential()).unwrap();

    // **Ordering here is load-bearing.** The bound is enforced at publish, so tightening while
    // both principals are already resident would leave both there and every read below would be a
    // hit. `tighten_to_one_entry` measures each principal alone and leaves the cache empty, which
    // is what makes the two reads below a build each.
    tighten_to_one_entry(&engine, &[&sparse, &broad]);
    let broad_out = engine.viewport(&broad, whole_extent()).unwrap();
    let sparse_before = engine.viewport(&sparse, whole_extent()).unwrap();

    // Non-vacuity: the two principals must genuinely see different sets, or "did not widen" is
    // satisfied by a fixture in which every mask is the same mask.
    assert!(
        sparse_before.tiles[0].visible < broad_out.tiles[0].visible,
        "the sparse credential must see strictly fewer items than the broad one"
    );

    // Round-robin the two sessions through a bound that holds one, so each request evicts the
    // other's entry and every read below is a cold rebuild.
    for _ in 0..3 {
        let sparse_out = engine.viewport(&sparse, whole_extent()).unwrap();
        assert_eq!(
            sparse_out, sparse_before,
            "the sparse principal's masked response must be identical after a rebuild — a miss \
             path more permissive than the hit path is a disclosure, not a cache bug"
        );
        let broad_again = engine.viewport(&broad, whole_extent()).unwrap();
        assert_eq!(broad_again, broad_out);
    }
    assert!(
        engine.row_projection_cache_stats().evictions >= 3,
        "the round-robin must have evicted repeatedly"
    );
}

/// A bound below the working set costs rebuilds, never refusals, and never a wrong answer. *The
/// bound holding is not the risk; the bound biting is.*
///
/// The engine-level companion to `single_flight`'s unit test of the same name, which covers the
/// refusal accounting deterministically. What this adds is that a real `RowProjection` round-robin
/// under a biting bound still serves correct responses to every session.
///
/// **What this does not cover, stated rather than implied.** The dominant failure at an undersized
/// bound in a *server* is not this: every miss holds an admission permit for the whole rebuild, so
/// once most requests are misses the compute gate saturates and warm requests are shed too. That is
/// a `tessera-server` property, not an engine one, and it is why the startup validation refuses the
/// configuration rather than relying on the engine degrading gracefully.
#[test]
fn an_undersized_bound_does_not_livelock() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let sessions: Vec<_> = (0..4)
        .map(|_| engine.authorise(&full_coverage_credential()).unwrap())
        .collect();

    let reference = engine.viewport(&sessions[0], whole_extent()).unwrap();
    tighten_to_one_entry(&engine, &[&sessions[0]]);

    for round in 0..4 {
        for (index, session) in sessions.iter().enumerate() {
            let out = engine
                .viewport(session, whole_extent())
                .unwrap_or_else(|e| panic!("round {round} session {index} was refused: {e:?}"));
            assert_eq!(
                out.tiles, reference.tiles,
                "every session must be served correctly under a biting bound"
            );
        }
    }

    let stats = engine.row_projection_cache_stats();
    assert!(stats.evictions > 0, "the bound must have bitten");
    assert!(
        stats.bytes <= stats.bound_bytes,
        "the bound must have held throughout: {} > {}",
        stats.bytes,
        stats.bound_bytes
    );
    assert!(
        stats.young_evictions > 0,
        "a working set that does not fit must show as young evictions — the thrash alarm"
    );
}

/// `FragmentCache::evict` drops the **in-memory** tier and leaves the digest-verified `.frag`
/// sidecar alone, so the cost of an eviction is a re-open plus a SHA-256 — never a re-union of
/// postings, and never correctness.
///
/// `rebuild_count` is the observable that separates the two: it counts calls to `build_fragment`,
/// so a re-authorise that re-opens the sidecar leaves it unchanged while a genuinely cold rebuild
/// increments it. An `evict` that deleted the pair too — the plausible over-implementation — fails
/// here, and fails loudly rather than merely being slower.
#[test]
fn fragment_evict_drops_the_memory_tier_not_the_sidecar() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "the first authorise unions postings"
    );
    assert_eq!(engine.fragment_cache_stats().entries, 1);

    // No sort: `canonical_key_for` sorts and dedups internally (see `canonical_key`'s doc — the key
    // must not depend on the caller's term order), and a sort here reads as if it did.
    let satisfied: Vec<_> = session.satisfied.iter().copied().collect();
    let key = engine.fragment_canonical_key(&satisfied);

    assert!(engine.evict_fragment(&key), "the entry was resident");
    assert_eq!(
        engine.fragment_cache_stats().entries,
        0,
        "the in-memory tier is dropped"
    );
    assert!(
        !engine.evict_fragment(&key),
        "a second evict of the same key removes nothing"
    );

    // Re-authorising the same credential repopulates the tier from the sidecar: an entry again,
    // but NOT a rebuild.
    let again = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(engine.fragment_cache_stats().entries, 1);
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "the .frag sidecar must survive an in-memory eviction — a re-open plus SHA-256, never a \
         re-union of postings"
    );
    assert_eq!(
        again.fragment.watermark, session.fragment.watermark,
        "and the reopened fragment is the one that was built, not a fresh one"
    );
}
