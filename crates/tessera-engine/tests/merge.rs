//! The row-space merge, end to end — the half of merge that permutes row ids (write-path §7,
//! decision 0044's D2/D3).
//!
//! Four properties, and each is fail-open or fail-closed the other way round:
//!
//! - **The segment count comes down**, which is the axis the entity-space coalesce cannot bound
//!   and the whole reason this half exists.
//! - **No item is lost.** A merge is row-count preserving and drops no posting: every item stays
//!   visible, at the same coordinates, and every external id still resolves to the same entity.
//!   Dropping a row would be the compaction *fold*, which is invariant-bearing work this layer
//!   must not do.
//! - **Row space is permuted, so a projection that spans it cannot be served stale**, and the
//!   refresh that produces the replacement is armed before the swap.
//! - **A restart opens what was committed.**

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::UnallocatedRow;
use tessera_types::EntityId;

/// `MergePolicy::tier_width` — how many adjacent, same-tier extents select a merge.
const TIER_WIDTH: usize = 4;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn ingest(engine: &Engine, external_id: &str, x: f32, y: f32) -> EntityId {
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x,
        y,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
}

/// A viewport, retried past the bounded `ProjectionBuilding` a merge's refresh window answers
/// with. Decision 0044 permits exactly that residual — stale-serve is unsound across a merge —
/// and a test that did not retry would be asserting the residual does not exist.
fn viewport(engine: &Engine, session: &tessera_engine::Session) -> tessera_engine::viewport::ViewportOut {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match engine.viewport(session, whole_extent()) {
            Ok(out) => return out,
            Err(e) => {
                assert!(Instant::now() < deadline, "timed out retrying a viewport: {e}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Flush `count` segments, one item each, and return their `(entity, external_id, x)` triples.
fn flush_segments(engine: &Engine, count: usize) -> Vec<(EntityId, String, f32)> {
    let mut items = Vec::new();
    for i in 0..count {
        let external_id = format!("ext-{i}");
        let x = 5.0 * (i as f32 + 1.0);
        let entity = ingest(engine, &external_id, x, 5.0);
        items.push((entity, external_id, x));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    }
    items
}

fn segment_count(engine: &Engine) -> usize {
    engine.generation().bundle.partitions["default"].slices["s0"]
        .segments
        .len()
}

/// **The segment count comes down and nothing is lost doing it.**
///
/// Without this the tile path pays one binary search and one `range_cardinality` per live segment
/// per tile, and a 90 s tick reaches ~960 segments in a day — the axis the entity-space coalesce
/// leaves untouched by construction.
///
/// **Mutation:** make `execute_merge` drop a row (the compaction fold, arriving as an
/// optimisation) and the visible count falls short.
#[test]
fn a_merge_collapses_segments_and_loses_no_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);
    engine.set_merge_for_test(false);

    let items = flush_segments(&engine, TIER_WIDTH);
    // The build segment plus one per flush.
    assert_eq!(segment_count(&engine), TIER_WIDTH + 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = viewport(&engine, &session);
    let visible_before: u64 = before.tiles.iter().map(|t| t.visible).sum();
    assert_eq!(visible_before, 64 + TIER_WIDTH as u64);

    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    assert_eq!(
        segment_count(&engine),
        2,
        "the build segment plus the one merged segment"
    );
    let after = viewport(&engine, &session);
    assert_eq!(
        after.tiles.iter().map(|t| t.visible).sum::<u64>(),
        visible_before,
        "a merge is row-count preserving: every item is still visible"
    );
    // Everything but the stamp, which names the new geometry version by construction.
    assert_eq!(
        after.tiles, before.tiles,
        "every tile's counts are unchanged — a merge moves rows, never items"
    );
    assert_eq!(
        after.points, before.points,
        "and every point keeps its identity and its exact code — the merge is byte-exact through \
         the Morton code, never through a dequantise-and-requantise"
    );

    // Every binding survives the run coalesce the merge performed on the way.
    for (entity, external_id, _) in &items {
        assert_eq!(
            engine
                .resolve_external_id(external_id.as_bytes())
                .expect("resolvable"),
            Some(*entity),
            "external id {external_id} lost its binding to the merge"
        );
        assert_eq!(
            engine.external_id_of(*entity).expect("no inconsistency"),
            Some(external_id.as_bytes().to_vec()),
            "and the reverse direction still answers for it"
        );
    }
}

/// **A merge bumps `segments_version` and arms the refresh before the swap.**
///
/// Row ids inside the merged span name different entities afterwards (I11), so no cached
/// projection covering the span may be served: `extends_to` refuses, and rung 3 of the ladder is
/// what a racer meets. The refresh replaces the entry with an extents-only re-projection rather
/// than the *measured* 4 550 ms rebuild, which is what keeps the residual bounded.
///
/// **Mutation:** carry the deny mask forward instead of re-deriving it and a suppressed row keeps
/// its old id — which after a permutation names a different entity.
#[test]
fn a_merge_moves_geometry_and_the_refresh_replaces_every_projection() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);
    engine.set_merge_for_test(false);
    flush_segments(&engine, TIER_WIDTH);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    viewport(&engine, &session);
    let geometry_before = engine.generation().segments_version;
    let builds_before = engine.full_projection_builds();

    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });
    assert!(
        engine.generation().segments_version > geometry_before,
        "a merge permutes row space, so it must bump the geometry version — the only safe \
         discriminator a row-space artefact may key on"
    );
    wait_until("the refresh to replace the entry", || {
        engine.refreshes() >= 1
    });

    // Served from the refresh's entry, not rebuilt on this thread.
    viewport(&engine, &session);
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "the post-merge viewport must be served from the refresh's extents-only re-projection, \
         never from a full rebuild on the request thread"
    );
}

/// **A restart opens what the merge committed**, with the consumed segments gone from the manifest
/// and every item still visible.
///
/// The delta tiers of the consumed segments stay listed — their entities still have rows, in the
/// merged segment — so this also pins the rule most easily got wrong: a merge that dropped a
/// consumed segment's tier would make every item that tier carries invisible to every session,
/// silently.
#[test]
fn a_merged_manifest_reopens_with_every_item_and_every_tier() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );

    let items = {
        let engine = engine_at(tmp.path(), &root);
        let items = flush_segments(&engine, TIER_WIDTH);
        // The merge is selected on the tick, and the last flush's own tick ran before that flush
        // published — so one more tick is what dispatches it.
        engine.request_flush();
        wait_until("the merge to publish", || {
            engine.write_executor_stats().merges >= 1
        });
        items
    };

    let reopened = engine_at(tmp.path(), &root);
    let generation = reopened.generation();
    assert_eq!(
        generation.bundle.partitions["default"].slices["s0"]
            .segments
            .len(),
        2,
        "the reopened bundle holds the build segment and the merged one"
    );
    assert_eq!(
        generation.delta_postings.len(),
        TIER_WIDTH,
        "every consumed segment's tier is still listed — its entities have rows in the merged \
         segment, and dropping one would make them invisible"
    );

    let session = reopened.authorise(&full_coverage_credential()).unwrap();
    let out = viewport(&reopened, &session);
    assert_eq!(
        out.tiles.iter().map(|t| t.visible).sum::<u64>(),
        64 + TIER_WIDTH as u64,
        "every item survives the merge and the restart"
    );
    for (entity, external_id, _) in &items {
        assert!(
            generation.bundle.partitions["default"].slices["s0"]
                .row_space
                .row_of(*entity)
                .is_some(),
            "entity {} lost its row across the merge and the restart",
            entity.raw()
        );
        assert_eq!(
            reopened
                .resolve_external_id(external_id.as_bytes())
                .expect("resolvable"),
            Some(*entity)
        );
    }
}

/// **A racer inside a merge's refresh window is shed, not made to pay the rebuild** — decision
/// 0044's bounded 429 residual, and the one place the design accepts a refusal.
///
/// Stale-serve is unsound across a merge: the stale entry's bits inside the merged span name
/// different entities now. So rung 2 refuses, and rung 3's choice is the whole of what 0044
/// bought — shed for the refresh's bounded duration, or pay the *measured* 4 550 ms rebuild on
/// the request thread. The refresh is **held** here rather than switched off, because those are
/// different states: a refresh that finishes without producing anything clears the flag and rung 3
/// builds, which is the liveness floor, not this.
///
/// **Mutation:** clear `refresh_in_flight` before the swap instead of setting it, and the racer
/// takes the rebuild silently.
#[test]
fn a_racer_inside_a_merges_refresh_window_is_shed_rather_than_rebuilding() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), &root);
    engine.set_merge_for_test(false);
    flush_segments(&engine, TIER_WIDTH);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    viewport(&engine, &session);
    let builds_before = engine.full_projection_builds();

    // Hold the refresh, then let the merge publish: the window stays open until we release it.
    engine.set_refresh_paused_for_test(true);
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    let refused = engine
        .viewport(&session, whole_extent())
        .expect_err("a racer inside the refresh window must be shed");
    assert!(
        matches!(refused, tessera_engine::EngineError::ProjectionBuilding),
        "and shed as backpressure with a Retry-After, never as a failure: {refused}"
    );
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "the racer must not have rebuilt — that is the 4 550 ms this residual exists to avoid"
    );

    // Released, the window closes and the same request is served.
    engine.set_refresh_paused_for_test(false);
    wait_until("the refresh to land", || engine.refreshes() >= 1);
    let served = viewport(&engine, &session);
    assert_eq!(
        served.tiles.iter().map(|t| t.visible).sum::<u64>(),
        64 + TIER_WIDTH as u64,
        "every item is visible once the refresh has replaced the entry"
    );
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "and the replacement was an extents-only re-projection, not a rebuild"
    );
}
