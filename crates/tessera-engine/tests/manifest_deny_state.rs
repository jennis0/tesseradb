//! The loader applies the deny state it honours (§8.1).
//!
//! Contracts §2.3 makes a `SEGMENTS-<n>.json` complete current state for its partition, so a
//! flush-published manifest carries `deny` (the active suppression set) and `tombstones` (deleted
//! entities that already have rows). `HONOURED_STATE` claims this reader acts on both — and a
//! manifest that opened while its deny state went nowhere would serve every entity it names,
//! which is a worse failure than refusing to open at all.

mod common;

use std::path::Path;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;

/// Rewrite `SEGMENTS-0.json` with `edit` applied to its parsed JSON. The side-manifest carries no
/// digest of its own — only the `files` entries *inside* it are verified — so this needs no
/// resealing.
fn edit_segments_manifest(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join("v00000/partitions/default/SEGMENTS-0.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut value);
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn visible(engine: &Engine) -> u64 {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 10) as usize),
        )
        .unwrap()
        .tiles[0]
        .visible
}

/// A suppression in the manifest hides its entity from the moment the node opens. Without the
/// seeding this asserts, honouring `"deny"` would mean nothing more than declining to refuse the
/// manifest — the strictly worse of the two failures, because the node then serves.
#[test]
fn a_manifest_suppression_hides_its_entity_from_the_first_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let baseline = visible(&open_engine(
        &root,
        &tmp.path().join("cache-0"),
        &tmp.path().join("wal-0.log"),
    ));

    let suppressed = entity_of_source(&root, 7);
    edit_segments_manifest(&root, |value| {
        value["deny"] = serde_json::json!([{ "entity_id": suppressed, "cause": "suppress" }]);
    });

    let after = visible(&open_engine(
        &root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
    ));
    assert_eq!(
        after,
        baseline - 1,
        "the manifest's suppression must be in force at open, not merely parsed"
    );
}

/// A tombstone names an entity already deleted. Nothing retires a deletion deny (the stamp ledger
/// does not exist), so the entry stands for the process's life and the row stays hidden.
#[test]
fn a_manifest_tombstone_hides_its_entity_from_the_first_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let baseline = visible(&open_engine(
        &root,
        &tmp.path().join("cache-0"),
        &tmp.path().join("wal-0.log"),
    ));

    let deleted = entity_of_source(&root, 11);
    edit_segments_manifest(&root, |value| {
        value["tombstones"] = serde_json::json!([deleted]);
    });

    let after = visible(&open_engine(
        &root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
    ));
    assert_eq!(after, baseline - 1);
}

/// The negative control: an empty deny state changes nothing. A seeding step that applied
/// something for an absent entry would show up here and nowhere else.
#[test]
fn an_empty_deny_state_hides_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let baseline = visible(&open_engine(
        &root,
        &tmp.path().join("cache-0"),
        &tmp.path().join("wal-0.log"),
    ));
    edit_segments_manifest(&root, |value| {
        value["deny"] = serde_json::json!([]);
        value["tombstones"] = serde_json::json!([]);
    });
    assert_eq!(
        visible(&open_engine(
            &root,
            &tmp.path().join("cache-1"),
            &tmp.path().join("wal-1.log")
        )),
        baseline
    );
}

/// The entity id `tessera build` assigned to source row `source_id`.
fn entity_of_source(root: &Path, source_id: u64) -> u64 {
    source_to_new_map(root, "v00000")[&source_id]
}
