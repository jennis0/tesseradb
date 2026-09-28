//! Counting a viewer's rows by map cell, over a view of two segments: the build's and a flush's.
//!
//! Every table the pass produces, at every depth from 0 to 32, with and without groups, over a
//! composed mask and over a bitmap of rows, is checked against one computed row by row from each
//! row's stored position and drawn code. Range counts, where they apply, must give the same cells.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use common::*;
use croaring::Bitmap;
use parquet::arrow::ArrowWriter;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::cells::{
    count_by_ranges, pass, pass_in_chunks, CellCount, CellSet, GroupTable, RowGroups,
};
use tessera_engine::compose::MaskedSet;
use tessera_engine::viewport::segments_with_row_bases;
use tessera_engine::Engine;
use tessera_lifecycle::{UnallocatedRow, WalScalar};
use tessera_store::read::{ScalarSlice, SegmentData};

const N: u64 = 5_000;

/// A drawn and indexed category whose keys the build and the ingest mint.
const SCHEMA: &str = r#"
[[vocabulary]]
name       = "kind"
width      = "u16"
value_set  = "open"
visibility = "public"

[[attribute]]
name       = "kind"
type       = "category"
render     = true
index      = true
vocabulary = "kind"
"#;

const KINDS: [Option<&str>; 5] = [Some("a"), Some("b"), Some("c"), Some("d"), None];

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("kind", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let kinds: Vec<Option<&str>> = ids.iter().map(|&e| KINDS[(e % 5) as usize]).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(kinds)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_bundle(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points);
    write_pairs_n(&pairs, N);
    std::fs::write(dir.join("config.toml"), SCHEMA).unwrap();
    let schema = Config::parse(&dir.join("config.toml"), &HashMap::new())
        .expect("the schema parses")
        .schema;
    let out = dir.join("bundle");
    build(&BuildArgs {
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
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");
    out
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Ingest `n` items at scattered positions, every other one visible to the subset principal, one
/// in four carrying a kind the build never saw and one in four none, and flush them into a
/// segment of their own.
fn flush_items(engine: &Engine, n: usize) {
    let mut rng = StdRng::seed_from_u64(7);
    let rows = (0..n)
        .map(|i| {
            let descriptors = if i % 2 == 0 {
                vec![b"0".to_vec(), b"1".to_vec()]
            } else {
                vec![b"0".to_vec()]
            };
            let kind = match i % 4 {
                0 => WalScalar::Utf8("a".to_string()),
                1 => WalScalar::Utf8("b".to_string()),
                2 => WalScalar::Utf8("e".to_string()),
                _ => WalScalar::Null,
            };
            UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                x: rng.gen_range(0.0..1000.0),
                y: rng.gen_range(0.0..1000.0),
                scalars: vec![kind],
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                scoped: Vec::new(),
            }
        })
        .collect();
    engine
        .ingest_rows(rows, "batch-1".to_string(), [1u8; 32])
        .expect("the ingest is accepted");
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
}

fn code_at(segment: &SegmentData, local: usize) -> u32 {
    match segment.columns.scalar("kind") {
        Some(ScalarSlice::U16(codes)) => u32::from(codes[local]),
        None => 0,
        Some(other) => panic!("kind is stored as {other:?}"),
    }
}

/// The table a pass must produce, computed row by row: each row of the set's cell at `depth`
/// from its 64-bit position, and its group from `group`.
fn oracle(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    group: &dyn Fn(u32) -> u32,
) -> Vec<CellCount> {
    let mut table: BTreeMap<(u64, u32), u64> = BTreeMap::new();
    for &(segment, row_base) in segments {
        let morton = segment.morton.u32();
        let residual = segment.columns.residual();
        for local in 0..segment.row_count as usize {
            let row = row_base + local as u32;
            if set.count(row..row + 1) == 0 {
                continue;
            }
            let position = (u64::from(morton[local]) << 32) | u64::from(residual[local]);
            let cell = if depth == 0 {
                0
            } else {
                position >> (64 - 2 * u32::from(depth))
            };
            *table
                .entry((cell, group(code_at(segment, local))))
                .or_default() += 1;
        }
    }
    table
        .into_iter()
        .map(|((cell, group), count)| CellCount { cell, group, count })
        .collect()
}

#[test]
fn every_table_of_the_pass_is_the_one_counted_row_by_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = build_bundle(dir.path());
    let engine = engine_at(dir.path(), &root, 3600);
    flush_items(&engine, 3_000);

    let session = engine.authorise(&subset_credential()).unwrap();
    let (generation, mask) = engine
        .composed_mask(&session, "s0")
        .expect("the mask composes");
    let view_data = &generation.bundle.partitions["default"].views["s0"];
    let segments = segments_with_row_bases("s0", view_data).expect("the segments resolve");
    assert_eq!(segments.len(), 2, "the flush wrote a segment of its own");
    let flushed = segments[1].1..segments[1].1 + segments[1].0.row_count;
    assert!(
        mask.count_range(flushed) > 0,
        "the mask holds rows of the flushed segment, so both segments are counted"
    );

    // Two codes listed, the first twice; everything else is the rest, and code 0 is none.
    let mut drawn: Vec<u32> = segments
        .iter()
        .flat_map(|&(segment, _)| (0..segment.row_count as usize).map(move |i| code_at(segment, i)))
        .filter(|&code| code != 0)
        .collect();
    drawn.sort_unstable();
    drawn.dedup();
    assert!(drawn.len() >= 4, "the fixture draws at least four kinds");
    let listed = [drawn[0], drawn[2], drawn[0]];
    let table = GroupTable::new(&listed);
    assert_eq!((table.rest(), table.none()), (3, 4));
    let by_table = |code: u32| match code {
        0 => 4,
        c if c == drawn[0] => 0,
        c if c == drawn[2] => 1,
        _ => 3,
    };
    let groups = RowGroups::drawn(&segments, "kind", &table);
    let tables = view_data
        .row_space
        .row_entities()
        .expect("the view has row-entity.u32");
    let codes = generation
        .filter_columns
        .entity_codes("kind")
        .expect("kind is indexed");
    let through_entities = RowGroups::entity(&segments, tables, &codes, &table);

    let mut rng = StdRng::seed_from_u64(11);
    let total = view_data.row_space.total_rows() as u32;
    let sample: Bitmap = (0..total).filter(|_| rng.gen_bool(0.3)).collect();
    let rows = mask.visible_rows(&sample);
    let whole_view = || {
        segments
            .iter()
            .map(|&(segment, row_base)| (segment, row_base, 0..segment.row_count))
    };

    for (what, set) in [
        ("the mask", CellSet::Mask(&mask)),
        ("a bitmap", CellSet::Rows(&rows)),
    ] {
        for depth in 0..=32u8 {
            let alone = oracle(set, &segments, depth, &|_| 0);
            assert_eq!(
                pass(set, &segments, depth, &RowGroups::None),
                alone,
                "{what}, {depth}"
            );
            assert_eq!(
                pass_in_chunks(set, &segments, depth, &RowGroups::None, 500),
                alone,
                "{what}, depth {depth}, in chunks of 500 rows"
            );
            let grouped = oracle(set, &segments, depth, &by_table);
            assert_eq!(
                pass_in_chunks(set, &segments, depth, &groups, 500),
                grouped,
                "{what}, depth {depth}, grouped by the drawn code"
            );
            assert_eq!(
                pass_in_chunks(set, &segments, depth, &through_entities, 500),
                grouped,
                "{what}, depth {depth}, grouped by the entity's indexed code"
            );
            if depth <= 16 {
                let ranged = count_by_ranges(whole_view(), depth, 0..1u64 << (2 * depth), &|r| {
                    set.count(r)
                });
                let cells: Vec<(u64, u64)> = alone.iter().map(|e| (e.cell, e.count)).collect();
                assert_eq!(ranged.cells, cells, "{what}, depth {depth}, range counts");
            }
        }
    }
}
