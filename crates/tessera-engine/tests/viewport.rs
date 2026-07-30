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
use tessera_engine::viewport::{ViewportRequest, SERIAL_FALLBACK_MAX_ROWS};
use tessera_engine::{
    default_compute_threads, CancelToken, Engine, EngineConfig, EngineError, Session,
};
use tessera_lifecycle::wal::{ChangeOp, Wal, WalRecord, WalRow};
use tessera_plugin::Passthrough;
use tessera_spatial::{morton_of, tiles_for_bbox, Extent};
use tessera_store::read::open_bundle;
use tessera_store::StoreError;
use tessera_types::{EntityId, IdentityKey, PinId};

const N_ITEMS: u64 = 10_000;
const ALL_TERM: u64 = 0;
const SUBSET_TERM: u64 = 1;

/// Calibration task: item count for the byte-equality tests that must exercise the GENUINE
/// parallel fan-out (`Engine::viewport`'s `SERIAL_FALLBACK_MAX_ROWS`, currently 200,000). The
/// scatter these fixtures use (`write_points_n`: `(e*37, e*53) % 1000`) confines every item to the
/// SAME fixed 1000x1000 extent regardless of `n`, and a full-extent request's resolved tiles
/// partition the whole permutation, so `Σ range.len()` over such a request equals `n` exactly —
/// 300,000 clears the threshold with 50% margin, comfortably outside measurement noise. Kept
/// separate from `N_ITEMS` (10,000, well BELOW the threshold) rather than raising it globally: the
/// two headline tests below need genuinely different regimes, and every other test in this file
/// still wants the small, fast fixture.
const PARALLEL_HEADLINE_ITEMS: u64 = 300_000;

/// Fix round 1: the runtime `rows_in_ranges >= SERIAL_FALLBACK_MAX_ROWS` assertions in the two
/// headline tests below only run under `--features bench-timing` (`StageTimings` is all-zero
/// without it). This makes the SAME guarantee a compile-time fact instead, so a plain `cargo test`
/// (no `bench-timing`) still catches `PARALLEL_HEADLINE_ITEMS` being dropped below the threshold
/// by some future edit, rather than silently degrading to serial-vs-serial with no build ever
/// catching it.
const _: () = assert!(PARALLEL_HEADLINE_ITEMS >= SERIAL_FALLBACK_MAX_ROWS);

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
        compute_threads: default_compute_threads(),
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

/// Parameterised over item count so the D-G concurrency tests near the end of this file (which
/// need `RowProjection::new` to take long enough to give a race a real window) can ask for a
/// larger synthetic corpus without duplicating the whole writer. [`build_fixture`] is the
/// `N_ITEMS`-sized default every other test in this file uses.
fn write_points_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
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

/// See [`write_points_n`]'s doc.
fn write_pairs_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..n {
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

/// Build the fixture bundle at `out` (Task 8's library API — `tessera_build::build`), over an
/// `n`-item synthetic corpus. See [`write_points_n`]'s doc for why this is parameterised.
fn build_fixture_n(out: &Path, points_path: &Path, pairs_path: &Path, n: u64) {
    write_points_n(points_path, n);
    write_pairs_n(pairs_path, n);
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

/// Build the fixture bundle at `out` (Task 8's library API — `tessera_build::build`).
fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    build_fixture_n(out, points_path, pairs_path, N_ITEMS)
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
            compute_threads: default_compute_threads(),
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
            compute_threads: default_compute_threads(),
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

/// **The response's tile order is `tiles_for_bbox`'s raster order, not Morton order — and the
/// points are a flat concatenation in exactly that order.**
///
/// `Engine::viewport` resolves every tile's row range in one bounded sweep
/// (`tessera_store::tile_ranges_all`), and that sweep visits tiles in ascending Morton code
/// order, which `tiles_for_bbox`'s `(ty outer, tx inner)` enumeration is *not*. If the sweep's
/// order ever leaked into the response, this test is what catches it: both `ViewportOut.tiles`
/// and the point concatenation the wire format splits by the `served` column depend on the
/// caller's order, and so does the reference oracle.
///
/// The oracle here deliberately bypasses `tile_ranges_all` and goes tile-by-tile through
/// `tile_ranges` — the untouched full-column search — so a sweep that is internally
/// self-consistent but differently ordered cannot satisfy both sides.
#[test]
fn response_tile_order_and_point_concatenation_follow_tiles_for_bbox_not_morton_order() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    const ZOOM: u8 = 4;
    const K: usize = 2;
    let bbox = [0.0, 0.0, 250.0, 250.0];

    let engine = open_engine_uncapped(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(&session, ViewportRequest::new("s0", ZOOM, bbox, K))
        .unwrap();

    let bundle = open_bundle(&bundle_root).unwrap();
    let segment = &bundle.partitions["default"].slices["s0"].segments[0];
    let tessera_ids = segment.columns.tessera_id();

    let tiles = tiles_for_bbox(bbox, ZOOM, &extent());
    let mut expected_prefixes: Vec<u64> = Vec::new();
    let mut expected_points: Vec<u64> = Vec::new();
    for tile in &tiles {
        let range = tessera_store::tile_ranges(segment, tile);
        if range.is_empty() {
            continue;
        }
        expected_prefixes.push(tile.prefix);
        // §7.2's served set, not the retired placeholder's "first k rows in row order". This
        // session is full-coverage and `open_engine_uncapped` saturates θ, so `C_θ` is the tile's
        // whole visible count and `m` is `min(K, visible)` — the K LOWEST identities in the tile,
        // ascending. Row order would be wrong here: storage sorts by `(morton, tessera_id)`, so
        // within one leaf cell the two coincide, but a depth-4 tile spans many cells.
        let mut ids_in_tile: Vec<u64> = range.clone().map(|r| tessera_ids[r as usize]).collect();
        ids_in_tile.sort_unstable();
        ids_in_tile.truncate(K);
        expected_points.extend(ids_in_tile);
    }

    // Guard the test's own premise: if the tile set happened to come back already in Morton
    // order the assertions below would pass for the wrong reason.
    let mut morton_order = expected_prefixes.clone();
    morton_order.sort_unstable();
    assert_ne!(
        expected_prefixes, morton_order,
        "this bbox/zoom must produce a raster order that differs from Morton order, or the \
         test cannot distinguish the two"
    );
    assert!(
        expected_prefixes.len() > 1,
        "the bbox must touch more than one non-empty tile"
    );

    let got_prefixes: Vec<u64> = out.tiles.iter().map(|t| t.tile).collect();
    assert_eq!(
        got_prefixes, expected_prefixes,
        "tiles must be reported in `tiles_for_bbox` enumeration order, with empty tiles skipped"
    );

    let got_points: Vec<u64> = out.points.iter().map(|p| p.tessera_id.raw()).collect();
    assert_eq!(
        got_points, expected_points,
        "points must be a flat concatenation in the reported tile order"
    );
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
            compute_threads: default_compute_threads(),
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

// ---------------------------------------------------------------------------------------------
// Concurrency — D-G's slot-state single-flight over the row-projection cache (F4)
// ---------------------------------------------------------------------------------------------
//
// F4 (`tessera-bench/src/arms/load.rs:34-76`): the previous cache ran `RowProjection::new` —
// seconds at 10⁹ rows — *inside* the map lock on a miss, so distinct sessions' first viewports
// serialised behind one global mutex. D-G replaces the cache value with a slot-state map
// (`Building` | `Ready`); the map lock is now held only for the O(1) state transition, and a
// concurrent arrival on the *same* key observed mid-build does not wait — it gets
// `EngineError::ProjectionBuilding` immediately, never a parked thread.
//
// The state machine itself (single-flight, non-blocking waiters, panic safety) is proven
// deterministically — no sleeps, no timing slack — by `tessera-engine`'s own `single_flight`
// unit tests, which control a build's start and finish with channels because the map is directly
// reachable there. The tests below instead exercise the real, public `Engine::viewport` path
// end to end, which cannot inject a pause into `RowProjection::new`; they use a large enough
// synthetic fixture that a cold build takes tens of milliseconds even unoptimised, well above OS
// thread-wake jitter, and — for the single-flight case — retry across fresh sessions until the
// race is actually observed rather than asserting it lands on a specific attempt.

/// D-G / F4: a concurrent arrival on the same `(token_id, slice, segments_version)` key while
/// another request is still building that key's `RowProjection` gets
/// `EngineError::ProjectionBuilding` immediately rather than blocking; once the build publishes
/// `Ready`, a retried loser succeeds, and the cache never ends up with more than one slot per
/// key.
#[test]
fn concurrent_same_key_viewports_single_flight_others_get_projection_building() {
    const ROUNDS: usize = 25;
    const THREADS: usize = 16;
    const ITEMS: u64 = 150_000;

    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        ITEMS,
    );

    let engine = Arc::new(open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    ));

    for round in 0..ROUNDS {
        // A fresh session -> a fresh `token_id` -> a cache key this engine has never built,
        // regardless of what earlier rounds warmed (`Session::token_id` is a process-lifetime
        // monotone counter — see `Engine::authorise`).
        let session = Arc::new(engine.authorise(&full_coverage_credential()).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let engine = Arc::clone(&engine);
                let session = Arc::clone(&session);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    engine.viewport(
                        &session,
                        ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let successes = results.iter().filter(|r| r.is_ok()).count();
        let building = results
            .iter()
            .filter(|r| matches!(r, Err(EngineError::ProjectionBuilding)))
            .count();
        assert_eq!(
            successes + building,
            THREADS,
            "round {round}: every racing viewport must either succeed or fail with \
             ProjectionBuilding — no other outcome is possible from a single-flight cache"
        );
        assert!(
            successes >= 1,
            "round {round}: the winning builder must always succeed"
        );

        if building == 0 {
            // No contention landed this round (every thread happened to queue behind the map
            // lock only after the builder had already published `Ready`) -- not a failure of
            // the property, just an unlucky schedule. Try another fresh key.
            continue;
        }

        // Contention observed: retry every loser and confirm it now succeeds -- the build must
        // have published `Ready` (never left the key wedged at `Building`, never cached a
        // failure).
        for result in &results {
            if matches!(result, Err(EngineError::ProjectionBuilding)) {
                engine
                    .viewport(
                        &session,
                        ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
                    )
                    .expect("a retried loser must succeed once the build has published Ready");
            }
        }

        assert_eq!(
            engine.row_projection_cache_len(),
            round + 1,
            "exactly one Ready slot per round's fresh key, never more than one build's worth"
        );
        return;
    }

    panic!(
        "never observed same-key contention in {ROUNDS} rounds of {THREADS} threads -- either \
         the race window is too narrow on this machine (widen ITEMS) or single-flight regressed \
         to blocking waiters"
    );
}

/// D-G / F4: distinct sessions' first viewports must build their row projections
/// **concurrently**, not serialise behind one global lock — the exact regression F4 measured
/// (Arm A at c=1000: throughput halves while server CPU *drops* from 712% to 426%, the signature
/// of threads blocked on a lock rather than doing work).
///
/// Measured directly: `serial` times N fresh sessions' cold first viewports run one after
/// another; `concurrent` times N *different* fresh sessions' cold first viewports released
/// together on N threads. Both exclude `Engine::authorise` (sessions are minted before either
/// timer starts) so only the row-projection build itself is measured. If builds still serialised
/// behind one lock, `concurrent` would be roughly `serial` (same total work, funnelled through
/// one mutex, plus contention overhead); genuine overlap should land `concurrent` well under
/// `serial` given more than one core.
#[test]
fn distinct_key_first_viewports_overlap_instead_of_serialising() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // N=4 concurrent builds need at least N cores to genuinely overlap; on a 2- or 3-core
    // runner the 70%-of-serial assertion below has too little headroom (some builds queue for a
    // core regardless of the lock-free design) and flakes for reasons unrelated to F4. Skip
    // rather than loosen the ratio, so a real regression on well-provisioned runners still fails
    // loudly.
    if cores < 4 {
        eprintln!("skipping distinct_key_first_viewports_overlap_instead_of_serialising: only {cores} cores available, need >= 4 for headroom");
        return;
    }

    const N: usize = 4;
    const ITEMS: u64 = 150_000;

    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        ITEMS,
    );
    let engine = Arc::new(open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    ));

    let serial_sessions: Vec<_> = (0..N)
        .map(|_| engine.authorise(&full_coverage_credential()).unwrap())
        .collect();
    let serial_start = std::time::Instant::now();
    for session in &serial_sessions {
        engine
            .viewport(
                session,
                ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
            )
            .unwrap();
    }
    let serial = serial_start.elapsed();

    let concurrent_sessions: Vec<Arc<Session>> = (0..N)
        .map(|_| Arc::new(engine.authorise(&full_coverage_credential()).unwrap()))
        .collect();
    let barrier = Arc::new(std::sync::Barrier::new(N));
    let concurrent_start = std::time::Instant::now();
    let handles: Vec<_> = concurrent_sessions
        .into_iter()
        .map(|session| {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                engine
                    .viewport(
                        &session,
                        ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5),
                    )
                    .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let concurrent = concurrent_start.elapsed();

    println!(
        "distinct-key overlap ({cores} cores, N={N}): serial={serial:?} concurrent={concurrent:?}"
    );
    assert!(
        concurrent < serial * 7 / 10,
        "concurrent ({concurrent:?}) should be well under serial ({serial:?}) if distinct \
         sessions' first-viewport builds genuinely overlap rather than serialising behind one \
         lock (F4); generous 70% slack on a {cores}-core machine"
    );
}

/// Byte-format/wire behaviour is unchanged by the D-G refactor: a warm cache must serve output
/// identical to a cold one for the same request — the refactor changes when and how the
/// projection is built and read, never what a request is served.
#[test]
fn warm_row_projection_cache_serves_output_identical_to_cold() {
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
    let session = engine.authorise(&subset_credential()).unwrap();

    assert_eq!(engine.row_projection_cache_len(), 0, "nothing built yet");
    let cold = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 4, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .unwrap();
    assert_eq!(
        engine.row_projection_cache_len(),
        1,
        "the cold call must have published exactly one Ready slot"
    );

    let warm = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 4, [0.0, 0.0, 1000.0, 1000.0], 30),
        )
        .unwrap();
    assert_eq!(
        engine.row_projection_cache_len(),
        1,
        "a warm hit must not add a second slot"
    );

    assert_eq!(
        cold, warm,
        "warm-cache output must be byte-identical to cold (PartialEq ignores only `timings`)"
    );
}

/// **The F1 canary — repaired, having been briefly worthless.**
///
/// Origin: `docs/design-memos/2026-07-30-f1-selection-overdraw.md`. The retired placeholder sampler
/// asked `iter_range` for a tile's visible rows and kept the first `k`, and `iter_range` was eager —
/// so every visible row was copied into a `Vec<u32>` and all but `k` discarded, ~100 MB per request
/// at 10⁹. The memo asserted that waste as an equality and asked whoever fixed it to re-point the
/// test rather than delete it.
///
/// **Win 1 is done** — `rows_in_range` returns a bitmap and nothing is copied. But the memo's
/// suggested replacement (`<= tiles_nonempty * k`) would be wrong: §7.2's served set is not a
/// per-tile prefix of size `k`, and no *exact* evaluation of the threshold clause is O(k).
///
/// **And the first re-pointing was a tautology**, which is why this comment is long. It asserted
/// `select_rows_materialised == sigma_visible` while `viewport.rs` incremented *both* counters from
/// the same `visible` variable, twenty lines apart, with `select.rs` having no knowledge of the
/// probe at all. It could not fail for any behavioural change to selection. The counter is now
/// incremented by `Selection::of` inside the loops that read rows, so the comparison is an
/// observation against a reference rather than a variable against itself.
///
/// **The fixture must be partial-coverage.** Under a full-coverage credential
/// `sigma_visible == rows_in_ranges`, so "walked the mask" and "walked the raw row range" produce
/// identical counts and one of the two directions this test claims to guard is undetectable in
/// principle. The precondition below states that requirement rather than relying on it.
///
/// **What this does and does not own.** It pins the *implemented route*: direct evaluation reads
/// every visible row in a tile. It does **not** own I7 — that the served set is §7.2's and not a
/// row-order prefix is established by output-level tests in `tests/selection.rs` and by the Python
/// oracle, in default builds, at every zoom. This test runs only under `--features bench-timing`.
/// Design §7.2 admits exact routes visiting fewer than Σvisible rows (within a leaf Morton cell the
/// `tessera_id` column is sorted, so `C_θ` there is a binary search plus a range cardinality); if
/// one lands, revise this test alongside the differential oracle rather than deleting it.
#[cfg(feature = "bench-timing")]
#[test]
fn f1_selection_visits_exactly_the_visible_set() {
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
    // Partial coverage, deliberately: see the doc above. Every third item carries SUBSET_TERM.
    let session = engine.authorise(&subset_credential()).unwrap();

    const K: usize = 5;
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], K),
        )
        .unwrap();
    let t = out.timings;

    assert!(
        t.enabled,
        "built with bench-timing, so timings must be real"
    );
    assert_eq!(t.tiles_nonempty, 1, "zoom 0 is one tile");
    assert!(
        t.sigma_visible < t.rows_in_ranges,
        "this fixture must be partial-coverage — {} visible of {} spanned. At full coverage the \
         two are equal and the 'walked the raw range' direction below cannot be detected at all.",
        t.sigma_visible,
        t.rows_in_ranges
    );
    assert_eq!(
        t.points_gathered, K as u64,
        "the cap governs what is returned"
    );

    assert_eq!(
        t.select_rows_visited, t.sigma_visible,
        "selection visited {} rows against {} visible. Fewer means an early exit or a prefix \
         sample, which would evaluate §7.2's threshold clause over part of the tile. More means \
         iterating the raw row range rather than the mask. If an exact fast path lands (candidate \
         lists, a cached threshold bitmap), sub-Σvisible visits become legitimate — revise this \
         with the differential oracle rather than deleting it.",
        t.select_rows_visited, t.sigma_visible
    );
}

// ---------------------------------------------------------------------------------------------
// Concurrency — D-C cooperative cancellation (the rapid-pan case)
// ---------------------------------------------------------------------------------------------
//
// `Engine::viewport` checks a caller-supplied `CancelToken` at three points (see its own doc):
// once before `compose`, once before θ's anchor (`mask.visible_total()`), and once per tile at
// the top of the tile loop. A hit at any of these aborts the WHOLE request with
// `EngineError::Cancelled` — I13: no partial `ViewportOut` is ever constructed past that point.
//
// The first test below is fully deterministic: the token is flipped before the call is even
// made, so the outcome does not depend on scheduling at all. Genuinely interrupting a request
// *mid-flight* inherently needs a second thread racing the engine call, and the engine call
// itself is synchronous with no hook to pause it at a specific tile — adding one purely for a
// test is exactly what the design brief calls out as not worth it. The second test instead
// proves interruption indirectly and robustly: it compares the wall-clock time of a genuinely
// interrupted run against this same run's own baseline for the full (uncancelled) sweep,
// following the self-scaling wall-clock-ratio pattern this file and `tessera-server`'s test suite
// already use elsewhere (e.g. `distinct_key_first_viewports_overlap_instead_of_serialising`
// above) rather than a fixed wall-clock bet. Which exact checkpoint caught the cancellation is
// left to code review of the call sites above; both tests only assert the externally-observable
// contract (whole-request abort, `Cancelled`, no partial output, and — for the second test —
// abandoned well before the full sweep would have finished).

/// D-C: a config wide enough to let a many-tile, high-fan-out §3.3 underlay request through
/// `Engine::viewport`'s own bounds checks — used only by the timing test below to engineer a
/// multi-tile sweep long enough to interrupt mid-flight. Same cost-model trick
/// `tessera-server`'s own slow-viewport test fixtures use: each sub-cell costs one small binary
/// search plus one bitmap range-count, independent of corpus size, so slowness is engineered via
/// fan-out rather than growing `N_ITEMS`.
fn config_for_slow_multi_tile_sweep() -> EngineConfig {
    EngineConfig {
        max_underlay_offset: 8,
        max_underlay_cells: 20_000_000,
        max_tiles_per_request: 262_144,
        ..config()
    }
}

/// D-C, I13: a token cancelled before the call is even made aborts the whole request with
/// `Cancelled` specifically — not swallowed into some other error arm, and (since the call
/// returns `Err`) no `ViewportOut`, partial or otherwise, is ever constructed.
#[test]
fn pre_flipped_cancel_token_aborts_immediately_with_no_partial_output() {
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

    let cancel = CancelToken::new();
    cancel.cancel();

    let result = engine.viewport(
        &session,
        ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5).cancel(Some(cancel)),
    );

    assert!(
        matches!(result, Err(EngineError::Cancelled)),
        "a pre-flipped token must abort the whole request with Cancelled, got {result:?}"
    );
}

/// D-C: a request with no `cancel` set at all behaves exactly as before this task — every other
/// test in this file already exercises that path implicitly, but this makes the "opt-in, zero
/// effect otherwise" claim an explicit assertion rather than an inference from the rest of the
/// suite staying green.
#[test]
fn absent_cancel_token_never_aborts() {
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
    assert_eq!(out.tiles.len(), 1);
}

/// D-C: cancellation flipped from another thread while a genuinely multi-tile, multi-millisecond
/// sweep is running aborts it well before the full sweep would have completed.
///
/// **What this test does NOT claim.** It does not assert, and cannot reliably force, which of the
/// three checkpoints (pre-compose, pre-theta-anchor, top-of-tile-loop) catches the flip. The
/// canceller thread's entire job is one atomic store released from a `Barrier`, and that store can
/// land anywhere relative to the engine call's own progress — including before the engine call has
/// even reached `compose`. Warming the cancelled run's session (a cheap request first, building its
/// row projection) removes the one genuinely slow thing that could precede the checkpoints and so
/// makes it *more likely* the flip lands during the tile loop rather than before it, but this is a
/// bias, not a guarantee, and the assertions below hold either way — `cancelled_elapsed` is small
/// whether the flip is caught pre-compose or mid-sweep. Proving the per-tile check specifically
/// exists and is correctly placed is left to code review of `Engine::viewport`'s call sites, which
/// the D-C design brief explicitly sanctions ("a unit-level check that the per-tile check exists
/// ... is NOT worth adding API for ... rely on code review for the per-tile placement").
///
/// **Self-scaling, not a sleep-based guess.** `baseline_elapsed` is this run's own measured time
/// for the full, uncancelled 16-tile sweep (`zoom = 2`, `underlay_offset = 8` — 4^8 = 65536
/// sub-cell evaluations per tile, ~1.05M total; measured at ~270ms in this task's tuning run,
/// comfortably above the floor asserted below). `cancelled_elapsed` should be a small fraction of
/// `baseline_elapsed` regardless of which checkpoint caught it (measured at ~4ms cancelled against
/// ~270ms baseline in this task's tuning run — comfortably inside the /2 bound asserted below, with
/// wide margin to spare).
#[test]
fn cancel_flipped_from_another_thread_aborts_a_long_request_before_it_completes() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = Arc::new(open_engine_with(
        &bundle_root,
        tmp.path(),
        config_for_slow_multi_tile_sweep(),
    ));

    let request =
        || ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], 1).underlay_offset(Some(8));

    // Baseline: an uncancelled full sweep over a fresh session, so `baseline_elapsed` reflects
    // this machine's real speed for the whole 16-tile workload (cold row-projection build
    // included, exactly like the cancelled run below before its own warm-up).
    let baseline_session = engine.authorise(&full_coverage_credential()).unwrap();
    let baseline_start = std::time::Instant::now();
    engine.viewport(&baseline_session, request()).unwrap();
    let baseline_elapsed = baseline_start.elapsed();
    assert!(
        baseline_elapsed > std::time::Duration::from_millis(20),
        "the uncancelled sweep finished in {baseline_elapsed:?}, too fast to exercise this \
         test's interruption scenario -- widen the underlay offset or the tile count"
    );

    // A fresh session for the cancelled run — a fresh `token_id`, so a fresh row-projection cache
    // key, deliberately warmed below (unlike the baseline session above) so the only slow work
    // left ahead of the checkpoints is the tile loop itself. See this test's doc for why this only
    // biases which checkpoint catches the flip rather than guaranteeing it lands mid-loop.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1),
        )
        .unwrap();

    let cancel = CancelToken::new();
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let canceller_barrier = Arc::clone(&barrier);
    let canceller_cancel = cancel.clone();
    let canceller = std::thread::spawn(move || {
        canceller_barrier.wait();
        canceller_cancel.cancel();
    });

    barrier.wait();
    let cancelled_start = std::time::Instant::now();
    let result = engine.viewport(&session, request().cancel(Some(cancel)));
    let cancelled_elapsed = cancelled_start.elapsed();
    canceller.join().unwrap();

    assert!(
        matches!(result, Err(EngineError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        cancelled_elapsed < baseline_elapsed / 2,
        "the cancelled run took {cancelled_elapsed:?}, not meaningfully less than the \
         uncancelled sweep's {baseline_elapsed:?} -- cancellation does not appear to interrupt an \
         in-flight multi-tile sweep"
    );
}

// ---------------------------------------------------------------------------------------------
// Concurrency — D-D/D-F intra-request rayon parallelism (Task 6)
// ---------------------------------------------------------------------------------------------

/// THE HEADLINE TEST (D-D/D-F): the same fixture and the same request produce a byte-for-byte
/// identical `ViewportOut` (`PartialEq` ignores only `timings` — see its hand-written impl)
/// whether the engine's shared pool has one worker or eight.
///
/// This is the whole point of D-F's collect shape — `self.pool.install(|| tiles.par_iter().zip
/// (..).with_min_len(..).map(tile_result).collect::<Vec<Result<Option<TileResult>>>>())`, never
/// `Result<Vec<TileResult>>` (see `viewport.rs`'s module doc) — the parallel sweep's output order
/// equals the input tiles' order **by construction** (rayon's indexed collect path), so the serial
/// fold's `tile_counts`/`points`/`sub_cells` concatenation is identical regardless of how many
/// workers ran the sweep or in which order they happened to finish.
///
/// A multi-tile request (`zoom = 3`, full bbox — 64 tiles, most non-empty over this fixture's
/// `(e*37, e*53) % 1000` scatter across `PARALLEL_HEADLINE_ITEMS = 300,000` items) with an
/// underlay requested too, so every per-tile code path this task touched (count, select — both
/// the serve-all and the heap/threshold branch, since θ is saturated but many tiles exceed the
/// `k = 50` cap — gather, underlay) runs across more than one tile.
///
/// **Calibration task fix-wave note.** This test used the file's default `N_ITEMS = 10,000`
/// fixture until the serial-fallback threshold (`SERIAL_FALLBACK_MAX_ROWS`, `viewport.rs`) landed
/// below it — at 10,000 items this request's `Σ range.len()` cannot reach the 200,000-row
/// threshold, so `compute_threads = 1` and `= 8` would both silently take the SAME serial-fold
/// branch and the comparison below would no longer test what its own doc claims (fan-out
/// invariance), only that the serial path is deterministic, which was never in question.
/// `PARALLEL_HEADLINE_ITEMS` (300,000, comfortable margin above the threshold) restores that: see
/// its own doc for why the fixture's fixed-extent scatter makes `Σ range.len() == n` exactly for
/// a full-extent request, and the `rows_in_ranges` assertion below for the belt-and-braces runtime
/// check under `bench-timing`.
///
/// **What this does not (and cannot) test.** It says nothing about the Python differential oracle
/// or the conformance byte-scanner directly — those consume `ViewportOut`/the wire bytes exactly
/// as any other test does, and know nothing about `compute_threads`. The claim this test backs is
/// narrower and sufficient: the engine's own output is invariant in that knob, so anything the
/// oracle or the scanner already assert about a `compute_threads = 1` response continues to hold
/// verbatim at any other value — the oracle and the conformance vectors are unaffected because
/// there is nothing in this response for them to disagree about.
#[test]
fn viewport_output_is_byte_identical_at_compute_threads_1_and_8() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    // Separate cache/WAL directories per engine (same read-only bundle) — two independent
    // `Engine::open`s over the same bundle, differing only in `compute_threads`. `open_engine_with`
    // joins `cache`/`wal.log` onto the directory it is given, and `Wal::open` does not create that
    // directory itself (unlike `tmp.path()`, which `TempDir::new` already created), so each must
    // be made first.
    let dir_1 = tmp.path().join("a");
    let dir_8 = tmp.path().join("b");
    std::fs::create_dir_all(&dir_1).unwrap();
    std::fs::create_dir_all(&dir_8).unwrap();
    let engine_1 = open_engine_with(
        &bundle_root,
        &dir_1,
        EngineConfig {
            compute_threads: 1,
            ..config()
        },
    );
    let engine_8 = open_engine_with(
        &bundle_root,
        &dir_8,
        EngineConfig {
            compute_threads: 8,
            ..config()
        },
    );

    let session_1 = engine_1.authorise(&full_coverage_credential()).unwrap();
    let session_8 = engine_8.authorise(&full_coverage_credential()).unwrap();

    let request =
        || ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], 50).underlay_offset(Some(2));

    let out_1 = engine_1.viewport(&session_1, request()).unwrap();
    let out_8 = engine_8.viewport(&session_8, request()).unwrap();

    assert!(
        out_1.tiles.len() > 1,
        "need more than one non-empty tile to exercise cross-tile ordering, got {}",
        out_1.tiles.len()
    );
    assert!(
        !out_1.sub_cells.is_empty(),
        "the underlay request must produce some sub-cells for this test to cover that path too"
    );
    // Belt-and-braces alongside the fixture-size argument above (this only runs under
    // `bench-timing`, since `StageTimings` is all-zero without it — `enabled` says which):
    // directly confirm `engine_8`'s run crossed `SERIAL_FALLBACK_MAX_ROWS` and therefore actually
    // took the `pool.install` branch rather than degrading to serial-vs-serial.
    if out_8.timings.enabled {
        assert!(
            out_8.timings.rows_in_ranges >= SERIAL_FALLBACK_MAX_ROWS,
            "rows_in_ranges = {} did not clear the serial-fallback threshold ({}) -- this test \
             would silently be comparing serial against serial, not exercising the parallel \
             fan-out its own doc claims to cover",
            out_8.timings.rows_in_ranges,
            SERIAL_FALLBACK_MAX_ROWS
        );
    }

    assert_eq!(
        out_1, out_8,
        "ViewportOut must be byte-for-byte identical (PartialEq ignores only `timings`) \
         regardless of compute_threads"
    );
}

/// A sparse/skewed-bbox variant of the headline test above (fix-wave minor: the headline fixture's
/// zoom = 3 request is "most non-empty over this fixture's scatter", so `tile_result`'s
/// `visible == 0 -> Ok(None)` empty-tile skip path — a real branch inside the parallel sweep, since
/// an empty tile contributes nothing to `tile_counts`/`points`/`sub_cells` in the serial fold — was
/// never exercised by a byte-equality assertion).
///
/// **Why zoom = 8 over the same fixture, no new fixture data.** The fixture's scatter
/// (`x = (e*37) % 1000, y = (e*53) % 1000`) is a bijection of `e % 1000` onto the 1000×1000 residue
/// lattice, repeated every 1,000-item cycle — so `n` items occupy only 1,000 distinct locations
/// (each hit `n / 1000` times), not `n` of them. At `zoom = 3` (64 candidate tiles) that is dense
/// enough to leave almost every tile non-empty; at `zoom = 8` (up to 65,536 candidate tiles over
/// the full extent) it is over 65 empty candidate cells per occupied one on average, so most tiles
/// are genuinely empty while a real minority are not — the mix this test needs, produced by
/// changing only the requested zoom, not by hand-building a new sparse corpus.
///
/// **Calibration task fix-wave note.** Same reasoning as the headline test above:
/// `PARALLEL_HEADLINE_ITEMS` (300,000) replaces the file's default `N_ITEMS` (10,000) so `Σ
/// range.len()` clears `SERIAL_FALLBACK_MAX_ROWS` and `compute_threads = 1` vs `= 8` are
/// genuinely comparing serial against parallel, not serial against serial. The occupied/empty
/// tile MIX this test is actually for is unaffected by the item count (still 1,000 distinct
/// locations either way, just more items stacked on each) — see the doc above.
#[test]
fn viewport_output_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    let dir_1 = tmp.path().join("a");
    let dir_8 = tmp.path().join("b");
    std::fs::create_dir_all(&dir_1).unwrap();
    std::fs::create_dir_all(&dir_8).unwrap();
    let engine_1 = open_engine_with(
        &bundle_root,
        &dir_1,
        EngineConfig {
            compute_threads: 1,
            ..config()
        },
    );
    let engine_8 = open_engine_with(
        &bundle_root,
        &dir_8,
        EngineConfig {
            compute_threads: 8,
            ..config()
        },
    );

    let session_1 = engine_1.authorise(&full_coverage_credential()).unwrap();
    let session_8 = engine_8.authorise(&full_coverage_credential()).unwrap();

    let bbox = [0.0, 0.0, 1000.0, 1000.0];
    let zoom = 8;
    let request = || ViewportRequest::new("s0", zoom, bbox, 50);
    let candidate_tiles = tiles_for_bbox(bbox, zoom, &extent()).len();

    let out_1 = engine_1.viewport(&session_1, request()).unwrap();
    let out_8 = engine_8.viewport(&session_8, request()).unwrap();

    assert!(
        !out_1.tiles.is_empty(),
        "need at least one non-empty tile for this to be a real mixed case, got none"
    );
    assert!(
        out_1.tiles.len() < candidate_tiles,
        "need at least one genuinely empty (Ok(None)-skipped) tile among the {candidate_tiles} \
         candidates to exercise the skip path this test is for -- got {} non-empty tiles, meaning \
         none were skipped",
        out_1.tiles.len()
    );
    // Belt-and-braces, same as the headline test above.
    if out_8.timings.enabled {
        assert!(
            out_8.timings.rows_in_ranges >= SERIAL_FALLBACK_MAX_ROWS,
            "rows_in_ranges = {} did not clear the serial-fallback threshold ({}) -- this test \
             would silently be comparing serial against serial",
            out_8.timings.rows_in_ranges,
            SERIAL_FALLBACK_MAX_ROWS
        );
    }

    assert_eq!(
        out_1, out_8,
        "ViewportOut must be byte-for-byte identical (PartialEq ignores only `timings`) \
         regardless of compute_threads, including on the mostly-empty-tile Ok(None) skip path"
    );
}

/// The calibration task's own below-threshold companion to the two headline tests above: at the
/// file's default `N_ITEMS = 10,000` (well under `SERIAL_FALLBACK_MAX_ROWS`), `compute_threads =
/// 1` and `= 8` both take the SERIAL fold branch, never `pool.install` — so this is not "does the
/// fan-out preserve order" (the headline tests' claim) but "does the new branch exist at all and
/// still produce byte-identical output regardless of the pool a request never enters" (trivially
/// true by construction, since neither run touches `self.pool` — asserted rather than assumed,
/// per the guard-rail's own preference for behavioural coverage over trusting the diff by eye).
#[test]
fn viewport_output_is_byte_identical_at_compute_threads_1_and_8_below_the_serial_fallback_threshold(
) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let dir_1 = tmp.path().join("a");
    let dir_8 = tmp.path().join("b");
    std::fs::create_dir_all(&dir_1).unwrap();
    std::fs::create_dir_all(&dir_8).unwrap();
    let engine_1 = open_engine_with(
        &bundle_root,
        &dir_1,
        EngineConfig {
            compute_threads: 1,
            ..config()
        },
    );
    let engine_8 = open_engine_with(
        &bundle_root,
        &dir_8,
        EngineConfig {
            compute_threads: 8,
            ..config()
        },
    );

    let session_1 = engine_1.authorise(&full_coverage_credential()).unwrap();
    let session_8 = engine_8.authorise(&full_coverage_credential()).unwrap();

    // Same request shape as the headline test (multi-tile, underlay) — only the fixture size
    // differs, which is the whole point of this variant.
    let request =
        || ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], 50).underlay_offset(Some(2));

    let out_1 = engine_1.viewport(&session_1, request()).unwrap();
    let out_8 = engine_8.viewport(&session_8, request()).unwrap();

    assert!(out_1.tiles.len() > 1, "need more than one non-empty tile");
    if out_8.timings.enabled {
        assert!(
            out_8.timings.rows_in_ranges < SERIAL_FALLBACK_MAX_ROWS,
            "rows_in_ranges = {} unexpectedly cleared the serial-fallback threshold ({}) at \
             N_ITEMS = {N_ITEMS} -- this test's whole premise (both configs take the serial \
             branch) no longer holds",
            out_8.timings.rows_in_ranges,
            SERIAL_FALLBACK_MAX_ROWS
        );
    }

    assert_eq!(
        out_1, out_8,
        "ViewportOut must be byte-for-byte identical regardless of compute_threads, including \
         below the serial-fallback threshold where neither run touches the pool"
    );
}
