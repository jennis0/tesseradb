//! The row-projection refresh: a flush must cost a live session **nothing** on its request thread.
//!
//! Every flush advances `segments_version`, which is a component of `RowProjectionKey`, so every
//! flush rotates every live session's key. Doing anything about that inline was measured at 1 277 ms
//! for a rebuild and 40.9 ms for the patch's bitmap clone alone at 10⁹
//! (`probes/2026-08-04-refresh-ladder/`), against decision 0044's stated budget of 0.2 ms. The
//! mechanism is therefore a background refresh at each publication, with a three-rung ladder in
//! front of it (`Engine::session_geometry`).
//!
//! Three things have to hold together, and each is worthless without the others:
//!
//! - **The refresh happens, and the request pays nothing.** Asserted on
//!   `Engine::full_projection_builds`, not on timing — a wall-clock assertion at fixture scale
//!   would measure noise.
//! - **What it produces is a rebuild.** Asserted on response equality against an engine that never
//!   had a source entry to derive from, so the two answers come from genuinely different routes.
//! - **The window in front of it is stale-serve, not a build.** Asserted with the refresh switched
//!   off, which is the only way to observe a window that is otherwise a race with the pool.
//!
//! A suite with only the first passes against a refresh that quietly loses rows; one with only the
//! second passes against no refresh at all; one without the third passes against a request path
//! that silently reinstates the inline rebuild.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::UnallocatedRow;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path, wal: &str, tick_secs: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join(format!("cache-{wal}")),
        &tmp.join(format!("{wal}.log")),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn ingest(engine: &Engine, external_id: &str, x: f64, y: f64) {
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x,
        y,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted");
}

fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
}

/// **The property, all of it.** After a flush the background refresh produces the session's next
/// entry, the session's next viewport pays no build at all, and what it is served is identical to
/// what a cold engine builds from scratch over the same published bundle.
///
/// The cold engine is opened on its **own** WAL, so its ingest buffer is empty and it has no
/// resident entry to refresh from: its first viewport is necessarily a full build. That is what
/// makes the equality a comparison of two routes rather than of one route with itself.
#[test]
fn a_flush_refreshes_a_sessions_geometry_and_the_refresh_equals_a_rebuild() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // The first viewport is a genuine build: this session has no resident entry, which is
    // establishment and is outside decision 0043's scope.
    engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.full_projection_builds(),
        1,
        "the first viewport of a session must build"
    );

    ingest(&engine, "ext-1", 5.0, 5.0);
    ingest(&engine, "ext-2", 500.0, 500.0);
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });
    assert_eq!(
        engine.generation().segments_version,
        1,
        "the flush must move geometry, or the key never rotates and this is vacuous"
    );
    wait_until("the background refresh to produce the new entry", || {
        engine.refreshes() >= 1
    });

    let refreshed = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.full_projection_builds(),
        1,
        "the post-flush viewport must be served from the refresh's entry — a rise here is a full \
         Permutation::project per session per tick, on the request thread, which is exactly what \
         decision 0044 rules out"
    );

    // The reference: a cold engine over the same published bundle, on its own WAL, with nothing
    // resident to refresh from.
    let cold = engine_at(tmp.path(), &root, "wal-cold", 3600);
    let cold_session = cold.authorise(&full_coverage_credential()).unwrap();
    let rebuilt = cold.viewport(&cold_session, whole_extent()).unwrap();
    assert_eq!(
        cold.full_projection_builds(),
        1,
        "the reference must be a genuine rebuild, or the comparison is between two refreshes"
    );

    assert_eq!(
        refreshed, rebuilt,
        "a refreshed projection must produce byte-identical counts and points to a rebuilt one"
    );
    assert!(
        refreshed.tiles.iter().map(|t| t.visible).sum::<u64>() > N_ITEMS,
        "and both must actually include the flushed items, or equality is satisfied by two \
         projections that both lost them"
    );
}

/// A second flush refreshes from the first flush's entry, not from the build's — so the depth-1
/// retention is sufficient for a *sequence* of flushes and not only for one.
///
/// The mutation this kills is a retention floor computed from the build generation rather than
/// from the live one: that keeps generation 0 alive for ever and drops generation 1, so the second
/// refresh has nothing to extend and rebuilds. Nothing else in the suite notices.
#[test]
fn consecutive_flushes_each_refresh_from_the_one_before() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine.viewport(&session, whole_extent()).unwrap();

    for flush in 1..=3u64 {
        ingest(&engine, &format!("ext-{flush}"), 5.0 * flush as f64, 5.0);
        wait_until("the next flush", || {
            engine.write_executor_stats().flushes >= flush
        });
        assert_eq!(engine.generation().segments_version, flush);
        wait_until("the refresh to catch up", || engine.refreshes() >= flush);

        let out = engine.viewport(&session, whole_extent()).unwrap();
        assert_eq!(
            out.tiles.iter().map(|t| t.visible).sum::<u64>(),
            N_ITEMS + flush,
            "flush {flush}: every flushed item so far is visible"
        );
        assert_eq!(
            engine.full_projection_builds(),
            1,
            "flush {flush}: still the one build from the very first viewport"
        );
    }
}

/// **The window in front of the refresh is stale-serve, and stale-serve is fail-closed** (decision
/// 0044's rung 2).
///
/// With the refresh switched off, the request that follows a flush finds no live entry and must
/// serve the one-generation-stale one *as it is*: no build, no 429, and the freshly flushed items
/// not yet drawn. That last part is the whole claim — a flush appends, so every row id the stale
/// entry holds still names the same entity and what it lacks is only rows that did not exist when
/// it was built. The session sees them one refresh later.
///
/// **Mutation:** make rung 2 rebuild instead of serving, and `full_projection_builds` rises —
/// which is the inline 1 277 ms this design exists to remove.
#[test]
fn the_window_before_a_refresh_serves_stale_geometry_rather_than_rebuilding() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    engine.set_background_refresh_for_test(false);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = engine.viewport(&session, whole_extent()).unwrap();
    let visible_before: u64 = before.tiles.iter().map(|t| t.visible).sum();

    ingest(&engine, "ext-1", 5.0, 5.0);
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });
    assert_eq!(engine.generation().segments_version, 1);

    let during = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.stale_serves(),
        1,
        "the post-flush request must take rung 2"
    );
    assert_eq!(
        engine.full_projection_builds(),
        1,
        "and must not have rebuilt — that is the inline cost decision 0044 removes"
    );
    assert_eq!(
        during.tiles.iter().map(|t| t.visible).sum::<u64>(),
        visible_before,
        "the flushed item is not yet drawn: fail-closed staleness, never a deny miss"
    );

    // **And the staleness self-heals rather than compounding.** Stale-serve inserts nothing, so a
    // session whose refresh never runs would sit one generation behind for ever — which would
    // silently falsify the ack→visibility bound. What stops it is the retention depth: at the next
    // publication the entry is two generations back, `prune_generations_below` removes it, and the
    // session's next request finds neither rung 1 nor rung 2 and builds. Fail-closed staleness is
    // bounded at two publications, never permanent.
    engine.set_background_refresh_for_test(true);
    ingest(&engine, "ext-2", 500.0, 500.0);
    wait_until("the second flush", || {
        engine.write_executor_stats().flushes >= 2
    });
    let after = wait_for_viewport(&engine, &session);
    assert_eq!(
        after.tiles.iter().map(|t| t.visible).sum::<u64>(),
        visible_before + 2,
        "both flushed items are drawn: the stale entry aged out and the session rebuilt"
    );
    assert_eq!(
        engine.full_projection_builds(),
        2,
        "and it did so by a build — the one the retention depth forces when a refresh is missed"
    );
}

/// **The content key tracks the geometry served, not the generation** (`delta-serving.md` §2).
///
/// The content key is what licenses the server to elide points a client says it already holds, and
/// eliding is exact only while the tile's visible set is unchanged. Two requests can straddle a
/// flush and still be answered over one visible set — rung 2 re-serves the very geometry the
/// earlier request built — so the key must *not* move between them, or every flush would void
/// declarations that are still perfectly sound. But once the refresh publishes, the live answer
/// covers rows neither earlier answer did, and there the key must move: otherwise a bound the
/// client declared from the stale answer would be honoured against the larger set, and the rows in
/// between are ones it never received and cannot know are missing. A hole, caused by the server,
/// which the client-side trust posture does not excuse.
///
/// **Mutation:** mint from `generation.watermark` rather than from the served fragment's, and the
/// first assertion fails — a flush would rotate the key while the answer had not changed at all.
#[test]
fn the_content_key_tracks_the_geometry_served_not_the_generation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    engine.set_background_refresh_for_test(false);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let live = engine.viewport(&session, whole_extent()).unwrap();

    ingest(&engine, "ext-1", 5.0, 5.0);
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });

    let stale = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(engine.stale_serves(), 1, "this request must take rung 2");

    // Once the refresh publishes, the answer covers rows neither earlier one did. THAT is the
    // boundary an elision must not cross: a bound declared against the smaller set, honoured
    // against the larger, drops rows the client never received and cannot know are missing.
    // Two publications, because that is the retention depth: the stale entry is rung-2-servable
    // until a second one ages it out, which is the very property the test above pins.
    engine.set_background_refresh_for_test(true);
    ingest(&engine, "ext-2", 500.0, 500.0);
    wait_until("the second flush", || {
        engine.write_executor_stats().flushes >= 2
    });
    let fresh = wait_for_viewport(&engine, &session);
    assert!(
        fresh.tiles.iter().map(|t| t.visible).sum::<u64>()
            > stale.tiles.iter().map(|t| t.visible).sum::<u64>(),
        "the refreshed answer must actually see more, or this proves nothing"
    );
    assert_ne!(
        fresh.coordinates.content_key, stale.coordinates.content_key,
        "the visible set grew, so a bound declared against the smaller one must not be honoured"
    );

    // The render partition is untouched throughout: the principal never changed, and a client that
    // dropped its held bands on a flush would be throwing away marks it may still legitimately draw.
    for out in [&live, &stale, &fresh] {
        assert_eq!(
            out.coordinates.identity_key, live.coordinates.identity_key,
            "content moved, authorisation did not"
        );
    }

    // **Idempotent while nothing writes.** Two answers over one visible set must agree, or the key
    // is rotating on something that is not content — a clock, a counter, the request itself — and
    // every declaration would lapse before it could ever be used.
    let again = wait_for_viewport(&engine, &session);
    assert_eq!(
        again.coordinates, fresh.coordinates,
        "nothing wrote between these two requests, so neither coordinate may move"
    );
}

/// **An accepted write rotates the content key, and the rotation is deliberately conservative.**
///
/// An ingest swaps the buffer and so bumps `overlay_version` before any flush lands, even though
/// buffered items have no rows and the row-space visible set is momentarily unchanged. Over-rotating
/// costs a client bytes it need not have spent; under-rotating costs it rows. The direction is the
/// point, and it is why `overlay_version` is in the key at all — not because a removal could open a
/// hole (it cannot, `delta-serving.md` §4) but because it moves the counts a client shows.
#[test]
fn an_accepted_write_rotates_the_content_key() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = engine.viewport(&session, whole_extent()).unwrap();

    ingest(&engine, "ext-1", 5.0, 5.0);

    let after = wait_for_viewport(&engine, &session);
    assert_ne!(
        before.coordinates.content_key, after.coordinates.content_key,
        "an accepted write must void every outstanding declaration"
    );
    assert_eq!(
        before.coordinates.identity_key, after.coordinates.identity_key,
        "but not the render partition"
    );
}

/// A viewport, retried past the bounded `ProjectionBuilding` a refresh window can answer with.
/// Decision 0044 permits exactly this residual, and a test that did not retry would be asserting
/// that the residual does not exist.
fn wait_for_viewport(
    engine: &Engine,
    session: &tessera_engine::Session,
) -> tessera_engine::viewport::ViewportOut {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match engine.viewport(session, whole_extent()) {
            Ok(out) => return out,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out retrying a viewport: {e}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
