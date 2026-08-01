//! Cache eviction and the two pruners, driven through `Engine::viewport` over the same synthetic
//! bundle `tests/pins.rs` uses (Phase 2 stage 2.1, Task 5).
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
use tessera_engine::{Engine, EngineConfig};
use tessera_store::read::open_bundle;
use tessera_store::Bundle;

use common::*;

/// The whole-extent, depth-0 request every count assertion below uses: one tile, so
/// `tiles[0].visible` is the session's total visible count (θ is saturated by `common::config`).
fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5)
}

fn open_with(config: EngineConfig, tmp: &TempDir, bundle_root: &Path) -> Engine {
    Engine::open(
        bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("engine should open against a freshly built bundle")
}

fn reopen(bundle_root: &Path) -> Arc<Bundle> {
    Arc::new(open_bundle(bundle_root).expect("the fixture bundle re-opens"))
}

fn segments_version_of(bundle: &Bundle) -> u64 {
    bundle
        .partitions
        .values()
        .next()
        .expect("the fixture bundle has one partition")
        .manifest
        .segments_version
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
fn publish_second_geometry(engine: &Engine, tmp: &TempDir) -> u64 {
    let second_root = tmp.path().join("bundle2");
    build_fixture_n(
        &second_root,
        &tmp.path().join("points2.parquet"),
        &tmp.path().join("pairs2.parquet"),
        N_ITEMS / 2,
    );
    let second = reopen(&second_root);
    let next_version = segments_version_of(&second) + 1;
    engine
        .publish_geometry(
            "v00001".to_string(),
            next_version,
            watermark_of(&second),
            second,
        )
        .expect("a strictly-increasing segments_version publishes");
    next_version
}

/// Squeeze the bound to what **one** entry occupies, so any second distinct key evicts the first.
///
/// Derived from the cache's own accounting rather than hard-coded, which keeps it independent of
/// the fixture's serialised size — but it must divide by the *entry count*, not use the resident
/// total. Setting the bound to the total is the mistake this helper was written with, and it is
/// silent: the bound is then already satisfied, nothing is ever evicted, and both tests that depend
/// on it fail at their "the eviction must have happened" guard rather than at their real assertion.
/// That guard is why they are not vacuous.
fn tighten_to_one_entry(engine: &Engine) -> u64 {
    let stats = engine.row_projection_cache_stats();
    assert!(
        stats.entries > 0,
        "the caller must populate the cache first"
    );
    let per_entry = stats.bytes / stats.entries as u64;
    assert!(per_entry > 0, "an entry must be charged something");
    engine.set_cache_bounds(per_entry, u64::MAX);
    per_entry
}

/// A revoke drops that session's projections and nobody else's.
///
/// Closes the Phase 1 deferral "revoke does not prune the projection cache". The *disclosure*
/// control for a revoked session is the registry removal in `tessera-server`, not this — see
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

/// A reclaim pass releases the projections of the geometry it reclaimed.
///
/// The TTL is zero, so `publish_geometry`'s own reclaim pass removes the superseded drain entry in
/// the same call — which is also the route that actually fires today, since nothing calls
/// `reclaim_pins` periodically.
#[test]
fn reclaim_prunes_the_generation() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(
        EngineConfig {
            pin_ttl_secs: 0,
            ..config()
        },
        &tmp,
        &bundle_root,
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.row_projection_cache_stats().entries,
        1,
        "the first viewport populates the cache"
    );

    publish_second_geometry(&engine, &tmp);

    assert_eq!(
        engine.row_projection_cache_stats().entries,
        0,
        "the reclaimed generation's projections must be released"
    );
}

/// **The wrong-coupling test.** A swap alone must NOT prune: between the swap and the reclaim the
/// superseded geometry is still resolvable from the drain list, so a pinned request is still
/// entitled to its projection — and paying a rebuild for it, measured in seconds at 10⁹, in the
/// middle of a drill-down is the cost of getting this coupling wrong.
///
/// The mutation this must fail for is **pruning `previous.segments_version` at the swap** — not
/// "calling `prune_generation` from `publish_geometry`", which is what the plan's text suggests and
/// which is *correct* behaviour: `publish_geometry` legitimately prunes over the `Reclaimed` values
/// it returns. The distinction is the whole point, so the assertion is on the pinned request being
/// a cache **hit**, which is exactly what a premature prune destroys.
#[test]
fn a_pinned_generations_projection_is_not_pruned_by_a_swap() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // The default 300 s TTL: the superseded geometry drains but is nowhere near reclaimable.
    let engine = open_with(config(), &tmp, &bundle_root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let before = engine.viewport(&session, whole_extent()).unwrap();
    let pinned_version = before.pin.segments_version;

    let next_version = publish_second_geometry(&engine, &tmp);
    assert_ne!(
        pinned_version, next_version,
        "the swap must move segments_version, or this test is vacuous"
    );

    assert_eq!(
        engine.row_projection_cache_stats().entries,
        1,
        "a swap must not prune: the drain list still resolves this geometry"
    );

    let misses_before = engine.row_projection_cache_stats().misses;
    let pinned = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .expect("the pin is still resolvable from the drain list");
    assert_eq!(
        engine.row_projection_cache_stats().misses,
        misses_before,
        "the pinned request must be served from cache — a swap-triggered prune deletes exactly \
         the key an outstanding pin is about to ask for, charging it a full rebuild"
    );
    assert_eq!(
        pinned.tiles, before.tiles,
        "and it must still see the geometry it pinned"
    );
}

/// A projection evicted for capacity and then rebuilt is the same projection, byte for byte —
/// **across a geometry swap**, so that "the miss path built from the live generation rather than
/// the pinned one" is a mutation that can actually fail this test rather than a no-op.
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

    let first = engine.viewport(&session, whole_extent()).unwrap();
    let pin = first.pin.clone();

    let next_version = publish_second_geometry(&engine, &tmp);
    let live = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(live.pin.segments_version, next_version);
    assert_ne!(
        live.tiles, first.tiles,
        "the two geometries must differ, or the pinned/live distinction is untestable"
    );

    // The pinned re-read, warm, is the reference.
    let warm = engine
        .viewport(&session, whole_extent().pin(Some(pin.clone())))
        .unwrap();
    assert_eq!(warm, first, "a warm pinned re-read is the same response");

    // Now force the pinned generation's entry out and re-read it cold.
    tighten_to_one_entry(&engine);
    let other = engine.authorise(&subset_credential()).unwrap();
    engine.viewport(&other, whole_extent()).unwrap();
    let evictions = engine.row_projection_cache_stats().evictions;
    assert!(
        evictions >= 1,
        "the bound must actually have evicted, or this test asserts nothing"
    );

    let misses_before = engine.row_projection_cache_stats().misses;
    let cold = engine
        .viewport(&session, whole_extent().pin(Some(pin)))
        .unwrap();
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

    // **Ordering here is load-bearing twice over.** The bound is enforced at publish, so tightening
    // while both principals are already resident would leave both there and every read below would
    // be a hit. And it must be sized from the *larger* of the two masks: sized from the sparse one,
    // the broad projection exceeds the whole bound and takes the oversized-admission path — served
    // but never retained — so the sparse entry is never evicted and the round-robin silently
    // becomes a sequence of hits and unretained builds. Both mistakes were made writing this test
    // and both were caught by the eviction guard at the end, which is why that guard is there.
    let broad_out = engine.viewport(&broad, whole_extent()).unwrap();
    tighten_to_one_entry(&engine);
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
    tighten_to_one_entry(&engine);

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
