//! Pin identity, the drain list, and lifecycle §2.2's two bounds — over the same synthetic bundle
//! `tests/viewport.rs` uses.
//!
//! Split out of `tests/viewport.rs` by Task 0c (Phase 2 stage 2.1) so that stage 2.1's engine-state
//! track owns the pin cases outright; Task 4 then added everything below
//! `pin_round_trips_and_rejects_a_mismatched_segments_version`. Shared fixtures live in [`common`].
//!
//! **Every test here needs a generation swap, and stage 2.1 has no production geometry publisher**
//! — flush is 2.2, compaction is 2.3. `Engine::publish_geometry` is the seam both of those will
//! publish through, and these tests are its first callers; see its doc for why it is a real API
//! rather than a test hook, and for the obligation it puts on 2.2's flush.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig, EngineError};
use tessera_lifecycle::wal::WalRow;
use tessera_lifecycle::ChangeOp;
use tessera_store::read::open_bundle;
use tessera_store::Bundle;
use tessera_types::{EntityId, PinId};

use common::*;

/// The whole-extent, depth-0 request every count assertion below uses: one tile, so `tiles[0].1`
/// is the session's total visible count (θ is saturated by `common::config`, see its doc).
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

/// A second `Arc<Bundle>` over the same bundle directory. Distinct from the engine's own — which
/// is what lets a test hold one against `Reclaimed::exclusively_held`.
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

/// A minted pin round-trips (re-presenting it succeeds and yields the same counts), and a pin
/// naming the wrong `segments_version` is rejected as expired (I11) — never silently accepted or
/// reinterpreted.
#[test]
fn pin_round_trips_and_rejects_a_mismatched_segments_version() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let first = engine.viewport(&session, whole_extent()).unwrap();

    let again = engine
        .viewport(&session, whole_extent().pin(Some(first.pin.clone())))
        .unwrap();
    assert_eq!(
        again.tiles, first.tiles,
        "a valid pin must round-trip identically"
    );

    let stale_pin = PinId {
        prefix: first.pin.prefix.clone(),
        segments_version: first.pin.segments_version + 1,
    };
    let err = engine
        .viewport(&session, whole_extent().pin(Some(stale_pin)))
        .unwrap_err();
    assert!(matches!(err, EngineError::PinExpired));
}

/// The headline of Task 4: a pin taken before a geometry swap still resolves after it, and
/// resolves to the **pinned** geometry rather than the live one.
///
/// *What each assertion catches.* The `unwrap` on the pinned request catches the pre-Task-4
/// behaviour outright — an equality check against the live generation `410`s here. The
/// `pinned.pin == before.pin` assertion catches the other plausible wrong answer, a `resolve` that
/// quietly falls back to live geometry: that returns `200` with the *new* pin, which is I11's named
/// failure ("not stale-restrictive but simply wrong"). The `live.tiles != before.tiles` assertion is
/// what stops the whole test passing vacuously: without it, an implementation that ignored the pin
/// entirely would still satisfy the count comparison, because the two geometries would be the same.
///
/// The second bundle is deliberately a *different corpus* (8 000 items against 10 000), which is
/// also why the live counts are not merely smaller but computed over a permutation that renumbered
/// its entities — exactly the compaction boundary I11 exists for.
#[test]
fn a_pin_survives_a_generation_swap() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let before = engine.viewport(&session, whole_extent()).unwrap();

    let second_root = tmp.path().join("bundle2");
    build_fixture_n(
        &second_root,
        &tmp.path().join("points2.parquet"),
        &tmp.path().join("pairs2.parquet"),
        8_000,
    );
    let second = reopen(&second_root);
    let next_version = before.pin.segments_version + 1;
    engine
        .publish_geometry(
            "v00001".to_string(),
            next_version,
            watermark_of(&second),
            second,
        )
        .unwrap();

    let live = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        live.pin.segments_version, next_version,
        "an unpinned request must see the new geometry"
    );
    assert_ne!(
        live.tiles, before.tiles,
        "the swap must actually move geometry, or every assertion below is vacuous"
    );

    let pinned = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .unwrap();
    assert_eq!(
        pinned.pin, before.pin,
        "a pinned request must be answered from the pinned geometry, never re-pinned to live"
    );
    assert_eq!(
        pinned.tiles, before.tiles,
        "the pinned geometry's counts must be unchanged by the swap"
    );
}

/// **The test that catches the fail-open.** A pin fixes geometry and never authorisation state
/// (I11, lifecycle §2.3): a suppression accepted *after* the pinned generation was superseded must
/// still apply to a request presenting that pin, the moment it is accepted.
///
/// The failure it names is the natural implementation — a drain list of `Arc<Generation>` whose
/// `resolve` hands the whole superseded generation back. That request would compose against the
/// pre-suppression overlay and the suppressed item would still be counted, so the last assertion
/// would read `before` instead of `before - 1`. `PinnedGeometry` is what makes that not compile
/// today; this test is what notices if it ever does.
#[test]
fn a_suppression_applies_to_a_pinned_request_immediately() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let before = engine.viewport(&session, whole_extent()).unwrap();
    let count_before = before.tiles[0].visible;

    // Supersede the geometry the pin names, so the pinned request below takes the drain-list path
    // rather than the live-equality one — the whole point being that the drained path is the one
    // that could reach a stale overlay.
    let republished = reopen(&bundle_root);
    engine
        .publish_geometry(
            "v00001".to_string(),
            before.pin.segments_version + 1,
            watermark_of(&republished),
            republished,
        )
        .unwrap();

    const SUPPRESSED_SOURCE_ID: u64 = 7;
    let entity = source_to_new_map(&bundle_root, &before.pin.prefix)[&SUPPRESSED_SOURCE_ID];
    engine
        .accept_change(
            source_id_key(SUPPRESSED_SOURCE_ID),
            EntityId::new(entity),
            ChangeOp::Suppress,
            None,
        )
        .expect("the suppression is accepted");

    let locks_before = engine.pin_stats().drain_locks;
    let after = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .unwrap();
    assert_eq!(
        after.pin, before.pin,
        "the pin must still resolve — a suppression does not expire a pin"
    );
    // The request really took the DRAIN-LIST path, which is where the fail-open would live —
    // without this the test could pass against a live pin and assert nothing about drained ones.
    assert_eq!(
        engine.pin_stats().drain_locks,
        locks_before + 1,
        "the pinned request must have resolved off the drain list"
    );
    assert_eq!(
        after.tiles[0].visible,
        count_before - 1,
        "the suppression must apply to the pinned request the moment it is accepted"
    );
}

/// The negative control for `PinnedGeometry::watermark`, which carries `#[allow(dead_code)]`
/// precisely because a *reader* is the thing to watch for: the effective watermark in I1
/// composition is always the mask fragment's own, and the pin vector's `W` is advisory (lifecycle
/// §2.3 and its Appendix R action 2, which amended five separate phrasings implying otherwise).
///
/// **The fixture is synthetic in two named ways, and it has to be.** Phase 1 gives buffered items
/// no rows at all, so the watermark's only observable effect — whether a buffered entity enters
/// `L` — cannot be seen through a viewport using the ordinary ingest path. The two liberties are
/// (a) generations whose declared watermark contradicts their own bundle manifest, published
/// through `Engine::publish_geometry`, and (b) an entity that is simultaneously **buffered and
/// row-bearing**, reached by handing `accept_ingest` a hand-framed `WalRow` naming an entity the
/// bundle already has. Neither is reachable from a client; both are needed to make the rule
/// observable at all. With them:
///
/// - the session's fragment is built while the live watermark is `LOW`, so `fragment.watermark` is
///   `LOW`;
/// - entity `e` (an existing bundle entity, with a row) is buffered carrying **no terms**, so it
///   takes `compose`'s ordinary rule-4 branch, its verdict is `false`, and its row is subtracted
///   from the mask *iff* it enters `L`;
/// - the pin names a generation whose watermark is `HIGH`, with `LOW <= e < HIGH`.
///
/// Composing with the fragment's `LOW` puts `e` in `L` and drops one row. Composing with the
/// pinned `HIGH` skips `e` and drops none. The assertion is the difference.
///
/// **The vacuity trap this test has to dodge** (found by the plan review): `FragmentCache` is keyed
/// on `(bundle_identity, auth_plugin_hash, satisfied)` and **not** on the watermark, and it
/// persists to disk. Any earlier `authorise` with this credential would freeze `fragment.watermark`
/// at the *bundle's* value and silently collapse `LOW < HIGH` — so this test authorises exactly
/// once, after the `LOW` generation is live, reads no `segments_version` through a session, and
/// asserts the fragment's watermark before relying on it.
///
/// Finally, note what this can and cannot catch. `compose` takes `&FrozenFragment` and no watermark
/// argument at all (`compose.rs`), so the mistake it guards is a *future* edit that widens that
/// signature or reads `PinnedGeometry::watermark` at the call site — not anything Task 4 does. That
/// is what a negative control is for; it is not coverage of the drain list.
#[test]
fn a_pinned_request_composes_with_the_fragment_watermark_not_the_pinned_one() {
    const LOW: u64 = 3_000;
    const HIGH: u64 = 9_000;
    /// Any entity in the bundle's row space works; it only has to sit in `[LOW, HIGH)`. Chosen as a
    /// literal rather than through `source_to_new_map` because the *entity id* is what the
    /// watermark comparison uses, and the fixture's ids are exactly `0..N_ITEMS`.
    const BUFFERED_ENTITY: u64 = 5_000;

    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    // Read the base version off the bundle, NOT through a viewport — a viewport needs a session,
    // and authorising here is exactly the vacuity trap described above.
    let base_version = segments_version_of(&reopen(&bundle_root));
    assert!((LOW..HIGH).contains(&BUFFERED_ENTITY));

    // 1. A generation declaring the LOW watermark, and the ONLY `authorise` in this test — this is
    //    what fixes `fragment.watermark`.
    engine
        .publish_geometry(
            "v_low".to_string(),
            base_version + 1,
            LOW,
            reopen(&bundle_root),
        )
        .unwrap();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        session.fragment.watermark, LOW,
        "the fragment must have been built against the LOW generation, or the two watermarks are \
         equal and this test asserts nothing"
    );
    let baseline = engine.viewport(&session, whole_extent()).unwrap().tiles[0].visible;

    // 2. Buffer an existing entity — one that has a row — carrying no terms.
    let entity = BUFFERED_ENTITY;
    engine
        .accept_ingest(
            vec![WalRow {
                external_id: None,
                entity_id: EntityId::new(entity),
                descriptors: Vec::new(),
                x: 0.0,
                y: 0.0,
                scalars: Vec::new(),
            }],
            vec![Vec::new()],
            "watermark-control".to_string(),
            [0u8; 32],
        )
        .expect("the synthetic buffered row is accepted");

    // 3. A generation declaring the HIGH watermark; its pin is what the request presents.
    engine
        .publish_geometry(
            "v_high".to_string(),
            base_version + 2,
            HIGH,
            reopen(&bundle_root),
        )
        .unwrap();
    let pin = PinId {
        prefix: "v_high".to_string(),
        segments_version: base_version + 2,
    };
    // 4. Supersede it, so the pin resolves off the drain list carrying HIGH.
    engine
        .publish_geometry(
            "v_low2".to_string(),
            base_version + 3,
            LOW,
            reopen(&bundle_root),
        )
        .unwrap();

    let pinned = engine
        .viewport(&session, whole_extent().pin(Some(pin.clone())))
        .unwrap();
    assert_eq!(pinned.pin, pin, "the pinned geometry must be the one used");
    assert_eq!(
        pinned.tiles[0].visible,
        baseline - 1,
        "composition must take its watermark from the mask fragment ({LOW}), which puts the \
         buffered entity in L; taking it from the pin ({HIGH}) would skip the entity and leave \
         the count at {baseline}"
    );
}

/// A pin whose drain entry has been **reclaimed** is `410`, not silently reinterpreted against
/// current geometry.
///
/// Distinct from `a_pin_past_its_ttl_is_410` in what it asserts, even though both reach the state
/// through the TTL: there the entry is still resident and the refusal must come from the
/// resolve-time bound; here the entry is gone and the refusal must come from the lookup missing.
/// `drain_depth` is asserted in both, opposite ways, so neither can pass for the other's reason.
#[test]
fn a_drained_pin_is_410() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(
        EngineConfig {
            pin_ttl_secs: 1,
            ..config()
        },
        &tmp,
        &bundle_root,
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = engine.viewport(&session, whole_extent()).unwrap();

    engine
        .publish_geometry(
            "v00001".to_string(),
            before.pin.segments_version + 1,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .unwrap();

    // Control: while it is on the drain list and inside its TTL, the pin resolves.
    engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .expect("a freshly drained pin inside its TTL still resolves");

    std::thread::sleep(Duration::from_millis(1_100));
    let reclaimed = engine.reclaim_pins();
    assert_eq!(
        reclaimed.len(),
        1,
        "the superseded geometry is the one entry to reclaim"
    );
    assert_eq!(
        engine.pin_stats().drain_depth,
        0,
        "reclaim removes; this test's refusal must come from the entry being GONE"
    );

    let err = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .unwrap_err();
    assert!(
        matches!(err, EngineError::PinExpired),
        "a reclaimed pin must be refused, never answered against current geometry; got {err:?}"
    );
}

/// Reclaim is **remove → verify → drop** (lifecycle §2.1), not verify → remove.
///
/// The distinguishing state is a drain entry whose `Arc<Bundle>` somebody else still holds. Under
/// remove-then-verify it is removed anyway and the verify merely *records* that the drop did not
/// release the mmaps. Under verify-then-remove — "if the strong count is one, remove it" — it stays
/// on the list, so the drain list grows by one per publication with nothing ever reclaiming it, and
/// the bound the list exists to enforce quietly stops existing. `drain_depth == 0` is the assertion
/// that separates them.
///
/// The two entries are also the control on each other: one is uniquely held and one is not, so
/// `exclusively_held` cannot be passing by being hardwired either way.
#[test]
fn reclaim_is_remove_then_verify() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(
        EngineConfig {
            pin_ttl_secs: 1,
            ..config()
        },
        &tmp,
        &bundle_root,
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let base_version = engine
        .viewport(&session, whole_extent())
        .unwrap()
        .pin
        .segments_version;

    // `held` stays alive in this test for the whole reclaim, so the drain entry that carries it can
    // never be uniquely owned.
    let held = reopen(&bundle_root);
    engine
        .publish_geometry("v_held".to_string(), base_version + 1, 0, Arc::clone(&held))
        .unwrap();
    engine
        .publish_geometry(
            "v_live".to_string(),
            base_version + 2,
            0,
            reopen(&bundle_root),
        )
        .unwrap();

    assert_eq!(
        engine.pin_stats().drain_depth,
        2,
        "both superseded geometries are on the list before the TTL passes"
    );

    std::thread::sleep(Duration::from_millis(1_100));
    let reclaimed = engine.reclaim_pins();

    assert_eq!(
        engine.pin_stats().drain_depth,
        0,
        "remove-then-verify removes every expired entry; verify-then-remove would leave the one \
         this test still holds a bundle Arc for"
    );
    let held_entry = reclaimed
        .iter()
        .find(|r| r.prefix == "v_held")
        .expect("the held geometry was reclaimed");
    assert!(
        !held_entry.exclusively_held,
        "the verify step must observe this test's outstanding Arc — and must run AFTER the removal"
    );
    let open_entry = reclaimed
        .iter()
        .find(|r| r.prefix != "v_held")
        .expect("the engine's original geometry was reclaimed");
    assert!(
        open_entry.exclusively_held,
        "an entry nobody else holds must verify as exclusive, or the flag is hardwired"
    );

    // The bundle this test still holds was not unmapped by the reclaim that removed its entry.
    assert!(!held.partitions.is_empty());
    engine
        .viewport(&session, whole_extent())
        .expect("serving is unaffected by the reclaim");
}

/// Lifecycle §2.2's TTL, enforced **at resolve** and not only by the reclaimer.
///
/// The failure it catches: a TTL applied only when a reclaim pass runs. A lifecycle thread that is
/// busy — which is exactly when the drain list is deepest — would then leave over-age pins
/// resolvable indefinitely, and the bound that stops one slow client holding a whole superseded
/// bundle's mmaps against the page cache would not exist. `drain_depth == 1` at the point of the
/// refusal is what proves the refusal came from the bound rather than from removal.
#[test]
fn a_pin_past_its_ttl_is_410() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(
        EngineConfig {
            pin_ttl_secs: 1,
            ..config()
        },
        &tmp,
        &bundle_root,
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = engine.viewport(&session, whole_extent()).unwrap();

    engine
        .publish_geometry(
            "v00001".to_string(),
            before.pin.segments_version + 1,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .unwrap();

    // Positive control: inside the TTL the same pin resolves, so the refusal below is the passage
    // of time and not "a drained pin never resolves".
    engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .expect("a drained pin inside its TTL resolves");

    std::thread::sleep(Duration::from_millis(1_100));
    let err = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .unwrap_err();
    assert!(
        matches!(err, EngineError::PinExpired),
        "a pin past pin_ttl_secs must be refused; got {err:?}"
    );
    assert_eq!(
        engine.pin_stats().drain_depth,
        1,
        "no reclaim pass has run — the refusal must come from the resolve-time TTL check"
    );
}

/// Lifecycle §2.2's per-session cap: the most **superseded** geometries one session may hold
/// resolvable at once.
///
/// Three failures it separates. (a) The cap not enforced at all — the third distinct pin succeeds.
/// (b) The cap consumed per *request* rather than per distinct pin — re-presenting an already-held
/// pin would then be refused, which would make the bound unusable by any client that pans twice.
/// (c) The cap counted process-wide rather than per session — the second session's pin would be
/// refused.
#[test]
fn a_session_cannot_exceed_its_pin_cap() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_with(
        EngineConfig {
            pins_per_session_max: 2,
            ..config()
        },
        &tmp,
        &bundle_root,
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let first = engine.viewport(&session, whole_extent()).unwrap().pin;

    // Four publications leave four superseded geometries on the drain list; `first` is the oldest.
    let mut pins = vec![first];
    for step in 1..=3 {
        let prefix = format!("v{step:05}");
        engine
            .publish_geometry(
                prefix.clone(),
                pins[0].segments_version + step,
                0,
                reopen(&bundle_root),
            )
            .unwrap();
        pins.push(PinId {
            prefix,
            segments_version: pins[0].segments_version + step,
        });
    }
    engine
        .publish_geometry(
            "v_live".to_string(),
            pins[0].segments_version + 4,
            0,
            reopen(&bundle_root),
        )
        .unwrap();
    assert_eq!(engine.pin_stats().drain_depth, 4);

    for pin in pins.iter().take(2) {
        engine
            .viewport(&session, whole_extent().pin(Some(pin.clone())))
            .unwrap_or_else(|e| panic!("pin {pin:?} is within the cap of 2, got {e:?}"));
    }

    let err = engine
        .viewport(&session, whole_extent().pin(Some(pins[2].clone())))
        .unwrap_err();
    assert!(
        matches!(err, EngineError::PinCapExceeded { held: 2, limit: 2 }),
        "a third distinct drained pin must be refused with the caller's own counts; got {err:?}"
    );

    // (b): a pin this session already holds costs nothing to present again.
    engine
        .viewport(&session, whole_extent().pin(Some(pins[0].clone())))
        .expect("re-presenting a held pin must not consume cap a second time");

    // (c): the cap is per session, not process-wide.
    let other = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(&other, whole_extent().pin(Some(pins[2].clone())))
        .expect("a second session has its own cap");
}

/// Rule 4: `PinManager::resolve` must not take the drain lock on the common path.
///
/// Every admitted request calls `resolve`, at the branch's 48-way admission concurrency, so a lock
/// taken unconditionally here is a process-wide serialisation point on the viewport path — the
/// exact class the concurrency workstream's F4 work removed. The drain list is deliberately
/// **non-empty** for the whole test, so an implementation that locked to "check whether there is
/// anything to check" would be caught; and the positive control at the end proves the counter is
/// wired to something rather than stuck at zero.
#[test]
fn resolve_takes_no_lock_when_no_pin_is_presented() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let drained = engine.viewport(&session, whole_extent()).unwrap().pin;

    engine
        .publish_geometry(
            "v00001".to_string(),
            drained.segments_version + 1,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .unwrap();
    let live = engine.viewport(&session, whole_extent()).unwrap().pin;
    assert_eq!(engine.pin_stats().drain_depth, 1);

    // `drain_locks` counts EVERY acquisition of the drain mutex, including the publication's own,
    // so the assertion is on the delta. `pin_stats` itself takes no lock — see `PinStats`.
    let before = engine.pin_stats().drain_locks;
    for _ in 0..20 {
        engine.viewport(&session, whole_extent()).unwrap();
        engine
            .viewport(&session, whole_extent().pin(Some(live.clone())))
            .unwrap();
    }
    assert_eq!(
        engine.pin_stats().drain_locks,
        before,
        "neither an unpinned request nor one presenting the LIVE pin may touch the drain lock"
    );

    engine
        .viewport(&session, whole_extent().pin(Some(drained)))
        .unwrap();
    assert_eq!(
        engine.pin_stats().drain_locks,
        before + 1,
        "a presented pin that fails the live equality check is the one case that locks"
    );
}
