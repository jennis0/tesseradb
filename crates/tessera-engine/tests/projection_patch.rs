//! The row-projection patch: a flush must not cost every session a full rebuild.
//!
//! Every flush advances `segments_version`, which is a component of `RowProjectionKey`, so every
//! flush rotates every live session's key. A miss is `Permutation::project` over the session's
//! whole fragment — a **measured 10.7 s at 10⁹** — and flush-and-merge §9 states the consequence:
//! *"the fallback is not an edge case but the steady state: a full 10.7 s projection per session
//! per tick, synchronised across the session population"*.
//!
//! Two things have to hold together, and each is worthless without the other:
//!
//! - **The patch happens.** Asserted on `Engine::full_projection_builds`, not on timing — a
//!   wall-clock assertion at fixture scale would measure noise.
//! - **The patch is a rebuild.** Asserted on response equality against an engine that never had
//!   the source entry to derive from, so the two answers come from genuinely different routes.
//!
//! A test with only the first passes against a patch that quietly loses rows; a test with only the
//! second passes against no patch at all.

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
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn ingest(engine: &Engine, external_id: &str, x: f32, y: f32) {
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
        .expect("ingest is accepted");
}

fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
}

/// **The property, both halves.** After a flush, the session's next viewport derives its projection
/// from the superseded generation's entry — no full build — and the response it produces is
/// identical to the one a cold engine builds from scratch over the same published bundle.
///
/// The cold engine is opened on its **own** WAL, so its ingest buffer is empty and it has no
/// superseded entry to derive from: its first viewport is necessarily a full build. That is what
/// makes the equality a comparison of two routes rather than of one route with itself.
#[test]
fn a_flush_patches_a_sessions_projection_and_the_patch_equals_a_rebuild() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = engine_at(tmp.path(), &root, "wal", 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // The first viewport is a genuine build: there is no earlier generation to derive from.
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

    let patched = engine.viewport(&session, whole_extent()).unwrap();
    assert_eq!(
        engine.full_projection_builds(),
        1,
        "the post-flush viewport must DERIVE from the superseded generation's entry -- a rise \
         here is a full Permutation::project per session per tick, which is flush §9's steady \
         state"
    );

    // The reference: a cold engine over the same published bundle, on its own WAL, with nothing to
    // derive from.
    let cold = engine_at(tmp.path(), &root, "wal-cold", 3600);
    let cold_session = cold.authorise(&full_coverage_credential()).unwrap();
    let rebuilt = cold.viewport(&cold_session, whole_extent()).unwrap();
    assert_eq!(
        cold.full_projection_builds(),
        1,
        "the reference must be a genuine rebuild, or the comparison is between two patches"
    );

    assert_eq!(
        patched, rebuilt,
        "a patched projection must produce byte-identical counts and points to a rebuilt one"
    );
    assert!(
        patched.tiles.iter().map(|t| t.visible).sum::<u64>() > N_ITEMS,
        "and both must actually include the flushed items, or equality is satisfied by two \
         projections that both lost them"
    );
}

/// A second flush derives from the first flush's generation, not from the build's — so the depth-1
/// retention is sufficient for a *sequence* of flushes and not only for one.
///
/// The mutation this kills is a retention floor computed from the build generation rather than
/// from the live one: that keeps generation 0 alive for ever and drops generation 1, so the second
/// flush's derive misses and rebuilds. Nothing else in the suite notices.
#[test]
fn consecutive_flushes_each_derive_from_the_one_before() {
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
        ingest(&engine, &format!("ext-{flush}"), 5.0 * flush as f32, 5.0);
        wait_until("the next flush", || {
            engine.write_executor_stats().flushes >= flush
        });
        assert_eq!(engine.generation().segments_version, flush);

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
