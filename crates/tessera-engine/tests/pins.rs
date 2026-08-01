//! Pin identity, the drain list, and lifecycle §2.2's two bounds — over the same synthetic bundle
//! `tests/viewport.rs` uses.
//!
//! Shared fixtures live in [`common`]. (History, as a pointer: these cases were split out of
//! `tests/viewport.rs` so that the engine-state track owns them outright.)
//!
//! **Every case below a generation swap drives it through `Engine::publish_geometry`**, which is
//! the one seam a geometry publication may go through — see its doc for why it is a real API
//! rather than a test hook, and for the obligation it places on the writer.
//!
//! What is *not* reachable from here, and is unit-tested in `src/pins.rs` instead:
//! `PinManager::retire`'s refusal to drain a geometry that is still live. It needs an interleaving
//! inside `publish_geometry`'s own body, which no public API can schedule.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig, EngineError, GeometryRefusedReason, DRAIN_DEPTH_MAX};
use tessera_lifecycle::wal::{Wal, WalRecord, WalRow};
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

/// Write a WAL at `path` holding one `IngestBatch` record whose single row names `entity` and
/// carries **no descriptors**, then close it. A subsequent `Engine::open` over `path` replays it,
/// so the engine starts with `entity` in its ingest buffer with an empty term set.
///
/// **Why a hand-written WAL rather than an ingest call** — see
/// [`a_pinned_request_composes_with_the_fragment_watermark_not_the_pinned_one`], whose fixture note
/// carries the argument: no engine API can buffer a row for a *chosen* entity, and
/// replay is both the only remaining route and the faithful one. Only `tessera-lifecycle`'s ordinary
/// public WAL API is used; nothing test-only exists in the engine to make this work.
///
/// `external_id: None` on purpose (contracts §3.4 r6): the row establishes no live-map entry, so it
/// cannot shadow the bundle's own external id for the same entity and cannot perturb any other case.
fn seed_buffered_row(path: &Path, entity: u64) {
    let (mut wal, recovered) = Wal::open(path).expect("a fresh WAL opens");
    assert!(
        recovered.is_empty(),
        "seed_buffered_row expects a WAL that does not exist yet"
    );
    wal.append(&WalRecord::IngestBatch {
        batch_id: "watermark-control".to_string(),
        body_hash: [0u8; 32],
        rows: vec![WalRow {
            external_id: None,
            entity_id: EntityId::new(entity),
            descriptors: Vec::new(),
            x: 0.0,
            y: 0.0,
            scalars: Vec::new(),
        }],
    })
    .expect("the record appends");
    wal.fsync().expect("the record is made durable");
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

/// The headline property: a pin taken before a geometry swap still resolves after it, and resolves
/// to the **pinned** geometry rather than the live one.
///
/// *What each assertion catches.* The `unwrap` on the pinned request catches the absence of a drain
/// list outright — a bare equality check against the live generation `410`s here. The
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
///
/// **The executor is started explicitly**: `/control/changes` work is submitted to the
/// write executor thread, and an engine that never calls `start_write_executor` answers every
/// `accept_change` with `SubmitError::ExecutorDead` rather than applying it. That is the correct
/// posture — there is no honest 200 when there is nothing to apply the write to — so the fix is to
/// start the thread, never to relax the `expect` below.
#[test]
fn a_suppression_applies_to_a_pinned_request_immediately() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
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
    //
    // `>=`, not `==`: the guarantee this case needs is "the drain lock was taken", and a future
    // legitimate second acquisition anywhere on the pinned path would turn a test named for a
    // *security* property red for a lock-accounting reason. Exact lock accounting is
    // `resolve_takes_no_lock_when_no_pin_is_presented`'s subject, and it asserts equality there.
    assert!(
        engine.pin_stats().drain_locks > locks_before,
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
/// **The fixture is synthetic in two named ways, and it has to be.** The watermark's only
/// observable effect is whether a *buffered* entity enters `L` (`compose`'s rule 4), and that moves
/// a count only for an entity that is simultaneously buffered and **row-bearing**. The two liberties
/// are (a) generations whose declared watermark contradicts their own bundle manifest, published
/// through `Engine::publish_geometry`, and (b) that row-bearing buffered entity, reached by
/// **replaying a WAL that already holds an `IngestBatch` row naming a bundle entity**
/// ([`seed_buffered_row`]). Neither is reachable from a client; both are needed to make the rule
/// observable at all.
///
/// **(b) goes through replay, and the reason is worth reading.** Handing `Engine::accept_ingest` a
/// hand-framed `WalRow` naming an entity of the caller's choosing is not available:
/// `Command::Ingest` carries `UnallocatedRow`, which has no id field, because the executor must
/// assign a whole window's ids in one signature-sorted run — so a fresh ingest allocates at the I9
/// high-water, and a newly allocated entity has no row and therefore contributes to no count (§11.2;
/// `tests/write.rs`'s `visible` says the same). **No engine API buffers a row for a chosen entity
/// at all**, and none should be added for this test's sake: "batch into an existing entity" is an
/// open question (SA §6.6), not a settled capability, and inventing a test-only door into it would
/// prejudge the owner's call.
///
/// Replay is the remaining route and it is the *faithful* one, not a workaround. A WAL still holding
/// the ingest rows of entities a flush has since folded into the bundle — with the watermark now
/// above them — is precisely the state rule 4's watermark clause exists for (`compose`: "a buffered
/// entity below it would mean a bundle/WAL inconsistency and is excluded from `L` defensively").
/// Only `tessera-lifecycle`'s ordinary public WAL API is used; nothing in the production path was
/// widened to make this test possible.
///
/// With the two liberties:
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
/// **Why the baseline comes from a second engine.** The buffered row now arrives at *open*, via
/// replay, so there is no instant in the engine under test at which the buffer is empty and a
/// "before" count could be taken. The baseline is therefore read from a second engine over the same
/// bundle with the same credential and an **empty** WAL — its own cache directory, so it cannot seed
/// the fragment the engine under test builds. With an empty buffer the watermark is irrelevant to
/// that count, which is why the baseline engine publishes no generation.
///
/// **The vacuity trap this test has to dodge** (found by the plan review): `FragmentCache` is keyed
/// on `(bundle_identity, auth_plugin_hash, satisfied)` and **not** on the watermark, and it
/// persists to disk. Any earlier `authorise` with this credential *against this cache directory*
/// would freeze `fragment.watermark` at the *bundle's* value and silently collapse `LOW < HIGH` —
/// so this test authorises exactly once against `cache/`, after the `LOW` generation is live, reads
/// no `segments_version` through a session, and asserts the fragment's watermark before relying on
/// it. The baseline engine's `authorise` is safe only because it writes to `cache-baseline/`.
///
/// Finally, note what this can and cannot catch. `compose` takes `&FrozenFragment` and no watermark
/// argument at all (`compose.rs`), so the mistake it guards is a *future* edit that widens that
/// signature or reads `PinnedGeometry::watermark` at the call site — not anything the drain list
/// does. That is what a negative control is for; it is not coverage of the drain list.
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
    assert!((LOW..HIGH).contains(&BUFFERED_ENTITY));

    // 0. The buffer-free baseline: same bundle, same credential, empty WAL, its OWN cache dir.
    let baseline = {
        let clean = open_engine(
            &bundle_root,
            &tmp.path().join("cache-baseline"),
            &tmp.path().join("wal-baseline.log"),
        );
        let session = clean.authorise(&full_coverage_credential()).unwrap();
        clean.viewport(&session, whole_extent()).unwrap().tiles[0].visible
    };

    // 1. The engine under test replays a WAL that already buffers an entity the bundle has a row
    //    for, carrying no terms.
    let wal_path = tmp.path().join("wal.log");
    seed_buffered_row(&wal_path, BUFFERED_ENTITY);
    let engine = open_engine(&bundle_root, &tmp.path().join("cache"), &wal_path);
    // Read the base version off the bundle, NOT through a viewport — a viewport needs a session,
    // and authorising here is exactly the vacuity trap described above.
    let base_version = segments_version_of(&reopen(&bundle_root));

    // 2. A generation declaring the LOW watermark, and the ONLY `authorise` against this engine —
    //    this is what fixes `fragment.watermark`.
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

    // 3. The buffered entity is really in play, and the mask really moves for it. Without this the
    //    whole case is vacuous: a replay that dropped the row, or an entity with no row, would
    //    leave every count below equal to `baseline` and the final assertion would pass for the
    //    wrong reason.
    let live = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        live.tiles[0].visible,
        baseline - 1,
        "the replayed buffered entity ({BUFFERED_ENTITY}) is >= the fragment watermark ({LOW}), so \
         it enters L, fails on an empty term set, and its row leaves the mask"
    );

    // 4. A generation declaring the HIGH watermark; its pin is what the request presents.
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
    // 5. Supersede it, so the pin resolves off the drain list carrying HIGH.
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

/// `a_session_cannot_exceed_its_pin_cap` needs four geometries resolvable at once, so it is at the
/// mercy of the depth ceiling. Checked at **compile** time rather than assumed: lowering
/// `DRAIN_DEPTH_MAX` below 4 must fail here with a reason, not as a mysterious `PinExpired` in the
/// middle of a cap assertion.
const _: () = assert!(DRAIN_DEPTH_MAX >= 4);

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

/// A publication that changes the **bundle** while leaving `(prefix, segments_version)` alone is
/// refused, and so is a lower `segments_version`.
///
/// **This is I11's named failure reached through the API rather than through `resolve`.** A bundle
/// swap under an unchanged pin identity retires nothing, so every outstanding pin naming that
/// identity takes `resolve`'s live-equality fast path and is answered against the *new* geometry —
/// "not stale-restrictive but simply wrong", a `200` over unrelated items. The second corpus is
/// deliberately a different one (8 000 items against 10 000), so the assertion that the pin still
/// answers `before.tiles` is a statement about which geometry answered rather than an identity.
///
/// The lower-version case is the mirror: a rollback by pointer flip (design §10.2) would put a
/// *live* geometry's identity on the drain list, where a later `prune_generation` would evict the
/// live generation's own projections.
///
/// Positive control at the end, so the test cannot pass by "publication always refuses".
#[test]
fn a_bundle_swap_under_an_unchanged_pin_identity_is_refused() {
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

    let refused = engine
        .publish_geometry(
            before.pin.prefix.clone(),
            before.pin.segments_version,
            watermark_of(&reopen(&second_root)),
            reopen(&second_root),
        )
        .expect_err("a bundle swap under an unchanged pin identity must be refused");
    assert_eq!(
        refused.reason,
        GeometryRefusedReason::SegmentsVersionNotIncreasing
    );
    assert_eq!(refused.live_segments_version, before.pin.segments_version);
    assert_eq!(refused.offered_prefix, before.pin.prefix);

    assert_eq!(
        engine.pin_stats().drain_depth,
        0,
        "a refused publication retires nothing"
    );
    let after = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        after.pin, before.pin,
        "the live geometry must not have moved"
    );
    assert_eq!(
        after.tiles, before.tiles,
        "the refused bundle must not be answering requests — this is the count the accepted \
         publication would have changed"
    );
    let pinned = engine
        .viewport(&session, whole_extent().pin(Some(before.pin.clone())))
        .unwrap();
    assert_eq!(
        pinned.tiles, before.tiles,
        "the pin still names the geometry it was minted from"
    );

    // The mirror case. Raise the live version first, so offering the ORIGINAL version afterwards is
    // a rollback rather than a repeat — and needs no underflow to express.
    engine
        .publish_geometry(
            "v_next".to_string(),
            before.pin.segments_version + 1,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .expect("a strictly increasing segments_version is publishable");
    let rolled_back = engine
        .publish_geometry(
            "v_older".to_string(),
            before.pin.segments_version,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .expect_err("republishing an older segments_version must be refused");
    assert_eq!(
        rolled_back.reason,
        GeometryRefusedReason::SegmentsVersionNotIncreasing
    );
    assert_eq!(
        engine.pin_stats().drain_depth,
        1,
        "only the accepted publication in between retired anything"
    );

    // Positive control.
    engine
        .publish_geometry(
            "v_after".to_string(),
            before.pin.segments_version + 2,
            watermark_of(&reopen(&bundle_root)),
            reopen(&bundle_root),
        )
        .expect("the refusals above are about the version, not about publication");
}

/// `DRAIN_DEPTH_MAX` is a **bound**, not a gauge: the list is trimmed to it, and the trimmed pins
/// `410`.
///
/// The failure it catches is the depth trim being absent or a no-op, which is invisible in every
/// other case here — without a ceiling, retention is publication rate × `pin_ttl_secs` and the list
/// grows until the TTL happens to catch up. `pin_ttl_secs` is the shipped 300 s throughout, so
/// nothing on this list can expire by elapsing and the removal must come from the trim.
///
/// The control that stops it passing for the wrong reason: the same pin is presented **before** the
/// trimming publication and resolves. So the `410` afterwards is the trim, not "a drained pin never
/// resolves".
#[test]
fn the_drain_list_is_trimmed_to_drain_depth_max() {
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
    let oldest = engine.viewport(&session, whole_extent()).unwrap().pin;

    let publish = |step: usize| {
        engine
            .publish_geometry(
                format!("v{step:05}"),
                oldest.segments_version + step as u64,
                watermark_of(&reopen(&bundle_root)),
                reopen(&bundle_root),
            )
            .expect("each publication strictly increases segments_version")
    };

    // Fill the list exactly to the ceiling: after `DRAIN_DEPTH_MAX` publications the original
    // geometry and the first `DRAIN_DEPTH_MAX - 1` replacements are all superseded.
    for step in 1..=DRAIN_DEPTH_MAX {
        assert!(
            publish(step).is_empty(),
            "nothing is reclaimed while the list is filling"
        );
    }
    assert_eq!(engine.pin_stats().drain_depth, DRAIN_DEPTH_MAX);
    // The control: at the ceiling, the oldest pin still resolves.
    engine
        .viewport(&session, whole_extent().pin(Some(oldest.clone())))
        .expect("a pin at the ceiling, well inside its TTL, resolves");

    let trimmed = publish(DRAIN_DEPTH_MAX + 1);
    assert_eq!(
        trimmed
            .iter()
            .map(|r| r.prefix.as_str())
            .collect::<Vec<_>>(),
        vec![oldest.prefix.as_str()],
        "the trim drops the OLDEST entry, and reports it as reclaimed so the cache pruner sees it"
    );
    assert_eq!(
        engine.pin_stats().drain_depth,
        DRAIN_DEPTH_MAX,
        "the list is capped, not merely alarmed on"
    );

    let err = engine
        .viewport(&session, whole_extent().pin(Some(oldest.clone())))
        .unwrap_err();
    assert!(
        matches!(err, EngineError::PinExpired),
        "a trimmed pin must be refused, never answered against current geometry; got {err:?}"
    );
    // …and the entry that took its place is resolvable, so the trim removed one end of the list.
    engine
        .viewport(
            &session,
            whole_extent().pin(Some(PinId {
                prefix: format!("v{:05}", DRAIN_DEPTH_MAX),
                segments_version: oldest.segments_version + DRAIN_DEPTH_MAX as u64,
            })),
        )
        .expect("the newest superseded geometry is still on the list");
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
