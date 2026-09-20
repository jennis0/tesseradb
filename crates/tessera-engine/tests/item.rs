//! `Engine::item`: the drill-down from a `tessera_id` to one item's card, over the shared
//! ~10k-item synthetic bundle in [`common`].
//!
//! What the verb owes a caller is the same at every layer: an id it cannot see and an id that
//! names nothing are one answer, a corrupt sidecar is an error rather than a missing external id,
//! and neither costs a row projection.

mod common;

use tempfile::TempDir;

use tessera_build::{build, BuildArgs};
use tessera_engine::EngineError;
use tessera_store::read::open_bundle;
use tessera_store::StoreError;
use tessera_types::EntityId;

use common::*;

/// `Engine::item` resolves a row wherever it sits: the item asserted here is a source item whose
/// signature-sorted entity id — and therefore its row — is not among the first built, and it comes
/// back with the right external id. `Permutation::row_of` is an O(1) bijection lookup and does not
/// care where the row sits; contracts r6 replaced the identity column's contents with the opaque
/// `tessera_id`, so a scan of *that* column would search the wrong space entirely.
///
/// **What this pins is the reach of the lookup, not its mechanism.** An implementation that walked
/// the entity-id column top to bottom would find the same row and pass, so the name says "far from
/// the segment's start" rather than claiming to discriminate a scan.
///
/// Mutations this kills: a lookup truncated to a prefix of the rows, or one that searches only the
/// first segment.
#[test]
fn item_lookup_resolves_a_row_far_from_the_segments_start() {
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

    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let last_source = N_ITEMS - 1;
    let entity = EntityId::new(source_to_new[&last_source]);
    let id = test_key().forward(0, entity).unwrap();

    let out = engine.item(&session, id, None).unwrap();
    assert!(
        out.is_some(),
        "an item far from segment start must still resolve through the permutation"
    );
    assert_eq!(
        out.unwrap().external_id,
        Some(last_source.to_le_bytes().to_vec())
    );
}

/// `Engine::item`'s `idset` argument is checked against the ONE generation this
/// call loads, before inversion, and identically for every `id` — a real, visible id and one
/// naming nothing both take the same `Err(StaleIdSet)` for the same mismatched idset
/// (mirrors `item_with_a_stale_idset_is_409_and_a_matching_idset_changes_nothing` in
/// `tessera-server`'s `http.rs`, at the engine layer this fix moved the check into). A matching
/// idset is a no-op, same as `None`.
#[test]
fn item_idset_check_is_entity_independent_and_decided_before_inversion() {
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

    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let visible_id = test_key().forward(0, entity).unwrap();
    let unknown_id = test_key()
        .forward(0, EntityId::new(N_ITEMS + 1_000_000))
        .unwrap();

    // Fixture's idset is 1 (see `build_fixture_n`). A matching idset changes nothing.
    assert!(engine
        .item(&session, visible_id, Some(1))
        .unwrap()
        .is_some());

    // A stale idset is `Err(StaleIdSet)` for a real, visible id...
    assert!(matches!(
        engine.item(&session, visible_id, Some(2)),
        Err(EngineError::StaleIdSet)
    ));
    // ...and identically for an id naming nothing — decided before inversion, so it cannot be
    // used to learn whether an id exists.
    assert!(matches!(
        engine.item(&session, unknown_id, Some(2)),
        Err(EngineError::StaleIdSet)
    ));
}

/// A bundle built without minted external IDs (the spec-conformant default — contracts §2.4:
/// callers supplied none, so the build wrote no extents and no locator) must serve the item
/// drill-down normally: `external_id` is `None` — the ordinary "identity is the tessera_id"
/// case — never an `InvalidSidecar` error. Regression test for the review finding that
/// `external_id_of_checked` treated every built entity of a no-sidecar bundle as a
/// live-map inconsistency and 500'd the whole `/v1/items` verb.
#[test]
fn item_drill_down_works_on_a_bundle_with_no_external_id_sidecar() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    write_points_n(&tmp.path().join("points.parquet"), N_ITEMS);
    write_pairs_n(&tmp.path().join("pairs.parquet"), N_ITEMS);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: tmp.path().join("points.parquet"),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(tmp.path().join("pairs.parquet")),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: bundle_root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args).expect("no-mint build should succeed");

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // Any built entity: below the high-water, no locator anywhere. Drill-down must succeed
    // with no external id, for the first entity and the last alike.
    // (`item`'s third argument is the fix-wave idset check, landed on this branch after main's
    // version of this test was written; `None` preserves its original meaning.)
    for entity in [0, N_ITEMS - 1] {
        let id = test_key().forward(0, EntityId::new(entity)).unwrap();
        let out = engine
            .item(&session, id, None)
            .expect("a no-sidecar bundle must serve items, not error");
        let out = out.expect("a visible item must resolve");
        assert_eq!(
            out.external_id, None,
            "an item with no caller-supplied external id reports None"
        );
    }
}

/// An identifier naming nothing and one naming an invisible item are indistinguishable
/// — one `Ok(None)` from one code path, with no error variant separating the two.
#[test]
fn an_unknown_id_and_an_invisible_one_are_indistinguishable() {
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
    let session = engine.authorise(&zero_credential()).unwrap();

    // Unknown: an entity id far beyond anything this bundle ever allocated.
    let unknown_id = test_key()
        .forward(0, EntityId::new(N_ITEMS + 1_000_000))
        .unwrap();
    assert_eq!(engine.item(&session, unknown_id, None).unwrap(), None);

    // Known but invisible: a zero-term session sees nothing, so any real item is invisible.
    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let invisible_id = test_key().forward(0, entity).unwrap();
    assert_eq!(engine.item(&session, invisible_id, None).unwrap(), None);
}

/// The timing channel is closed rather than narrowed: the entity-space visibility test never
/// constructs a `RowProjection` (the cached artefact that costs 9.5-19.3s at 10^9), for an unknown
/// id or an
/// invisible one, on a session that has never drawn a viewport.
#[test]
fn the_item_path_never_constructs_a_row_projection() {
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
    assert_eq!(
        engine.row_projection_cache_len(),
        0,
        "no viewport drawn yet"
    );

    let unknown_id = test_key().forward(0, EntityId::new(N_ITEMS + 1)).unwrap();
    engine.item(&session, unknown_id, None).unwrap();
    assert_eq!(
        engine.row_projection_cache_len(),
        0,
        "an unknown id must not build a projection"
    );

    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let visible_id = test_key().forward(0, entity).unwrap();
    engine.item(&session, visible_id, None).unwrap();
    assert_eq!(
        engine.row_projection_cache_len(),
        0,
        "a visible id must not build a projection either"
    );
}

/// The behaviour the row-space formulation could not offer: a client's FIRST request may be a
/// drill-down, and a visible item must return `Some` — not a uniform 404 pending a warmed
/// per-session cache.
#[test]
fn drill_down_works_on_a_session_that_has_never_drawn_a_viewport() {
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

    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let id = test_key().forward(0, entity).unwrap();

    let out = engine.item(&session, id, None).unwrap();
    assert!(
        out.is_some(),
        "a visible item's first request against this session may be a drill-down"
    );
}

/// A corrupt sidecar must surface as `Err`, never fold into `Ok(None)` (which
/// would report "this item has no external id" for one that does, at a `200`). The digest check
/// runs before Arrow decoding (`tessera_store::sidecar`'s `load_validated`), so corrupting any
/// byte of the extent is sufficient to trip it, regardless of where in the file it lands.
#[test]
fn a_sidecar_error_on_drill_down_is_an_error_not_a_missing_external_id() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // Resolve the target entity, and open the engine, against the *pristine* extent first —
    // `Engine::open`'s bundle-open protocol (`tessera_store::read::open_bundle`) eagerly
    // verifies every manifest-listed file's digest up front (a bundle-level integrity property,
    // independent of the sidecar's own per-extent laziness), so corrupting the file before open
    // would fail at `Engine::open` itself rather than exercising the drill-down path this test
    // targets.
    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let id = test_key().forward(0, entity).unwrap();

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // Now corrupt the extent's bytes on disk — the sidecar's lazy open verifies digest
    // and sortedness on first touch, so this failure is deferred until `item()` actually
    // resolves the visible entity's external id.
    let bundle = open_bundle(&bundle_root).unwrap();
    let part = &bundle.partitions["default"];
    let ext_rel = &part.manifest.external_id_runs[0];
    let ext_path = bundle_root.join("v00000").join(ext_rel);
    drop(bundle);
    let mut bytes = std::fs::read(&ext_path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&ext_path, bytes).unwrap();

    let err = engine.item(&session, id, None).unwrap_err();
    assert!(
        matches!(err, EngineError::Store(StoreError::InvalidSidecar { .. })),
        "a corrupt sidecar must be Err(EngineError::Store(InvalidSidecar)), never a fail-open \
         Ok(None): {err:?}"
    );
}

// `drill_down_resolves_an_external_id_for_a_post_build_entity` moved to `tests/write.rs` — see
// that file's module doc. It was this file's only `accept_ingest` call site, and the acceptance
// API's shape belongs beside the rest of the write path rather than here — leaving it would put a
// write-path assertion in a file frozen for every
// track. The subject moved; the shared fixtures did not.
