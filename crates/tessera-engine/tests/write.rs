//! Track B's engine-level test file (Phase 2 stage 2.1) — the acceptance path, asserted against
//! `Engine` directly rather than through HTTP.
//!
//! **Why this file exists.** Task 3a moves the WAL behind a single writer thread, which changes
//! the shape of the acceptance API: entity IDs stop being supplied by the caller and start being
//! assigned by the executor (that is the point of the change — Task 7a then assigns a whole
//! window's IDs in one signature-sorted run). Every existing caller therefore has to move, and
//! the case below lived in `tests/viewport.rs`, which Task 0c froze for **every** track because
//! its residue spans several of them. Track B would have had nowhere to put the migration, and
//! nowhere to put engine-level ordering assertions either — its four Task 3a tests would all have
//! had to reach the executor through the HTTP surface to observe an ordering that is not an HTTP
//! property.
//!
//! So: the same remedy Task 0c applied to the pin cases (`tests/pins.rs`) and the Task 0 gate
//! applied to Track C's server-plane cases (`http_engine_state.rs`) — a track-owned file, carved
//! before the track needs it rather than discovered missing mid-task. The fixture block stays
//! shared in [`common`]; only the subject moves.

mod common;

use tempfile::TempDir;

use tessera_lifecycle::wal::WalRow;
use tessera_types::EntityId;

use common::{build_fixture, open_engine};

/// IMPORTANT I-9: an entity ingested after the build has no locator slot and no extent entry —
/// the live map must answer first, or `external_id_of` would wrongly report "this item has no
/// external id" for one that does.
///
/// Moved here verbatim from `tests/viewport.rs` (Phase 2 stage 2.1, controller ruling on the
/// Task 3a report's F1). It is the **only** `accept_ingest` call site in the engine's own tests,
/// so it is the one Task 3a's signature change has to carry — and it asserts a property that
/// survives that change unaltered: whatever assigns the ID, the live map must answer for it
/// before the build's locator does.
#[test]
fn drill_down_resolves_an_external_id_for_a_post_build_entity() {
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

    let new_entity = EntityId::new(engine.allocator_high_water());
    let row = WalRow {
        external_id: Some(b"post-build-key".to_vec()),
        entity_id: new_entity,
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
    };
    engine
        .accept_ingest(
            vec![row],
            vec![Vec::new()],
            "batch-1".to_string(),
            [0u8; 32],
        )
        .unwrap();

    let resolved = engine.resolve_external_id(b"post-build-key").unwrap();
    assert_eq!(resolved, Some(new_entity));

    let external = engine.external_id_of(new_entity).unwrap();
    assert_eq!(external.as_deref(), Some(&b"post-build-key"[..]));
}
