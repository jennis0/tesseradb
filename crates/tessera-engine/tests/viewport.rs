//! Task 11, Step 1: the masked viewport query, end to end over a ~10k-item synthetic bundle
//! built through `tessera-build`'s library API (Task 8's smoke-test pattern).
//!
//! Every item carries `ALL_TERM` ("0"); every third item (`source_id % 3 == 0`) additionally
//! carries `SUBSET_TERM` ("1"). Most tests below query at `zoom = 0`, where `tiles_for_bbox`
//! always returns exactly one tile covering the whole grid regardless of `bbox` (depth 0 has no
//! bits to discriminate on) — a brute-force oracle over the *input* relation is then trivial: no
//! bbox-intersection geometry to reproduce, just term membership. That leaves the
//! `tile_ranges`/`count_range` tiling path itself unexercised, so
//! `tile_counts_match_brute_force_at_a_non_degenerate_zoom_and_bbox_subset` below queries at
//! `zoom = 4` with a bbox covering a strict subset of tiles, cross-checked against an independent
//! per-item Morton-prefix oracle.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig, EngineError};
use tessera_lifecycle::wal::{ChangeOp, Wal, WalRecord, WalRow};
use tessera_plugin::Passthrough;
use tessera_spatial::{morton_of, tiles_for_bbox, Extent};
use tessera_store::read::open_bundle;
use tessera_store::StoreError;
use tessera_types::{EntityId, IdentityKey, PinId};

const N_ITEMS: u64 = 10_000;
const ALL_TERM: u64 = 0;
const SUBSET_TERM: u64 = 1;

/// A fixed, non-degenerate test key — the same canonical vector used across the identity
/// construction's own tests (`tessera_types::identity`'s `CANONICAL_KEY`) and
/// `tessera-build`'s fixture tests, so a mismatch between crates would show up as a vector
/// disagreement rather than an independently-chosen value.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// The base config for these tests.
///
/// **`theta_target_marks` is raised above `N_ITEMS` on purpose.** These tests assert *masking* —
/// which items a principal may see — not density. Leaving θ live would make every assertion about a
/// point set depend on the density rule's threshold clause as well, so a masking bug and a θ
/// arithmetic bug would be indistinguishable. Raising the target above the fixture's total visible
/// count saturates θ at every depth, which reduces selection to "serve every visible row up to the
/// cap" and isolates what these tests are for. Density itself is tested in `selection.rs`, against
/// fixtures built for it.
fn config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        theta_target_marks: N_ITEMS * 2,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
    }
}

/// A config whose caps are large enough to never truncate a sample — used by tests asserting
/// membership (counts, exact point sets) rather than a cap itself. θ is saturated here too, for the
/// reason given on [`config`].
fn config_uncapped() -> EngineConfig {
    EngineConfig {
        max_k: N_ITEMS as usize,
        k_max_marks: N_ITEMS as usize,
        ..config()
    }
}

/// Every item carries `ALL_TERM`; every third carries `SUBSET_TERM` too.
fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![ALL_TERM, SUBSET_TERM]
    } else {
        vec![ALL_TERM]
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Build the fixture bundle at `out` (Task 8's library API — `tessera_build::build`).
fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    write_points(points_path);
    write_pairs(pairs_path);
    let args = BuildArgs {
        points: points_path.to_path_buf(),
        pairs: pairs_path.to_path_buf(),
        out: out.to_path_buf(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        identity_epoch: 1,
        shard_id: 0,
    };
    build(&args).expect("fixture build should succeed");
}

/// Read the bundle's external-ids extent into a `source_id -> new entity_id` map — the same
/// ground truth `tessera-build`'s own smoke test cross-checks against.
fn source_to_new_map(bundle_root: &Path, prefix: &str) -> BTreeMap<u64, u64> {
    let bundle = open_bundle(bundle_root).unwrap();
    let part = &bundle.partitions["default"];
    let ext_path = bundle_root
        .join(prefix)
        .join(&part.manifest.external_id_extents[0]);
    let file = File::open(&ext_path).unwrap();
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let mut map = BTreeMap::new();
    for batch in reader {
        let batch = batch.unwrap();
        let ext = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        // Contracts r6: the external-id extent's entity column is `UInt32` (entities are capped
        // at `u32::MAX` by the I9 allocator), not the pre-r6 `UInt64`.
        let ent = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
            map.insert(source, ent.value(i) as u64);
        }
    }
    map
}

/// The external id `tessera-build` writes for a source row: the source corpus id, 8 bytes
/// little-endian (see `source_to_new_map`'s decode of the same convention).
fn source_id_key(source_id: u64) -> Vec<u8> {
    source_id.to_le_bytes().to_vec()
}

fn open_engine(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        Passthrough::new(),
        config(),
    )
    .expect("engine should open against a freshly built bundle")
}

fn open_engine_uncapped(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        Passthrough::new(),
        config_uncapped(),
    )
    .expect("engine should open against a freshly built bundle")
}

fn full_coverage_credential() -> Vec<u8> {
    br#"{"terms": ["0"]}"#.to_vec()
}

fn subset_credential() -> Vec<u8> {
    br#"{"terms": ["1"]}"#.to_vec()
}

fn zero_credential() -> Vec<u8> {
    br#"{"terms": []}"#.to_vec()
}

/// (a) A full-coverage session sees every point of the bbox; a small `k` caps the sampled points
/// but never the count. (e) `matched == visible` everywhere (Phase 1 has no filters).
#[test]
fn a_full_coverage_session_sees_every_point_small_k_caps_sampling() {
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

    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
        )
        .unwrap();

    assert_eq!(out.tiles.len(), 1, "zoom 0 is always exactly one tile");
    assert_eq!(out.tiles[0].visible, N_ITEMS, "every item carries ALL_TERM");
    assert_eq!(
        out.tiles[0].matched, out.tiles[0].visible,
        "(e) matched == visible"
    );
    assert_eq!(
        out.points.len(),
        5,
        "k=5 caps sampled points, not the count"
    );
}

/// (b) A session satisfying one term sees exactly that term's items — neither more nor fewer.
#[test]
fn b_subset_session_sees_exactly_its_terms_items() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = open_engine_uncapped(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&subset_credential()).unwrap();

    let expected: u64 = (0..N_ITEMS).filter(|e| e.is_multiple_of(3)).count() as u64;
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize),
        )
        .unwrap();

    assert_eq!(out.tiles.len(), 1);
    assert_eq!(out.tiles[0].visible, expected);
    assert_eq!(out.tiles[0].matched, expected);
    assert_eq!(out.points.len(), expected as usize);

    // Every sampled point must actually be a subset-term source item. Invert the wire-visible
    // `tessera_id` back to the entity id with the same key the fixture was built under (test-only
    // — a real client never gets to do this, per I10) before mapping it to its source id.
    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let new_to_source: BTreeMap<u64, u64> = source_to_new.iter().map(|(&s, &n)| (n, s)).collect();
    let key = test_key();
    for point in &out.points {
        let (shard, entity) = key.invert(point.tessera_id);
        assert_eq!(shard, 0, "fixture uses shard 0 only");
        let source = new_to_source[&entity.raw()];
        assert_eq!(
            source % 3,
            0,
            "sampled point (source id {source}) does not carry SUBSET_TERM"
        );
    }
}

/// (c) A zero-term session is valid (R5) but sees nothing: every count is 0 (the "skip empty"
/// tile rule means the tiles vec itself is empty), and no points.
#[test]
fn c_zero_term_session_sees_nothing() {
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
    assert!(
        session.satisfied.is_empty(),
        "zero-term credential grants nothing"
    );

    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .unwrap();

    assert!(
        out.tiles.is_empty(),
        "every tile has 0 visible and is skipped"
    );
    assert!(out.points.is_empty());
}

/// (d) Suppressing an item via the overlay (here: a change replayed from the WAL at open, the
/// same durability path a live `/control/changes` acceptance would go through) drops the
/// viewport count by exactly one, without touching anything else.
#[test]
fn d_suppressing_an_item_drops_the_count_by_one() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // Baseline: no WAL changes.
    let engine_a = open_engine(
        &bundle_root,
        &tmp.path().join("cache_a"),
        &tmp.path().join("wal_a.log"),
    );
    let session_a = engine_a.authorise(&full_coverage_credential()).unwrap();
    let out_a = engine_a
        .viewport(
            &session_a,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
        )
        .unwrap();

    // A WAL pre-populated with a suppression of source item 5 (established in the bundle, not
    // the WAL — exercising `resolve_from_bundle`, the seam Task 10 left open).
    const SUPPRESS_SOURCE_ID: u64 = 5;
    let wal_path_b = tmp.path().join("wal_b.log");
    {
        let (mut wal, _initial) = Wal::open(&wal_path_b).unwrap();
        wal.append(&WalRecord::Change {
            external_id: SUPPRESS_SOURCE_ID.to_le_bytes().to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let engine_b = open_engine(&bundle_root, &tmp.path().join("cache_b"), &wal_path_b);
    let session_b = engine_b.authorise(&full_coverage_credential()).unwrap();
    let out_b = engine_b
        .viewport(
            &session_b,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
        )
        .unwrap();

    assert_eq!(
        out_b.tiles[0].visible,
        out_a.tiles[0].visible - 1,
        "suppressing one item must drop the count by exactly one"
    );
    assert_eq!(out_b.tiles[0].matched, out_b.tiles[0].visible);
}

/// (f) Selection returns the *k* lowest `tessera_id`s in the mask — **not** the first *k* in row
/// (Morton) order.
///
/// This test replaces one that asserted the opposite, and the replacement is the point: the
/// placeholder took `mask.iter_range(..)` in row order, which within a leaf Morton cell *is*
/// `tessera_id` order (storage sort is `(morton, tessera_id)`), so the two only diverge across
/// cells. At zoom 0 the whole segment is one tile spanning every cell, so the divergence is maximal
/// and the second assertion below — that the served set is *not* the first three rows — is what
/// actually pins the fix. A sample ordered by row order is a sample ordered by **permission
/// signature**, because entity IDs are signature-sorted permanently under I9; that is the defect
/// `docs/design-memos/2026-07-30-priority-as-identity-prefix.md` exists to close.
#[test]
fn f_selection_returns_the_lowest_tessera_ids_not_the_first_rows() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let bundle = open_bundle(&bundle_root).unwrap();
    let segment = &bundle.partitions["default"].slices["s0"].segments[0];
    let ids = segment.columns.tessera_id();

    // The definition's answer, computed independently of the engine: the three smallest identities
    // in the segment, ascending.
    let mut by_id: Vec<(u64, usize)> = ids.iter().copied().zip(0..).collect();
    by_id.sort_unstable();
    let expected: Vec<u64> = by_id[0..3].iter().map(|&(id, _)| id).collect();
    let expected_xy: Vec<(f32, f32)> = by_id[0..3]
        .iter()
        .map(|&(_, row)| (segment.columns.x()[row], segment.columns.y()[row]))
        .collect();

    // What the retired placeholder would have returned.
    let first_rows_in_row_order: Vec<u64> = ids[0..3].to_vec();

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 3),
        )
        .unwrap();

    assert_eq!(out.points.len(), 3);
    assert_eq!(
        out.tiles[0].served, 3,
        "the tile row must report what it served"
    );

    let got: Vec<u64> = out.points.iter().map(|p| p.tessera_id.raw()).collect();
    assert_eq!(
        got, expected,
        "served set must be the three lowest identities"
    );
    for (i, point) in out.points.iter().enumerate() {
        assert_eq!((point.x, point.y), expected_xy[i], "point {i} geometry");
    }

    // The discriminating assertion. If the fixture ever changed such that these coincided, the
    // test above would pass while proving nothing — so the divergence is asserted, not assumed.
    assert_ne!(
        expected, first_rows_in_row_order,
        "fixture is degenerate: the lowest identities are also the first rows, so this test \
         cannot distinguish the definition from the retired row-order placeholder"
    );
    assert_ne!(
        got, first_rows_in_row_order,
        "selection is still returning the first k rows in Morton order — the sample is ordered by \
         permission signature (I9 signature-sorted entity IDs), which is the defect being fixed"
    );
}

/// **θ must not move when the viewer pans.** A viewport-recomputed θ sheds marks on every pan,
/// which is the churn the whole priority scheme exists to avoid — so θ is derived from the session's
/// composed visible total and the request's depth, and from nothing about `bbox`.
///
/// This test pans across two overlapping bboxes at a fixed zoom and asserts that a tile appearing in
/// both is served identically. It holds the generation fixed (one engine, no overlay writes) because
/// θ is generation-*dependent* by design — an overlay swap may move it, a pan may not.
#[test]
fn theta_does_not_move_when_the_viewport_pans() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // θ live, not saturated: this test is about θ's inputs, so it must actually be doing something.
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
            k_min: 2,
            k_max_marks: 128,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
        },
    )
    .unwrap();
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // Two overlapping viewports at zoom 3, chosen so their tile sets intersect.
    let wide = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .unwrap();
    let narrow = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 3, [400.0, 400.0, 700.0, 700.0], 30),
        )
        .unwrap();

    let shared: Vec<u64> = narrow
        .tiles
        .iter()
        .map(|t| t.tile)
        .filter(|p| wide.tiles.iter().any(|t| t.tile == *p))
        .collect();
    assert!(
        !shared.is_empty(),
        "the two viewports must share at least one tile for this test to mean anything"
    );

    for prefix in shared {
        let a = wide.tiles.iter().find(|t| t.tile == prefix).unwrap();
        let b = narrow.tiles.iter().find(|t| t.tile == prefix).unwrap();
        assert_eq!(
            a.visible, b.visible,
            "tile {prefix}: visible moved on a pan"
        );
        assert_eq!(
            a.served, b.served,
            "tile {prefix}: served moved on a pan — θ is being recomputed per viewport, which \
             sheds marks on every pan"
        );
    }
}

/// A non-empty tile always draws at least one mark, at every depth, under the live-θ configuration.
/// Owner decision 4 accepts that the density rule draws *fewer* marks than the retired flat `k`;
/// it does not accept blank tiles.
#[test]
fn no_visible_tile_is_ever_served_empty() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
            k_min: 2,
            k_max_marks: 128,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
        },
    )
    .unwrap();
    // The sparsest principal in this fixture — a third of the corpus.
    let session = engine.authorise(&subset_credential()).unwrap();

    for zoom in 0..8u8 {
        let out = engine
            .viewport(
                &session,
                ViewportRequest::new("s0", zoom, [0.0, 0.0, 1000.0, 1000.0], 30),
            )
            .unwrap();
        for tile in &out.tiles {
            assert!(tile.visible > 0, "an empty tile should not be reported");
            assert!(
                tile.served >= 1,
                "zoom {zoom}, tile {}: {} visible but nothing served — the floor clause is not \
                 holding, which is an I7 regression",
                tile.tile,
                tile.visible
            );
        }
        assert_eq!(
            out.points.len(),
            out.tiles.iter().map(|t| t.served as usize).sum::<usize>(),
            "zoom {zoom}: the points batch length must equal the sum of served counts"
        );
    }
}

/// Review finding: every other test in this suite queries at `zoom = 0`, where `bbox` is inert
/// (`tiles_for_bbox` always returns the single whole-grid tile regardless of its value) — so the
/// `tile_ranges`/`count_range` aggregation path, and `TileCount.tile`'s prefix, were never
/// actually exercised against a bbox that selects a strict subset of tiles. This test picks a
/// non-degenerate zoom (4: a 16x16 tile grid) and a bbox covering roughly one quadrant of the
/// extent, then cross-checks the engine's per-tile counts against an independent brute-force
/// oracle: each item's own `(x, y)` quantised to a Morton code and shifted to a zoom-4 tile
/// prefix by hand (the same public `tessera_spatial` functions the engine itself calls, but
/// grouped independently of `tile_ranges`/`count_range`) — an off-by-one in either would show up
/// here even though it passes at zoom 0.
#[test]
fn tile_counts_match_brute_force_at_a_non_degenerate_zoom_and_bbox_subset() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = open_engine_uncapped(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    const ZOOM: u8 = 4;
    // Roughly the lower-left quadrant of the extent — a strict, non-degenerate subset of zoom
    // 4's 16x16 tile grid (unlike zoom 0, whose one tile is inert to `bbox`).
    let bbox = [0.0, 0.0, 250.0, 250.0];

    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", ZOOM, bbox, N_ITEMS as usize),
        )
        .unwrap();

    let e = extent();
    let touched_tiles = tiles_for_bbox(bbox, ZOOM, &e);
    let touched_prefixes: std::collections::BTreeSet<u64> =
        touched_tiles.iter().map(|t| t.prefix).collect();
    assert!(
        touched_prefixes.len() > 1,
        "the bbox must touch more than one tile for this to be a non-degenerate check"
    );

    // Independent brute-force oracle, entirely bypassing `tile_ranges`/`count_range`: each
    // item's own fixture `(x, y)` (the exact formula `write_points` used) quantised to a Morton
    // code, then shifted to a zoom-4 prefix (`Tile::code_range`'s own inverse: `prefix = code >>
    // (32 - 2*depth)`).
    let shift = 32 - 2 * ZOOM as u32;
    let mut brute_counts: BTreeMap<u64, u64> = BTreeMap::new();
    for source_id in 0..N_ITEMS {
        let x = ((source_id * 37) % 1000) as f64;
        let y = ((source_id * 53) % 1000) as f64;
        let code = morton_of(x, y, &e).raw() as u64;
        let prefix = code >> shift;
        if touched_prefixes.contains(&prefix) {
            *brute_counts.entry(prefix).or_insert(0) += 1;
        }
    }
    let expected: BTreeMap<u64, u64> = brute_counts.into_iter().filter(|&(_, c)| c > 0).collect();
    assert!(
        !expected.is_empty(),
        "the fixture bbox must touch at least one non-empty tile for this test to mean anything"
    );

    let got: BTreeMap<u64, u64> = out.tiles.iter().map(|t| (t.tile, t.visible)).collect();
    assert_eq!(
        got, expected,
        "tile prefixes/counts must match the brute-force (x,y) -> Morton -> prefix oracle exactly"
    );
    for t in &out.tiles {
        assert_eq!(t.matched, t.visible, "(e) matched == visible");
        assert!(
            touched_prefixes.contains(&t.tile),
            "engine returned tile prefix {} outside tiles_for_bbox's own tile set",
            t.tile
        );
    }
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

/// `Engine::open` seeds the I9 allocator at `max(manifest high-water, WAL high-water)` — here,
/// with an empty WAL, that is exactly the bundle's `entity_id_high_water` (== `N_ITEMS`, the
/// bootstrap build's dense `0..n` assignment).
#[test]
fn engine_open_seeds_the_allocator_from_the_manifest_high_water() {
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
    assert_eq!(engine.allocator_high_water(), N_ITEMS);
}

/// Task 9, Step 1: `Engine::item` inverts the wire `tessera_id` and locates its row via
/// `Permutation::row_of` — an O(1) bijection lookup, never a linear scan of an identity column
/// (contracts r6 replaced that column's contents with the opaque `tessera_id`, so a scan of it
/// would search the wrong space entirely). Asserted against a source item whose signature-sorted
/// entity id (and therefore its row) is not the first one built — a truncated or
/// first-rows-only lookup would miss it, while the permutation's O(1) `row_of` does not care
/// where the row sits.
#[test]
fn item_lookup_goes_through_the_permutation_not_a_column_scan() {
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

    let out = engine.item(&session, id).unwrap();
    assert!(
        out.is_some(),
        "an item far from segment start must still resolve through the permutation"
    );
    assert_eq!(
        out.unwrap().external_id,
        Some(last_source.to_le_bytes().to_vec())
    );
}

/// Owner ruling: an identifier naming nothing and one naming an invisible item are indistinguishable
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
    assert_eq!(engine.item(&session, unknown_id).unwrap(), None);

    // Known but invisible: a zero-term session sees nothing, so any real item is invisible.
    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let invisible_id = test_key().forward(0, entity).unwrap();
    assert_eq!(engine.item(&session, invisible_id).unwrap(), None);
}

/// CRITICAL C-5, closed rather than narrowed: the entity-space visibility test never constructs
/// a `RowProjection` (the cached artefact that costs 9.5-19.3s at 10^9), for an unknown id or an
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
    engine.item(&session, unknown_id).unwrap();
    assert_eq!(
        engine.row_projection_cache_len(),
        0,
        "an unknown id must not build a projection"
    );

    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let entity = EntityId::new(source_to_new[&0]);
    let visible_id = test_key().forward(0, entity).unwrap();
    engine.item(&session, visible_id).unwrap();
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

    let out = engine.item(&session, id).unwrap();
    assert!(
        out.is_some(),
        "a visible item's first request against this session may be a drill-down"
    );
}

/// CRITICAL N-3: a corrupt sidecar must surface as `Err`, never fold into `Ok(None)` (which
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

    // Now corrupt the extent's bytes on disk — the sidecar's lazy open (Task 8) verifies digest
    // and sortedness on first touch, so this failure is deferred until `item()` actually
    // resolves the visible entity's external id.
    let bundle = open_bundle(&bundle_root).unwrap();
    let part = &bundle.partitions["default"];
    let ext_rel = &part.manifest.external_id_extents[0];
    let ext_path = bundle_root.join("v00000").join(ext_rel);
    drop(bundle);
    let mut bytes = std::fs::read(&ext_path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&ext_path, bytes).unwrap();

    let err = engine.item(&session, id).unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidSidecar { .. }),
        "a corrupt sidecar must be Err(InvalidSidecar), never a fail-open Ok(None): {err:?}"
    );
}

/// IMPORTANT I-9: an entity ingested after the build has no locator slot and no extent entry —
/// the live map must answer first, or `external_id_of` would wrongly report "this item has no
/// external id" for one that does.
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

/// S6: `Allocator::try_new`'s own doc calls it "the check that belongs at open", and `Engine::open`
/// is open — it must refuse a seed at or above `u32::MAX` before any ingest, rather than let the
/// first allocation surface it as an opaque exhaustion error. The seed comes from durable state
/// this process did not write (MANIFEST's high-water, or a replayed WAL lease), so a hand-edited or
/// corrupt value has to fail closed here.
#[test]
fn engine_open_refuses_an_out_of_range_allocator_seed() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // A lease claiming the whole u32 space — `high_water_from` folds `Lease.hi` into the seed, so
    // this is the WAL-side half of the seeding rule, reached without touching MANIFEST's digest.
    let wal_path = tmp.path().join("wal.log");
    {
        let (mut wal, _initial) = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::Lease {
            lo: u32::MAX as u64 - 1,
            hi: u32::MAX as u64,
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let opened = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &wal_path,
        Passthrough::new(),
        config(),
    );
    let Err(err) = opened else {
        panic!("a seed at u32::MAX must be refused at open, not at the first ingest");
    };
    assert!(
        matches!(err, EngineError::Malformed(ref d) if d.contains("allocator")),
        "expected a typed refusal naming the allocator, got {err:?}"
    );
}

/// Residency proxy: `Engine::open` must never touch the external-id sidecar (Task 8's per-extent
/// laziness guarantee, contracts §0.3 deviation 9) — the real memory-residency measurement is
/// Task 15's memo.
///
/// **The files are removed from disk before the engine opens.** `is_open()` alone was not a test
/// of this: it only reports the sidecar's own `OnceLock` state, and `open_bundle`'s `verify_files`
/// pass — which read and SHA-256'd every extent and the locator, the whole 18.9 GB sequential read
/// the deviation exists to remove — never sets it. That defect was green under an `is_open()`
/// assertion for a whole commit sequence. Deleting the files makes any read of them, at any layer,
/// a hard failure of `Engine::open`, which is the property actually claimed.
#[test]
fn engine_open_does_not_touch_the_sidecar() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // Every sidecar file the build wrote, named from MANIFEST's own `files` map rather than
    // guessed, so this cannot silently check nothing if the layout moves.
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle_root.join("CURRENT")).unwrap()).unwrap();
    let prefix_dir = bundle_root.join(current["prefix"].as_str().unwrap());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(prefix_dir.join("MANIFEST.json")).unwrap()).unwrap();
    let sidecar_files: Vec<PathBuf> = manifest["files"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|rel| rel.contains("/entities/"))
        .map(|rel| prefix_dir.join(rel))
        .collect();
    assert!(
        sidecar_files.len() >= 2,
        "fixture must write at least an extent and the locator, found {sidecar_files:?}"
    );
    // The digests stay in MANIFEST — the deviation defers verification, it does not drop it.
    for path in &sidecar_files {
        let rel = path.strip_prefix(&prefix_dir).unwrap().to_string_lossy();
        let rel = rel.replace('\\', "/");
        assert!(
            manifest["files"][&rel]["sha256"].is_string(),
            "{rel} must keep its digest in MANIFEST so the sidecar can verify it at first touch"
        );
        std::fs::remove_file(path).unwrap();
    }

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    assert!(
        !engine.external_id_sidecar_is_open(),
        "Engine::open must not touch the external-id sidecar"
    );

    // Deferred, not dropped: the first resolution *does* reach for the file, and fails closed
    // because it is gone.
    let err = engine
        .resolve_external_id(&source_id_key(0))
        .expect_err("the first resolution must reach the (now absent) extent and fail closed");
    assert!(
        matches!(
            err,
            StoreError::InvalidSidecar { .. } | StoreError::Io { .. }
        ),
        "expected a typed sidecar/IO error, got {err:?}"
    );
}

/// Step 3: latency sanity at 2.4M items — a generous local gate (p99 < 50ms); the real 10ms gate
/// is Task 16, at 10⁹. Builds `/tmp/tessera-2m4` from the real corpus if it is not already there
/// (disk is tight — this bundle is meant to be reused across runs, not deleted after each one).
///
/// `#[ignore]`d: this is a real-corpus, multi-second build plus a real timing measurement, not a
/// fast unit test — run explicitly with `cargo test --release -p tessera-engine --test viewport \
/// -- --ignored latency_sanity_at_2_4m_p99_under_50ms`.
#[test]
#[ignore]
fn latency_sanity_at_2_4m_p99_under_50ms() {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use std::time::Instant;

    const ITEM_LIMIT: u64 = 2_422_486;

    let bundle_root = PathBuf::from("/tmp/tessera-2m4");
    if !bundle_root.join("CURRENT").exists() {
        let args = BuildArgs {
            points: PathBuf::from("data/scaled/geometry.parquet"),
            pairs: PathBuf::from("data/scaled/pairs/categories-subclass.pairs.parquet"),
            out: bundle_root.clone(),
            // Identity extent (contracts §2.5 grid): `geometry.parquet` stores Morton codes, not
            // coordinates (Task 8's `read_points` Morton branch requires this exact extent).
            extent: Extent {
                x_min: 0.0,
                x_max: 65536.0,
                y_min: 0.0,
                y_max: 65536.0,
            },
            slice_id: "s0".to_string(),
            limit: Some(ITEM_LIMIT),
            identity_key: test_key(),
            identity_key_hex: TEST_KEY_HEX.to_string(),
            identity_epoch: 1,
            shard_id: 0,
        };
        build(&args).expect("2.4M fixture build should succeed");
    }

    let tmp = TempDir::new().unwrap();
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // **Production defaults, deliberately.** Everything else in this file saturates theta
            // so that masking assertions do not also depend on the density rule — but this is the
            // only latency gate in the tree, and under saturation the threshold clause never binds,
            // `admits` is a constant `true`, and the counting/selecting branch is barely exercised.
            // It would have measured a path the server does not take. Keep these in step with
            // `tessera-server`'s DEFAULT_* constants.
            max_k: 1_000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
        },
    )
    .expect("engine should open the 2.4M bundle");

    // A real descriptor from the built dictionary (the real corpus's term ids, unlike the
    // synthetic fixtures above) — read directly from `terms-0.dict` rather than guessed.
    let descriptor = first_dictionary_descriptor(&bundle_root);
    let auth = format!(r#"{{"terms": ["{descriptor}"]}}"#);
    // Warm token: authorise once, outside the timing loop — a session's fragment is built once
    // at authorise time (I2) and reused across every viewport, exactly as a real client would.
    let session = engine
        .authorise(auth.as_bytes())
        .expect("authorise should succeed");

    let mut rng = StdRng::seed_from_u64(42);
    let mut latencies = Vec::with_capacity(300);
    for _ in 0..300 {
        let x0: f64 = rng.gen_range(0.0..65000.0);
        let y0: f64 = rng.gen_range(0.0..65000.0);
        let x1 = (x0 + rng.gen_range(1.0..500.0)).min(65536.0);
        let y1 = (y0 + rng.gen_range(1.0..500.0)).min(65536.0);
        let zoom: u8 = rng.gen_range(4..=12);

        let start = Instant::now();
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", zoom, [x0, y0, x1, y1], 30),
            )
            .expect("viewport should succeed");
        latencies.push(start.elapsed());
    }

    latencies.sort();
    let p99_idx = ((latencies.len() as f64) * 0.99) as usize;
    let p99 = latencies[p99_idx.min(latencies.len() - 1)];
    println!("p99 latency over 300 random viewports at 2.4M items: {p99:?}");
    assert!(
        p99.as_millis() < 50,
        "p99 latency {p99:?} exceeds the 50ms generous local gate (the real 10ms gate is Task \
         16, at 10⁹)"
    );
}

fn first_dictionary_descriptor(bundle_root: &Path) -> String {
    let dict_path = bundle_root.join("v00000/dictionary/terms-0.dict");
    let data = std::fs::read(dict_path).unwrap();
    let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    String::from_utf8(data[4..4 + len].to_vec()).unwrap()
}

// ---------------------------------------------------------------------------------------------
// §3.3 — the density underlay
// ---------------------------------------------------------------------------------------------

fn config_with_underlay(max_cells: usize) -> EngineConfig {
    EngineConfig {
        max_underlay_cells: max_cells,
        max_tiles_per_request: 262_144,
        ..config()
    }
}

fn open_engine_with(bundle_root: &Path, tmp: &Path, cfg: EngineConfig) -> Engine {
    Engine::open(
        bundle_root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        cfg,
    )
    .unwrap()
}

/// **I2.** Sub-cell counts are exact *masked* cardinalities, so a tile's sub-cells must sum to that
/// tile's own `visible` — for every principal, not just the fully-authorised one.
///
/// This is the assertion that would fail if the underlay ever read raw row counts and gated
/// afterwards: an unmasked sub-cell sum would equal the tile's stored row count instead, which for
/// the subset principal is three times larger.
#[test]
fn underlay_sub_cells_sum_to_the_tile_s_masked_visible_count() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_with(&bundle_root, tmp.path(), config());

    for credential in [full_coverage_credential(), subset_credential()] {
        let session = engine.authorise(&credential).unwrap();
        for (zoom, offset) in [(0u8, 2u8), (2, 3), (4, 2)] {
            let out = engine
                .viewport(
                    &session,
                    ViewportRequest::new("s0", zoom, [0.0, 0.0, 1000.0, 1000.0], 30)
                        .underlay_offset(Some(offset)),
                )
                .unwrap();
            assert!(!out.sub_cells.is_empty(), "zoom {zoom}: underlay was empty");

            for tile in &out.tiles {
                let summed: u64 = out
                    .sub_cells
                    .iter()
                    .filter(|c| (c.cell >> (2 * offset as u32)) == tile.tile)
                    .map(|c| c.count)
                    .sum();
                assert_eq!(
                    summed, tile.visible,
                    "zoom {zoom} offset {offset}, tile {}: sub-cells summed to {summed} but the \
                     masked visible count is {} — an unmasked underlay would over-count here (I2)",
                    tile.tile, tile.visible
                );
            }
            assert!(
                out.sub_cells.iter().all(|c| c.count > 0),
                "empty sub-cells must be omitted, exactly as empty tiles are"
            );
        }
    }
}

/// A less-authorised principal sees strictly smaller sub-cell totals than a fully-authorised one —
/// the underlay is per-viewer, like every other count (§7.1).
#[test]
fn underlay_totals_are_per_viewer() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_with(&bundle_root, tmp.path(), config());

    let total = |credential: Vec<u8>| -> u64 {
        let session = engine.authorise(&credential).unwrap();
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], 30)
                    .underlay_offset(Some(2)),
            )
            .unwrap()
            .sub_cells
            .iter()
            .map(|c| c.count)
            .sum()
    };

    let full = total(full_coverage_credential());
    let subset = total(subset_credential());
    assert_eq!(full, N_ITEMS, "the full principal sees every item");
    assert!(
        subset < full,
        "the subset principal's underlay ({subset}) must be smaller than the full one's ({full})"
    );
}

/// Absent, or explicitly zero, means no underlay and no cost.
#[test]
fn no_underlay_is_requested_by_default() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_with(&bundle_root, tmp.path(), config());
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    for offset in [None, Some(0)] {
        let out = engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], 30)
                    .underlay_offset(offset),
            )
            .unwrap();
        assert!(
            out.sub_cells.is_empty(),
            "underlay_offset {offset:?} must serve no sub-cells"
        );
    }
}

/// All three underlay bounds **reject** rather than clamp, and that is one rule rather than three.
///
/// A Morton prefix carries no depth of its own, so a silently-reduced offset would hand the client
/// cells it could not interpret; rejecting keeps the depth pinned to `zoom + offset` from the
/// caller's own request. The cell budget is checked before any counting work, because
/// `tiles_for_bbox` is uncapped and the underlay multiplies its output by `4^offset`.
#[test]
fn every_underlay_bound_rejects_rather_than_clamping() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // (a) Above the configured maximum offset.
    let engine = open_engine_with(&bundle_root, tmp.path(), config());
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let err = engine
        .viewport(
            &session,
            // config()'s max_underlay_offset is 4
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 30).underlay_offset(Some(5)),
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::UnderlayRefused(_)),
        "offset above the configured maximum must be refused, got {err:?}"
    );

    // (b) Beyond the depth-16 grid (§5.2 fixes the grid at 2^16 x 2^16). The bbox is deliberately
    // TINY: at zoom 14 a full-extent bbox spans 4^14 = 2.7e8 tiles and would be refused by the
    // `max_tiles_per_request` bound first, which would make this test pass for the wrong reason.
    let err = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 14, [0.0, 0.0, 0.05, 0.05], 30).underlay_offset(Some(4)),
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::UnderlayRefused(_)),
        "zoom 14 + offset 4 needs depth 18 and must be refused, got {err:?}"
    );

    // (c) Over the total sub-cell budget — checked before any counting happens.
    let tmp2 = TempDir::new().unwrap();
    let tight = open_engine_with(&bundle_root, tmp2.path(), config_with_underlay(8));
    let session = tight.authorise(&full_coverage_credential()).unwrap();
    let err = tight
        .viewport(
            &session,
            // 64 cells per tile, well past a budget of 8
            ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], 30).underlay_offset(Some(3)),
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::UnderlayRefused(_)),
        "a request over the cell budget must be refused, got {err:?}"
    );

    // And the same request without the underlay still succeeds — the refusal is scoped to the
    // underlay, not to the viewport.
    tight
        .viewport(
            &session,
            ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .expect("the viewport itself must still be served");
}

/// **The availability bound on the base path.** `zoom` and `bbox` are both caller-chosen and the
/// tile set is their product, so an unbounded `tiles_for_bbox` lets one authenticated request
/// allocate ~69 GB (zoom 16 over the full extent: 65536² tiles at 16 B). It must be counted and
/// refused, never allocated and survived — and the refusal must not depend on the underlay, since an
/// attacker has no reason to ask for one.
#[test]
fn an_over_large_tile_set_is_refused_before_it_is_allocated() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_with(&bundle_root, tmp.path(), config());
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // Zoom 16 over the whole extent: 4.29e9 tiles. No underlay requested.
    let err = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 16, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .unwrap_err();
    match err {
        EngineError::TooManyTiles { demanded, limit } => {
            assert_eq!(demanded, 65536u64 * 65536, "the full grid at depth 16");
            assert_eq!(limit, config().max_tiles_per_request);
        }
        other => panic!("expected TooManyTiles, got {other:?}"),
    }

    // A viewport of the size the system is actually designed for is unaffected: a few hundred tiles.
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 4, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .expect("a normal viewport must still be served");
    assert!(!out.tiles.is_empty());
}

/// `tiles_for_bbox_count` must agree exactly with `tiles_for_bbox().len()` — otherwise the bound
/// above either refuses requests it should serve or fails to refuse the one that matters.
#[test]
fn the_tile_count_estimate_is_exact() {
    let e = extent();
    for zoom in 0..=8u8 {
        for bbox in [
            [0.0, 0.0, 1000.0, 1000.0],
            [0.0, 0.0, 0.05, 0.05],
            [250.0, 300.0, 700.0, 800.0],
            [999.9, 999.9, 1000.0, 1000.0],
            // Reversed corners: `tiles_for_bbox` normalises them, so the count must too.
            [700.0, 800.0, 250.0, 300.0],
        ] {
            assert_eq!(
                tessera_spatial::tiles_for_bbox_count(bbox, zoom, &e),
                tiles_for_bbox(bbox, zoom, &e).len() as u64,
                "zoom {zoom}, bbox {bbox:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// θ's anchor: the composed total, and the equality the disclosure argument rests on
// ---------------------------------------------------------------------------------------------

/// **The I2 property θ's anchor turns on: it is the COMPOSED total, not the frozen projection's.**
///
/// `RowProjection` is `M_auth` *before* the overlay diff. If θ anchored there, mark counts would be
/// scaled by a quantity strictly larger than the viewer's own visible set after a suppression — and
/// a viewer aggregating marks across tiles could solve for it, difference it against its own summed
/// `visible`, and recover **how many of its own items had been denied**. That is a count of items
/// outside `M_auth`, which no Appendix C row admits.
///
/// Asserted here rather than argued, because nothing else in the tree pins it: a regression to
/// `base.cardinality()` would leave every other test passing.
#[test]
fn the_theta_anchor_falls_when_an_item_is_suppressed() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let baseline = open_engine(
        &bundle_root,
        &tmp.path().join("cache_a"),
        &tmp.path().join("wal_a.log"),
    );
    let session_a = baseline.authorise(&full_coverage_credential()).unwrap();
    let before = baseline
        .viewport(
            &session_a,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
        )
        .unwrap();

    const SUPPRESS_SOURCE_ID: u64 = 5;
    let wal_path_b = tmp.path().join("wal_b.log");
    {
        let (mut wal, _initial) = Wal::open(&wal_path_b).unwrap();
        wal.append(&WalRecord::Change {
            external_id: SUPPRESS_SOURCE_ID.to_le_bytes().to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        })
        .unwrap();
        wal.fsync().unwrap();
    }
    let suppressed = open_engine(&bundle_root, &tmp.path().join("cache_b"), &wal_path_b);
    let session_b = suppressed.authorise(&full_coverage_credential()).unwrap();
    let after = suppressed
        .viewport(
            &session_b,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
        )
        .unwrap();

    // The zoom-0 tile's `visible` IS the composed total (see the next test), so this is the anchor
    // observed through the only surface that exposes it.
    assert_eq!(
        after.tiles[0].visible,
        before.tiles[0].visible - 1,
        "the composed total must fall by one when an item is suppressed — if it did not, theta is \
         anchored on the pre-overlay projection and the I2 argument in select.rs is void"
    );
}

/// **The equality `GET /v1/meta`'s disclosure argument rests on.**
///
/// Publishing `theta_target_marks` is defended on the grounds that solving through it yields only
/// the viewer's own composed masked total — *precisely* what a `zoom = 0`, full-extent request
/// already returns as `visible`, in one call. That is currently true by the coincidence of three
/// separate properties in three crates (depth 0 returns one whole-grid tile whatever the bbox;
/// `Tile::code_range` at depth 0 spans the segment; `count_range` and `visible_total` are the same
/// three-term arithmetic), none of which was asserted anywhere. Pin it here, so the disclosure
/// argument cannot be quietly falsified by a change to any one of them.
#[test]
fn the_zoom_zero_count_equals_the_anchor_a_client_could_solve_for() {
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

    for credential in [full_coverage_credential(), subset_credential()] {
        let session = engine.authorise(&credential).unwrap();
        let expected: u64 = engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
            )
            .unwrap()
            .tiles[0]
            .visible;

        // ...and a bbox that does NOT cover the whole extent must give the same answer, because at
        // depth 0 there is only one tile and `bbox` cannot discriminate. If that ever stopped
        // holding, a client could no longer obtain the anchor in one call and the argument would
        // need restating rather than silently weakening.
        let narrow = engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, [10.0, 10.0, 11.0, 11.0], 1),
            )
            .unwrap();
        assert_eq!(
            narrow.tiles[0].visible, expected,
            "zoom 0 must report the whole slice's composed total regardless of bbox"
        );
    }
}

/// `tile_ranges_within` must agree with `tile_ranges` for every tile contained in the window — the
/// property the underlay's restricted search rests on. If it ever diverged, sub-cell counts would
/// silently under-report and the underlay would understate density rather than error.
#[test]
fn a_restricted_tile_range_search_agrees_with_the_full_column_search() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let bundle = open_bundle(&bundle_root).unwrap();
    let segment = &bundle.partitions["default"].slices["s0"].segments[0];

    for parent_depth in 0..5u8 {
        for offset in 1..=3u8 {
            let sub_depth = parent_depth + offset;
            for parent_prefix in 0..(1u64 << (2 * parent_depth as u32)) {
                let parent = tessera_spatial::Tile {
                    prefix: parent_prefix,
                    depth: parent_depth,
                };
                let parent_range = tessera_store::tile_ranges(segment, &parent);
                let first = parent_prefix << (2 * offset as u32);
                for i in 0..(1u64 << (2 * offset as u32)) {
                    let sub = tessera_spatial::Tile {
                        prefix: first + i,
                        depth: sub_depth,
                    };
                    assert_eq!(
                        tessera_store::tile_ranges_within(segment, &sub, parent_range.clone()),
                        tessera_store::tile_ranges(segment, &sub),
                        "parent {parent_prefix}@{parent_depth}, sub {}@{sub_depth}",
                        first + i
                    );
                }
            }
        }
    }
}
