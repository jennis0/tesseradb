//! Pin identity and pin expiry, over the same synthetic bundle `tests/viewport.rs` uses.
//!
//! Split out of `tests/viewport.rs` by Task 0c (Phase 2 stage 2.1) so that stage 2.1's engine-state
//! track owns the pin cases outright. **No test moved here was otherwise changed** — same name,
//! same assertions, same fixture values; only the binary it lives in. Shared fixtures live in
//! [`common`].

mod common;

use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::EngineError;
use tessera_types::PinId;

use common::*;

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

    let first = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
        )
        .unwrap();

    let again = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5)
                .pin(Some(first.pin.clone())),
        )
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
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5).pin(Some(stale_pin)),
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::PinExpired));
}
