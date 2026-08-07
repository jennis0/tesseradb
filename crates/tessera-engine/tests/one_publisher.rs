//! One publisher (§1.1, lifecycle §1.3). Closes #59.
//!
//! A generation swap happens on the write executor thread and nowhere else. That was a discipline
//! with one recorded exception until flush needed a second publisher and made the arrangement
//! untenable: `Engine::publish_geometry` swapped the pointer under a compare-and-swap, which is
//! safe against another caller of itself and not against the executor's own unconditional `store`
//! — and a lost publication leaves the **live** generation on the pin drain list, where a later
//! prune evicts projections still in use.

use tessera_engine::PublishGeometryError;

/// The rule `scripts/check-layers.sh` enforces, asserted from inside the crate too, because a grep
/// is one `#[allow]` — or one deleted line of shell — away from being argued with.
///
/// The marker it looks for was the single permitted exemption, and the shell rule *counted* it so
/// that a second one failed. Both are gone: there is no publisher outside `write.rs` to exempt.
#[test]
fn the_engine_has_no_geometry_publisher_outside_the_executor() {
    let src = std::fs::read_to_string("src/session.rs").unwrap();
    assert!(
        !src.contains("PUBLISHER-EXEMPT"),
        "publication moved onto the executor; the exemption must go with it"
    );
    assert!(
        !src.contains("compare_and_swap"),
        "session.rs must not publish a generation at all — submit ExecutorWork::PublishGeometry"
    );
}

/// **An engine with no write executor cannot publish, and says so.** Not a `GeometryRefused`,
/// which is a statement about the geometry offered; the geometry may be perfectly publishable and
/// there is simply no thread to publish it on.
///
/// This is the observable that makes "publication is a submission" true rather than asserted: a
/// `publish_geometry` that still swapped inline would answer `Ok` here.
#[test]
fn an_engine_without_a_write_executor_cannot_publish() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    common::build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // `open_engine`, not `open_engine_publishing`: the whole point is that no executor is running.
    let engine = common::open_engine(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let live = engine.generation();

    let err = engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            std::sync::Arc::clone(&live.bundle),
            std::sync::Arc::clone(&live.dict),
            Vec::new(),
        ))
        .expect_err("there is no publisher thread");
    assert_eq!(err, PublishGeometryError::NoExecutor);
    assert_eq!(
        engine.generation().segments_version,
        live.segments_version,
        "and nothing moved"
    );
}

mod common;
use tessera_engine::GeometryPublication;
