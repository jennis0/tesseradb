//! Cache eviction, sharing and retention, driven through `Engine::viewport` over the same synthetic
//! bundle `tests/pins.rs` uses.
//!
//! **These cases go through the real request path on purpose.** The single-flight state machine,
//! the eviction rules and the lock accounting are unit-tested in `mosaica-cache`, where a
//! synthetic `V` makes every interleaving schedulable. What cannot be tested there is the thing
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

use mosaica_engine::viewport::{ViewportOut, ViewportRequest};
use mosaica_engine::{Engine, EngineConfig};
use mosaica_store::read::open_bundle;
use mosaica_store::Bundle;
use mosaica_types::ShardId;

use common::*;
use mosaica_engine::GeometryPublication;

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
    bundle.only_shard().record.watermark
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
    let live_watermark = engine.generation().only_shard().watermark;
    assert!(
        watermark_of(&second) <= live_watermark,
        "the fixture's premise: a smaller second corpus is not a successor"
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            mosaica_types::ShardId::ZERO,
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

/// Each credential's whole-extent answer, and the largest charge among their projections, taken
/// on an engine of its own over `bundle_root` so the engine under test starts with an empty cache.
/// A bound of that charge holds any one of these entries and never two.
///
/// The largest rather than any one: a projection is run-optimised, so a broad grant covering a run
/// of row space can be charged less than a sparse one scattered over it, and a bound below an
/// entry's charge serves that entry without retaining it and evicts nothing.
fn measure(tmp: &TempDir, bundle_root: &Path, credentials: &[&[u8]]) -> (u64, Vec<ViewportOut>) {
    let engine = Engine::open(
        bundle_root,
        &tmp.path().join("measure-cache"),
        &tmp.path().join("measure-wal.log"),
        config(),
    )
    .expect("a second engine opens over the fixture");
    let mut largest = 0u64;
    let mut answers = Vec::new();
    for credential in credentials {
        let before = engine.row_projection_cache_stats().bytes;
        let session = engine.authorise(credential).unwrap();
        answers.push(engine.viewport(&session, whole_extent()).unwrap());
        let charge = engine.row_projection_cache_stats().bytes - before;
        assert!(charge > 0, "each credential's grant must be its own entry");
        largest = largest.max(charge);
    }
    (largest, answers)
}

/// Two sessions with one grant read one projection, whatever else their credentials hold: the
/// second builds nothing and is served the first's answer, under an identity key of its own.
#[test]
fn sessions_with_one_grant_share_one_projection() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    // A term no item carries resolves to no index key, so both credentials make one grant.
    let first = engine.authorise(&full_coverage_credential()).unwrap();
    let second = engine
        .authorise(br#"{"terms": ["0", "carried-by-nothing"]}"#)
        .unwrap();

    let first_out = engine.viewport(&first, whole_extent()).unwrap();
    let builds = engine.full_projection_builds();
    assert_eq!(builds, 1);
    let hits = engine.row_projection_cache_stats().hits;
    let second_out = engine.viewport(&second, whole_extent()).unwrap();

    let stats = engine.row_projection_cache_stats();
    assert_eq!(stats.entries, 1, "one grant, one view, one shard: one entry");
    assert_eq!(
        engine.full_projection_builds(),
        builds,
        "the second session reads the first's projection"
    );
    assert_eq!(stats.hits, hits + 1);
    assert_eq!(second_out, first_out);
    assert_ne!(
        second_out.coordinates.identity_key, first_out.coordinates.identity_key,
        "the identity key is the session's credential's, not the grant's"
    );
}

/// Sessions with different grants never read each other's projection, and each is served its own
/// visible set.
#[test]
fn sessions_with_different_grants_never_share_a_projection() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let broad = engine.authorise(&full_coverage_credential()).unwrap();
    let sparse = engine.authorise(&subset_credential()).unwrap();
    let none = engine.authorise(&zero_credential()).unwrap();
    let broad_out = engine.viewport(&broad, whole_extent()).unwrap();
    let sparse_out = engine.viewport(&sparse, whole_extent()).unwrap();
    let none_out = engine.viewport(&none, whole_extent()).unwrap();

    assert_eq!(engine.row_projection_cache_stats().entries, 3);
    assert_eq!(engine.full_projection_builds(), 3);
    assert_eq!(broad_out.tiles[0].visible, N_ITEMS);
    assert_eq!(
        sparse_out.tiles[0].visible,
        (0..N_ITEMS).filter(|&e| subset_sees(e)).count() as u64
    );
    assert!(
        none_out.tiles.iter().all(|tile| tile.visible == 0),
        "a session holding no term sees nothing"
    );
}

/// A revoke leaves the projection its session read: another session with the grant keeps being
/// served from it, and the entry count does not move.
///
/// The *disclosure* control for a revoked session is the registry removal in `mosaica-server`;
/// the entry is the grant's, so the byte bound and the publications' retention reclaim it.
#[test]
fn revoke_leaves_the_grants_projection() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let doomed = engine.authorise(&full_coverage_credential()).unwrap();
    let survivor = engine.authorise(&full_coverage_credential()).unwrap();
    let other = engine.authorise(&subset_credential()).unwrap();
    let before = engine.viewport(&doomed, whole_extent()).unwrap();
    engine.viewport(&survivor, whole_extent()).unwrap();
    engine.viewport(&other, whole_extent()).unwrap();
    assert_eq!(engine.row_projection_cache_stats().entries, 2);

    engine.prune_tokens(&rustc_hash::FxHashSet::from_iter([doomed.token_id()]));
    assert_eq!(engine.row_projection_cache_stats().entries, 2);

    let stats = engine.row_projection_cache_stats();
    let after = engine.viewport(&survivor, whole_extent()).unwrap();
    assert_eq!(
        engine.row_projection_cache_stats().hits,
        stats.hits + 1,
        "the surviving session must still hit, not rebuild"
    );
    assert_eq!(after, before);
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
/// anything other than the grant's frozen fragment and the live row space.
#[test]
fn an_evicted_then_rebuilt_projection_is_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let (one_entry, _) = measure(
        &tmp,
        &bundle_root,
        &[&full_coverage_credential(), &subset_credential()],
    );
    let engine = open_with(config(), &tmp, &bundle_root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let other = engine.authorise(&subset_credential()).unwrap();

    // Force this session's entry out by round-robinning another principal through a bound that
    // holds the larger of the two entries and not both.
    engine.set_cache_bounds(one_entry, u64::MAX);
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
/// in either direction, but the one that matters is upward. A second session with the sparse grant
/// builds the entry the first then reads, and that entry answers as the first's own did.
#[test]
fn eviction_never_widens_a_mask() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let (one_entry, _) = measure(
        &tmp,
        &bundle_root,
        &[&subset_credential(), &full_coverage_credential()],
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let sparse = engine.authorise(&subset_credential()).unwrap();
    let sparse_twin = engine
        .authorise(br#"{"terms": ["1", "carried-by-nothing"]}"#)
        .unwrap();
    let broad = engine.authorise(&full_coverage_credential()).unwrap();

    // The bound is enforced at publish, so it is set while the cache is empty, which is what makes
    // the two reads below a build each.
    engine.set_cache_bounds(one_entry, u64::MAX);
    let broad_out = engine.viewport(&broad, whole_extent()).unwrap();
    let sparse_before = engine.viewport(&sparse, whole_extent()).unwrap();

    // Non-vacuity: the two principals must genuinely see different sets, or "did not widen" is
    // satisfied by a fixture in which every mask is the same mask.
    assert!(
        sparse_before.tiles[0].visible < broad_out.tiles[0].visible,
        "the sparse credential must see strictly fewer items than the broad one"
    );

    // Round-robin the two grants through a bound that holds one, so each grant's first read in a
    // round evicts the other's entry and is a cold rebuild.
    for _ in 0..3 {
        let twin_out = engine.viewport(&sparse_twin, whole_extent()).unwrap();
        assert_eq!(twin_out, sparse_before);
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
/// The engine-level companion to `mosaica-cache`'s unit test of the same name, which covers the
/// refusal accounting deterministically. What this adds is that a real `RowProjection` round-robin
/// under a biting bound still serves correct responses to every session.
///
/// **What this does not cover, stated rather than implied.** The dominant failure at an undersized
/// bound in a *server* is not this: every miss holds an admission permit for the whole rebuild, so
/// once most requests are misses the compute gate saturates and warm requests are shed too. That is
/// a `mosaica-server` property, not an engine one, and it is why the startup validation refuses the
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
    let credentials: [&[u8]; 4] = [
        &full_coverage_credential(),
        &subset_credential(),
        &zero_credential(),
        br#"{"terms": ["0", "1"]}"#,
    ];
    let (one_entry, references) = measure(&tmp, &bundle_root, &credentials);
    let engine = open_with(config(), &tmp, &bundle_root);

    let sessions: Vec<_> = credentials
        .iter()
        .map(|credential| engine.authorise(credential).unwrap())
        .collect();
    engine.set_cache_bounds(one_entry, u64::MAX);

    for round in 0..4 {
        for (index, session) in sessions.iter().enumerate() {
            let out = engine
                .viewport(session, whole_extent())
                .unwrap_or_else(|e| panic!("round {round} session {index} was refused: {e:?}"));
            assert_eq!(
                out.tiles, references[index].tiles,
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
/// so a read that re-opens the sidecar leaves it unchanged while a genuinely cold rebuild
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
        0,
        "authorise builds no fragment"
    );
    let built = engine
        .session_fragment_for_test(&session, ShardId::ZERO)
        .unwrap();
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "the session's first read unions postings"
    );
    assert_eq!(engine.fragment_cache_stats().entries, 1);

    // No sort: `canonical_key_for` sorts and dedups internally (see `canonical_key`'s doc — the key
    // must not depend on the caller's term order), and a sort here reads as if it did.
    let satisfied: Vec<_> = session.satisfied_for_test().iter().copied().collect();
    let key = engine
        .fragment_canonical_key(ShardId::ZERO, &satisfied)
        .expect("the bundle has shard 0");

    assert!(
        engine.evict_fragment(ShardId::ZERO, &key),
        "the entry was resident"
    );
    assert_eq!(
        engine.fragment_cache_stats().entries,
        0,
        "the in-memory tier is dropped"
    );
    assert!(
        !engine.evict_fragment(ShardId::ZERO, &key),
        "a second evict of the same key removes nothing"
    );

    // Another session of the same credential repopulates the tier from the sidecar: an entry
    // again, but NOT a rebuild.
    let again = engine.authorise(&full_coverage_credential()).unwrap();
    let reopened = engine
        .session_fragment_for_test(&again, ShardId::ZERO)
        .unwrap();
    assert_eq!(engine.fragment_cache_stats().entries, 1);
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "the .frag sidecar must survive an in-memory eviction — a re-open plus SHA-256, never a \
         re-union of postings"
    );
    assert_eq!(
        reopened.watermark, built.watermark,
        "and the reopened fragment is the one that was built, not a fresh one"
    );
}

/// **Every authorise mints its own token, and the token is the bearer secret's whole shape.**
///
/// Two sessions over one grant share a fragment and a projection — the cases above prove that —
/// and share nothing else: a token repeated across two authorises would let one viewer's bearer
/// secret name another's session, and a `token_id` repeated would hand the second session every
/// per-session cache entry the first one warmed. The lifetime is the bound `Session::is_stale`
/// leans on for a session that ignores the hint, so it is asserted here rather than assumed.
#[test]
fn every_authorise_mints_its_own_token() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(config(), &tmp, &bundle_root);

    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let before = now();
    let first = engine.authorise(&full_coverage_credential()).unwrap();
    let second = engine.authorise(&full_coverage_credential()).unwrap();
    let after = now();

    assert_ne!(
        first.token(),
        second.token(),
        "the same credential twice must not mint the same bearer secret"
    );
    assert_ne!(
        first.token_id(),
        second.token_id(),
        "nor the same per-session cache identity"
    );

    for token in [first.token(), second.token()] {
        assert_eq!(token.len(), 64, "32 random bytes, hex-encoded: {token}");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex and nothing else: {token}"
        );
    }

    let lifetime = config().token_max_lifetime_secs;
    for session in [&first, &second] {
        assert!(
            session.expires_at() >= before + lifetime && session.expires_at() <= after + lifetime,
            "a session expires exactly its configured lifetime from when it was authorised: \
             {} against {}..={}",
            session.expires_at(),
            before + lifetime,
            after + lifetime
        );
    }
}
