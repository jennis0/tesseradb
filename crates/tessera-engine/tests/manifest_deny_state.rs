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
use tessera_lifecycle::wal::ChangeOp;
use tessera_types::EntityId;

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

/// **The seed is the starting state; the WAL replays over it.** A manifest's deny state is
/// always at least as old as the WAL — publication is deliberately off the ack path — so a record
/// must be able to retire what the manifest carries. `Unsuppress` is the only op that retires
/// anything, and this is the case that gets it wrong under the other order.
///
/// The scenario is the one the publication gap makes ordinary, not a corner: suppress E (published
/// in a manifest), unsuppress E (acked, WAL-durable), crash before the next publication. On
/// restart, replay correctly clears the suppression — and a seed applied *afterwards* re-applies
/// it, reverting an acked disposition. The next manifest write would then make that permanent.
///
/// Fail-closed in direction — the item is hidden, never leaked — but it contradicts what the 200
/// asserted, and it is invisible to every other test here because those carry no WAL.
///
/// **Mutation:** move the seed back after `replay` in `WritePath::reconstruct` and this fails.
#[test]
fn a_wal_unsuppress_beats_a_manifest_suppression_that_predates_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let wal = tmp.path().join("wal.log");
    let entity = entity_of_source(&root, 7);

    let baseline = {
        let engine = open_engine_publishing(&root, &tmp.path().join("cache-0"), &wal);
        let baseline = visible(&engine);

        // Both accepted and WAL-durable, in this order. The suppression is the one the manifest
        // below is standing in for; the unsuppress is the one that outlives it.
        for op in [ChangeOp::Suppress, ChangeOp::Unsuppress] {
            engine
                .accept_change(EntityId::new(entity), op)
                .expect("the change is accepted");
        }
        assert_eq!(
            visible(&engine),
            baseline,
            "the unsuppress took effect live, or this test is asserting the wrong thing"
        );
        baseline
    };

    // The manifest published between the two: it carries the suppression and knows nothing of the
    // unsuppress that followed.
    edit_segments_manifest(&root, |value| {
        value["deny"] = serde_json::json!([{ "entity_id": entity, "cause": "suppress" }]);
    });

    // Reopened on the **same** WAL — which is the whole point: the records that retire the
    // manifest's state are still there.
    let reopened = open_engine_publishing(&root, &tmp.path().join("cache-1"), &wal);
    assert_eq!(
        visible(&reopened),
        baseline,
        "the WAL's acked unsuppress must win over the older manifest's suppression; seeding after \
         replay would revert it, and the next publication would make that permanent"
    );
}

/// The entity id `tessera build` assigned to source row `source_id`.
fn entity_of_source(root: &Path, source_id: u64) -> u64 {
    source_to_new_map(root, "v00000")[&source_id]
}
