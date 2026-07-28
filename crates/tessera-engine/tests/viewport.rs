//! Task 11, Step 1: the masked viewport query, end to end over a ~10k-item synthetic bundle
//! built through `tessera-build`'s library API (Task 8's smoke-test pattern).
//!
//! Every item carries `ALL_TERM` ("0"); every third item (`source_id % 3 == 0`) additionally
//! carries `SUBSET_TERM` ("1"). At `zoom = 0`, `tiles_for_bbox` always returns exactly one tile
//! covering the whole grid regardless of `bbox` (depth 0 has no bits to discriminate on), which
//! makes a brute-force oracle over the *input* relation trivial: no bbox-intersection geometry to
//! reproduce, just term membership.

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
use tessera_engine::{Engine, EngineConfig, EngineError};
use tessera_lifecycle::wal::{ChangeOp, Wal, WalRecord};
use tessera_plugin::Passthrough;
use tessera_spatial::Extent;
use tessera_store::read::open_bundle;
use tessera_types::PinId;

const N_ITEMS: u64 = 10_000;
const ALL_TERM: u64 = 0;
const SUBSET_TERM: u64 = 1;

fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
    }
}

/// A config whose `max_k` is large enough to never cap sampling — used by tests asserting
/// membership (counts, exact point sets) rather than the `max_k` cap itself.
fn config_uncapped() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: N_ITEMS as usize,
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
        let ent = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
            map.insert(source, ent.value(i));
        }
    }
    map
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
        .viewport(&session, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5, None)
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
            "s0",
            0,
            [0.0, 0.0, 1000.0, 1000.0],
            N_ITEMS as usize,
            None,
        )
        .unwrap();

    assert_eq!(out.tiles.len(), 1);
    assert_eq!(out.tiles[0].visible, expected);
    assert_eq!(out.tiles[0].matched, expected);
    assert_eq!(out.points.len(), expected as usize);

    // Every sampled point must actually be a subset-term source item.
    let source_to_new = source_to_new_map(&bundle_root, "v00000");
    let new_to_source: BTreeMap<u64, u64> = source_to_new.iter().map(|(&s, &n)| (n, s)).collect();
    for point in &out.points {
        let source = new_to_source[&point.entity_id.raw()];
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
        .viewport(&session, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 30, None)
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
        .viewport(&session_a, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1, None)
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
        .viewport(&session_b, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 1, None)
        .unwrap();

    assert_eq!(
        out_b.tiles[0].visible,
        out_a.tiles[0].visible - 1,
        "suppressing one item must drop the count by exactly one"
    );
    assert_eq!(out_b.tiles[0].matched, out_b.tiles[0].visible);
}

/// (f) The placeholder sampler returns the **first** `k` visible row IDs in row (Morton) order —
/// exact row ids asserted directly against the segment's own on-disk row order, proving this is
/// the naive placeholder described in the module doc, not a priority sample.
#[test]
fn f_sampler_returns_first_k_in_row_order() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let bundle = open_bundle(&bundle_root).unwrap();
    let segment = &bundle.partitions["default"].slices["s0"].segments[0];
    let expected_entity_ids: Vec<u64> = segment.columns.entity_id()[0..3].to_vec();
    let expected_xs: Vec<f32> = segment.columns.x()[0..3].to_vec();
    let expected_ys: Vec<f32> = segment.columns.y()[0..3].to_vec();

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(&session, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 3, None)
        .unwrap();

    assert_eq!(out.points.len(), 3);
    for (i, point) in out.points.iter().enumerate() {
        assert_eq!(
            point.entity_id.raw(),
            expected_entity_ids[i],
            "point {i} is not the placeholder's expected first-k row"
        );
        assert_eq!(point.x, expected_xs[i]);
        assert_eq!(point.y, expected_ys[i]);
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
        .viewport(&session, "s0", 0, [0.0, 0.0, 1000.0, 1000.0], 5, None)
        .unwrap();

    let again = engine
        .viewport(
            &session,
            "s0",
            0,
            [0.0, 0.0, 1000.0, 1000.0],
            5,
            Some(first.pin.clone()),
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
            "s0",
            0,
            [0.0, 0.0, 1000.0, 1000.0],
            5,
            Some(stale_pin),
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
            max_k: 200,
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
            .viewport(&session, "s0", zoom, [x0, y0, x1, y1], 30, None)
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
