//! The band route serves exactly what the shipped scan serves (`tessera_engine::bands`).
//!
//! Every check below runs one request twice on one engine and one generation: answered from the
//! identity bands at every zoom, and answered by the scan alone, the reference. The two responses
//! must be equal in every tile's counts, every served `tessera_id` in its order, every position and
//! every render value with its presence. Principals see the whole corpus, a third of it, a fiftieth
//! of it and one item; the corpus is checked as built, with an ingest flushed beside it, with
//! deletions and suppressions applied, after a fold and after a restart.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{BooleanArray, Float64Array, Int32Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::{build, BuildArgs};
use tessera_engine::bands::BANDS_BELOW_ZOOM;
use tessera_engine::viewport::{PointRows, ViewportRequest};
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::UnallocatedRow;
use tessera_plugin::Passthrough;
use tessera_spatial::tiler::ScalarType;
use tessera_types::EntityId;

/// Enough rows that the bands answer tiles down to zoom 3 under [`config`]'s threshold.
const ROWS: u64 = 60_000;
/// The one item the single-item principal sees.
const LONE: u64 = 4_242;
/// Every zoom the checks request: the bands answer the shallow ones and decline the deep ones.
const ZOOMS: std::ops::RangeInclusive<u8> = 0..=7;

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A position in the fixture's extent, clustered so that tiles range from dense to nearly empty.
fn position(e: u64) -> (f64, f64) {
    let h = mix(e);
    let spread = if e % 4 == 0 { 1000.0 } else { 120.0 };
    let x = (h % 1_000_000) as f64 / 1_000_000.0 * spread;
    let y = ((h >> 20) % 1_000_000) as f64 / 1_000_000.0 * spread;
    (x, y)
}

/// Every item carries "0"; every third "1"; every fiftieth "2"; [`LONE`] alone "3".
fn terms(e: u64) -> Vec<u32> {
    let mut t = vec![0];
    if e % 3 == 0 {
        t.push(1);
    }
    if e % 50 == 0 {
        t.push(2);
    }
    if e == LONE {
        t.push(3);
    }
    t
}

fn build_fixture(root: &Path) {
    let dir = root.parent().unwrap();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Int32, true),
        Field::new("flag", DataType::Boolean, false),
        Field::new("weight", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..ROWS).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| position(e).0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| position(e).1).collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                ids.iter()
                    .map(|&e| (e % 7 != 0).then_some(e as i32 - 30_000))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                ids.iter().map(|&e| e % 5 == 1).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| e as f64 / 8.0).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let pair_schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let (mut entities, mut term_ids) = (Vec::new(), Vec::new());
    for e in 0..ROWS {
        for t in terms(e) {
            entities.push(e);
            term_ids.push(t);
        }
    }
    let batch = RecordBatch::try_new(
        pair_schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(term_ids)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&pairs).unwrap(), pair_schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let mut schema = id_schema();
    for (name, ty) in [
        ("score", ScalarType::I32),
        ("flag", ScalarType::Bool),
        ("weight", ScalarType::F64),
    ] {
        schema.attributes.push(tessera_build::config::Attribute {
            field: None,
            name: name.to_string(),
            title: None,
            ty,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index: false,
            render: true,
            unique: false,
        });
    }
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: root.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("the fixture builds");
}

/// A live threshold, so that tiles are thinned by the cut, held up by the floor and stopped by the
/// cap, rather than the saturated one the shared config sets.
fn config() -> EngineConfig {
    EngineConfig {
        theta_target_marks: 4,
        k_min: 2,
        max_k: 200,
        k_max_marks: 200,
        ..common::config()
    }
}

fn open(root: &Path, tmp: &Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        config(),
    )
    .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

fn credential(term: &str) -> Vec<u8> {
    format!("{{\"terms\": [\"{term}\"]}}").into_bytes()
}

/// What one principal was served over every zoom and request shape: how many non-empty tiles, how
/// many of them the bands answered, and how many points.
#[derive(Default, Debug)]
struct Served {
    tiles: u64,
    from_bands: u64,
    points: usize,
}

/// Every request shape for `term`'s principal, answered from the bands and by the scan, compared.
fn check(engine: &Engine, term: &str) -> Served {
    let session = engine.authorise(&credential(term)).unwrap();
    let mut served = Served::default();
    let columns = ["flag".to_string()];
    for zoom in ZOOMS {
        for (k, point_rows, underlay) in [
            (200, PointRows::Full, None),
            (3, PointRows::Full, None),
            (200, PointRows::Columns(&columns), (zoom <= 4).then_some(2)),
        ] {
            let request = || {
                let mut req = ViewportRequest::new("s0", zoom, WHOLE_MAP, k);
                req.point_rows = point_rows;
                req.underlay_offset = underlay;
                req
            };
            engine.set_bands_below_zoom_for_test(17);
            let bands = engine.viewport(&session, request()).unwrap();
            engine.set_bands_below_zoom_for_test(0);
            let scan = engine.viewport(&session, request()).unwrap();
            engine.set_bands_below_zoom_for_test(BANDS_BELOW_ZOOM);
            assert_eq!(
                scan.timings.tiles_from_bands, 0,
                "the reference is the scan alone"
            );
            assert_eq!(
                bands, scan,
                "principal {term}, zoom {zoom}, k {k}: the band route serves what the scan serves"
            );
            served.tiles += bands.timings.tiles_nonempty;
            served.from_bands += bands.timings.tiles_from_bands;
            served.points += bands.points.tessera_ids.len();
        }
    }
    served
}

/// Every principal, checked, with the bands answering some tiles of the wide ones and the column
/// answering some tiles of the narrow ones.
fn check_all(engine: &Engine) {
    let broad = check(engine, "0");
    let medium = check(engine, "1");
    let narrow = check(engine, "2");
    let lone = check(engine, "3");
    for (who, s) in [("broad", &broad), ("medium", &medium)] {
        assert!(s.from_bands > 0, "{who}: the bands answered no tile: {s:?}");
    }
    assert!(
        narrow.from_bands < narrow.tiles,
        "narrow: some tile fell to the column: {narrow:?}"
    );
    assert_eq!(
        lone.points,
        ZOOMS.count() * 3,
        "the one visible item is served in every request: {lone:?}"
    );
}

fn entity_of(map: &std::collections::BTreeMap<u64, u64>, source: u64) -> EntityId {
    EntityId::new(map[&source])
}

#[test]
fn the_band_route_serves_what_the_scan_serves_through_writes_a_fold_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(&root);
    let map = source_to_new_map(&root, "v00000");

    let engine = open(&root, tmp.path());
    check_all(&engine);

    // An ingest flushed into a second segment, its items inside the dense cluster and out of it.
    let mut rows: Vec<UnallocatedRow> = (0..6_000u64)
        .map(|i| {
            let (x, y) = position(ROWS + i);
            let descriptors = if i % 3 == 0 {
                vec![b"0".to_vec(), b"1".to_vec()]
            } else {
                vec![b"0".to_vec()]
            };
            UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors,
                x,
                y,
                scalars: vec![
                    WalScalar::U64(key_id(&format!("new-{i}"))),
                    if i % 4 == 0 {
                        WalScalar::Null
                    } else {
                        WalScalar::I32(i as i32)
                    },
                    WalScalar::Bool(i % 2 == 0),
                    WalScalar::F64(-(i as f64)),
                ],
                terms: Vec::new(),
                scoped: Vec::new(),
            }
        })
        .collect();
    for row in &mut rows {
        row.terms = engine.resolve_terms(&row.descriptors);
    }
    engine
        .ingest_rows(rows, "band-route-1".to_string(), [1u8; 32])
        .expect("the ingest is accepted");
    publish_buffered(&engine);
    assert_eq!(segments(&engine), 2, "the flush wrote a second segment");
    check_all(&engine);

    // Deletions and suppressions, applied through the mask: the denied rows stay in the bands.
    let mut changes: Vec<(EntityId, ChangeOp)> = (0..ROWS)
        .filter(|e| e % 3 == 0 && e % 11 == 0)
        .map(|e| (entity_of(&map, e), ChangeOp::Delete))
        .collect();
    changes.extend(
        (0..ROWS)
            .filter(|e| e % 13 == 0 && *e != LONE)
            .map(|e| (entity_of(&map, e), ChangeOp::Suppress)),
    );
    engine.accept_changes(changes).expect("the changes are accepted");
    tick(&engine);
    check_all(&engine);

    fold(&engine);
    assert_eq!(segments(&engine), 1, "the fold writes one segment");
    check_all(&engine);
    drop(engine);

    let engine = open(&root, tmp.path());
    check_all(&engine);
}

fn segments(engine: &Engine) -> usize {
    engine
        .generation()
        .bundle
        .partitions
        .values()
        .map(|p| p.views["s0"].segments.len())
        .sum()
}
