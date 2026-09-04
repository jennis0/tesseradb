//! **The same coordinate placed by a build and by an ingest lands in the same cell.**
//!
//! Building a database and adding rows to one are the same operation ([decision
//! 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)), so the two
//! routes have to agree about where a point is. They quantise in different crates —
//! `tessera_build::input` at the read of a points file, `tessera_store::flush` at the flush of a
//! buffered row — and the only thing keeping them in step is that both are handed the same value
//! at the same width. A narrowing on one side and not the other is invisible in every count, every
//! digest and every type signature, and shows up only as a point in the wrong cell.
//!
//! The frame here is 1/4096 of a 65,536-unit coordinate range, at the far end of that range —
//! zoom offset 12, where one `f32` step spans sixteen cells (`projections.md` §6). The fixture
//! carries a pair of positions closer together than one `f32` step, so the run is not merely
//! consistent: it is consistent at a depth where an `f32` path on either side would put both
//! points in one cell and disagree with the other side about the rest.

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::UnallocatedRow;
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_types::EntityId;

/// A frame 16 units wide out of a 65,536-unit coordinate range — 1/4096 of it — placed where the
/// coordinates are largest and an `f32`'s step is therefore coarsest. One cell is 2.44 × 10⁻⁴
/// wide here; one `f32` step is 3.9 × 10⁻³, sixteen of them.
fn frame() -> Bounds {
    Bounds {
        x_min: 65_504.0,
        x_max: 65_520.0,
        y_min: 65_504.0,
        y_max: 65_520.0,
    }
}

/// The fixture's positions, in source order.
///
/// The first two are 10⁻³ apart — closer than one `f32` step at this frame, and four cells apart
/// at `f64`. The rest spread across the frame so the agreement below is over a set rather than
/// over one lucky value.
fn fixture_points() -> Vec<(f64, f64)> {
    let mut points = vec![(65_508.5, 65_508.5), (65_508.501, 65_508.501)];
    for i in 0..14u64 {
        points.push((65_504.25 + i as f64 * 1.0625, 65_519.75 - i as f64 * 1.0625));
    }
    points
}

fn write_points(path: &Path, points: &[(f64, f64)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from(
                (0..points.len() as u64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                points.iter().map(|p| p.0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                points.iter().map(|p| p.1).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("the fixture batch is well-formed");
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// Every item carries `ALL_TERM`, so the credential below reaches all of them.
fn write_pairs(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from((0..n).collect::<Vec<_>>())),
            Arc::new(UInt32Array::from(vec![ALL_TERM as u32; n as usize])),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The fixture bundle: the frame is this file's own, so `common::build_fixture_n` cannot serve —
/// its extent is the shared `[0, 1000]` square and the depth is the whole point here.
fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path, points: &[(f64, f64)]) {
    write_points(points_path, points);
    write_pairs(pairs_path, points.len() as u64);
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: frame(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("the deep-frame fixture builds");
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Every served point's 64-bit position, by `tessera_id`.
fn served_positions(engine: &Engine) -> BTreeMap<u64, u64> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let f = frame();
    let request = ViewportRequest::new(
        "s0",
        0,
        [f.x_min, f.y_min, f.x_max, f.y_max],
        N_ITEMS as usize,
    );
    let out = engine
        .viewport(&session, request)
        .expect("the viewport answers");
    out.points
        .iter()
        .map(|(id, code)| (id.raw(), code))
        .collect()
}

/// **The property.** The same sixteen coordinates, reached once through a build and once through
/// an ingest, occupy the same cell — and, since nothing between the two routes rounds differently
/// either, the same sub-cell position.
///
/// The two routes are genuinely different code: the built rows were quantised by
/// `tessera_build::input` against the frame in `MANIFEST.quantisation`, and the ingested rows by
/// `tessera_store::flush` against the same field, having travelled through an `UnallocatedRow`, a
/// WAL record and the ingest buffer on the way. Agreement is asserted on the *positions*, because
/// counts agree under a narrowing that moves every point.
#[test]
fn a_build_and_an_ingest_place_one_coordinate_in_one_cell() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = fixture_points();
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        &points,
    );

    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            // Nothing here is about the tick: the flush is asked for explicitly.
            flush_max_age_secs: 3600,
            ..config_uncapped()
        },
    )
    .expect("the engine opens against the deep-frame bundle");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");

    let rows: Vec<UnallocatedRow> = points
        .iter()
        .enumerate()
        .map(|(i, (x, y))| UnallocatedRow {
            external_id: Some(format!("ingested-{i}").into_bytes()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![ALL_TERM.to_string().into_bytes()],
            x: *x,
            y: *y,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[ALL_TERM.to_string().into_bytes()]),
            scoped: Vec::new(),
        })
        .collect();
    let ingested: Vec<EntityId> = engine
        .accept_ingest(rows, "batch-1".to_string(), [7u8; 32])
        .expect("the batch is accepted");
    assert_eq!(ingested.len(), points.len());
    flush(&engine);

    let built = source_to_new_map(&root, "v00000");
    let positions = served_positions(&engine);
    assert_eq!(
        positions.len(),
        points.len() * 2,
        "both copies of every point must be served, or the comparison below is vacuous"
    );

    let position_of = |entity: EntityId| -> u64 {
        let id = engine.tessera_id_of(entity).expect("a live entity has one");
        *positions
            .get(&id.raw())
            .unwrap_or_else(|| panic!("entity {entity:?} was not served"))
    };
    let mut cells = Vec::new();
    for (source, (x, y)) in points.iter().enumerate() {
        let from_build = position_of(EntityId::new(built[&(source as u64)]));
        let from_ingest = position_of(ingested[source]);
        assert_eq!(
            from_build, from_ingest,
            "({x}, {y}) was placed differently by the build and the ingest"
        );
        cells.push(from_build >> 32);
    }

    // The frame is deep enough for the assertion above to mean something: the first two positions
    // are inside one `f32` step of each other and still occupy different cells, so an `f32` path
    // on either route would have put them together and disagreed with the other route.
    let (a, b) = (points[0].0, points[1].0);
    assert_eq!(a as f32, b as f32, "the fixture pair collapses under f32");
    assert_ne!(
        cells[0], cells[1],
        "the fixture pair must occupy different cells at f64, or the depth proves nothing"
    );
}
