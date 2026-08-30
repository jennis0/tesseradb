//! The attribute join's **per-column split** — every declared column staged, scattered and tallied
//! on a thread of its own (`pipeline::read_one_attribute_source`).
//!
//! The columns share nothing mutable, which is what makes the split sound; what they do share is
//! the arithmetic that reports it. **A tally that lost or double-counted a lane changes the
//! coverage report without changing one byte of the bundle** — the one defect a bundle comparison
//! cannot see — so the figures are asserted here against numbers the fixture computes for itself,
//! and against the linear build's serial pass over the same file.
//!
//! Three properties the split invites a change to break, and one that predates it:
//!
//! * **Absence is a presence bit, never a sentinel.** The empty string is a value a corpus may
//!   hold and an absent one is not a zero-length one, on both sides of the move across.
//! * **Presence is stored 64 entities to a word**, so the corpus here is deliberately not a
//!   multiple of 64 — a lane that rounded its range to a word boundary would write into a
//!   neighbour's bits.
//! * **A batch is staged as a unit and a chunk spans batches**, so the attribute file is written
//!   in small row groups: a Parquet batch never spans one, so this hands the pass several batches
//!   without a corpus of `input::ATTRIBUTE_BATCH_ROWS` rows to do it with. A staging position
//!   miscomputed across a batch boundary gives every later row of the chunk another row's
//!   values.
//! * **A source id is not its own entity id** (§11.1), which is what every read-back below goes
//!   through the external-id sidecar for.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    BinaryArray, BooleanArray, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray,
    UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Attribute, Schema};
use tessera_build::{build, build_in_memory, AttributeCoverage, BuildArgs};
use tessera_filter::{Access, RecordBlob, RecordValue};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// **Not a multiple of 64**, which is what presence is stored in words of.
const N: u64 = 5_001;

/// The attribute file's row group size. A Parquet batch never spans a row group, so this is what
/// makes the pass see several batches — the boundary matters, the corpus size does not.
const ROW_GROUP: usize = 700;

/// Rows of the attribute file naming ids this build never loads — counted, never refused.
const STRANGERS: u64 = 500;

/// Whether the attribute source carries a row for this source id at all.
fn carried(e: u64) -> bool {
    !e.is_multiple_of(3)
}

/// The variable-width column. `None` is absence; `Some("")` is the empty string, which is a value.
fn note_of(e: u64) -> Option<String> {
    match e {
        e if e.is_multiple_of(5) => None,
        e if e.is_multiple_of(7) => Some(String::new()),
        e => Some(format!("note-{e}")),
    }
}

fn score_of(e: u64) -> Option<f64> {
    (!e.is_multiple_of(11)).then_some(e as f64 * 0.5 + 0.25)
}

fn count_of(e: u64) -> Option<i64> {
    (!e.is_multiple_of(13)).then_some((e * 11) as i64)
}

fn small_of(e: u64) -> Option<u8> {
    (!e.is_multiple_of(17)).then_some((e % 251) as u8)
}

fn flag_of(e: u64) -> Option<bool> {
    (!e.is_multiple_of(19)).then_some(e.is_multiple_of(2))
}

fn when_of(e: u64) -> Option<i64> {
    (!e.is_multiple_of(23)).then_some(1_700_000_000_000_000 + e as i64)
}

/// Geometry only — the attributes live in their own file, which is what gives the join something
/// to miss.
fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Six declared columns over a subset of this build's ids, plus [`STRANGERS`] rows naming ids no
/// build here assigns.
fn write_attributes(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("score", DataType::Float64, true),
        Field::new("count", DataType::Int64, true),
        Field::new("small", DataType::UInt8, true),
        Field::new("flag", DataType::Boolean, true),
        Field::new(
            "when",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ]));
    let ids: Vec<u64> = (0..N)
        .filter(|&e| carried(e))
        .chain(1_000_000..1_000_000 + STRANGERS)
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| note_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| score_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| count_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt8Array::from(
                ids.iter().map(|&e| small_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                ids.iter().map(|&e| flag_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(TimestampMicrosecondArray::from(
                ids.iter().map(|&e| when_of(e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(ROW_GROUP))
        .build();
    let mut w =
        ArrowWriter::try_new(File::create(path).unwrap(), schema, Some(properties)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_empty_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
            Arc::new(UInt32Array::from(Vec::<u32>::new())),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Six columns, none rendered and none indexed, so every one of them is blob-resident and can be
/// read back per entity without a serving path (records §3). Tags follow declared position.
fn schema() -> Schema {
    let neither = |name: &str, ty: ScalarType| Attribute {
        field: None,
        name: name.to_string(),
        title: None,
        ty,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: false,
    };
    Schema {
        attributes: vec![
            neither("note", ScalarType::Keyword),
            neither("score", ScalarType::F64),
            neither("count", ScalarType::I64),
            neither("small", ScalarType::U8),
            neither("flag", ScalarType::Bool),
            neither("when", ScalarType::TimestampUs),
        ],
        vocabularies: HashMap::new(),
    }
}

fn args(dir: &Path, out: PathBuf) -> BuildArgs {
    let points = dir.join("points.parquet");
    let attributes = dir.join("attributes.parquet");
    let schema = schema();
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(dir.join("pairs.parquet")),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(attributes, &schema),
        out,
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

fn fixture(dir: &Path) {
    write_points(&dir.join("points.parquet"));
    write_attributes(&dir.join("attributes.parquet"));
    write_empty_pairs(&dir.join("pairs.parquet"));
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// Source id → entity id through the external-id sidecar.
fn source_to_entity(out: &Path) -> HashMap<u64, u32> {
    let bundle = open_bundle(out).unwrap();
    let part = bundle.partitions.values().next().unwrap();
    let prefix = current_prefix(out);
    let mut map = HashMap::new();
    for rel in &part.manifest.external_id_runs {
        let path = out.join(&prefix).join(rel);
        let reader =
            arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
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
                .downcast_ref::<UInt32Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                map.insert(
                    u64::from_le_bytes(ext.value(i).try_into().unwrap()),
                    ent.value(i),
                );
            }
        }
    }
    map
}

fn record_dir(out: &Path) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out))
        .join("partitions")
        .join(phash)
        .join("attrs")
        .join("record")
}

/// Every entity's blob row, keyed by **source** id: tag → value, absent tags omitted.
fn rows_by_source(out: &Path) -> BTreeMap<u64, BTreeMap<u16, RecordValue>> {
    let entity_of = source_to_entity(out);
    let blob = RecordBlob::open_dir(&record_dir(out), Access::Read).expect("the blob opens");
    blob.self_check().expect("the artefact is self-consistent");
    let mut rows = BTreeMap::new();
    for (source, entity) in entity_of {
        let read: BTreeMap<u16, RecordValue> = blob
            .fields_of(entity)
            .expect("a well-formed read")
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.tag, f.value))
            .collect();
        rows.insert(source, read);
    }
    rows
}

/// What the fixture says each column's presence tally must be, in declared order.
fn expected_present() -> Vec<u64> {
    let carried_ids = || (0..N).filter(|&e| carried(e));
    vec![
        carried_ids().filter(|&e| note_of(e).is_some()).count() as u64,
        carried_ids().filter(|&e| score_of(e).is_some()).count() as u64,
        carried_ids().filter(|&e| count_of(e).is_some()).count() as u64,
        carried_ids().filter(|&e| small_of(e).is_some()).count() as u64,
        carried_ids().filter(|&e| flag_of(e).is_some()).count() as u64,
        carried_ids().filter(|&e| when_of(e).is_some()).count() as u64,
    ]
}

fn assert_coverage(coverage: &[AttributeCoverage], which: &str) {
    assert_eq!(coverage.len(), 1, "{which}: one attribute source");
    let source = &coverage[0];
    assert_eq!(source.entities, N, "{which}: the denominator is this build");
    assert_eq!(
        source.matched_rows,
        (0..N).filter(|&e| carried(e)).count() as u64,
        "{which}: rows that resolved to an entity"
    );
    assert_eq!(
        source.unknown_rows, STRANGERS,
        "{which}: rows naming ids this build never loaded"
    );
    let names: Vec<&str> = source.columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["note", "score", "count", "small", "flag", "when"],
        "{which}: the columns are reported in declared order"
    );
    let tallies: Vec<u64> = source.columns.iter().map(|(_, c)| *c).collect();
    assert_eq!(
        tallies,
        expected_present(),
        "{which}: each column's presence tally is its own"
    );
}

/// **Every value lands on its own entity, and the tallies say so.**
///
/// The columns are staged, scattered and counted one lane per column; this asserts the whole of
/// what a lane owns — the value, the presence bit beside it, and the lane's share of the coverage
/// report — over a corpus that crosses a batch boundary and ends mid-presence-word.
#[test]
fn every_column_lands_on_its_own_entities_and_reports_its_own_tally() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let out = dir.path().join("bundle");
    let report = build(&args(dir.path(), out.clone())).expect("the build succeeds");
    assert_coverage(&report.attribute_coverage, "streaming");

    let rows = rows_by_source(&out);
    assert_eq!(rows.len(), N as usize);
    for source in 0..N {
        let row = &rows[&source];
        if !carried(source) {
            assert!(
                row.is_empty(),
                "source {source} is named by no attribute row, so it has no field at all"
            );
            continue;
        }
        assert_eq!(
            row.get(&0).cloned(),
            note_of(source).map(RecordValue::Utf8),
            "note, source {source}"
        );
        assert_eq!(
            row.get(&1).cloned(),
            score_of(source).map(RecordValue::F64),
            "score, source {source}"
        );
        assert_eq!(
            row.get(&2).cloned(),
            count_of(source).map(RecordValue::I64),
            "count, source {source}"
        );
        assert_eq!(
            row.get(&3).cloned(),
            small_of(source).map(RecordValue::U8),
            "small, source {source}"
        );
        assert_eq!(
            row.get(&4).cloned(),
            flag_of(source).map(RecordValue::Bool),
            "flag, source {source}"
        );
        assert_eq!(
            row.get(&5).cloned(),
            when_of(source).map(RecordValue::TimestampUs),
            "when, source {source}"
        );
    }
}

/// **The empty string is a value and absence is not a zero-length one**, across the move from the
/// staging buffer into entity order and out again into the blob.
///
/// Asserted apart from the sweep above because it is the distinction the arena's shape invites a
/// change to lose, and because a sweep that compared `Option<String>` to `Option<String>` would
/// pass just as happily if both sides had lost it.
#[test]
fn the_empty_string_survives_the_move_and_absence_stays_absent() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let out = dir.path().join("bundle");
    build(&args(dir.path(), out.clone())).expect("the build succeeds");
    let rows = rows_by_source(&out);

    // 7 and 49 carry the empty string; 5 and 70 carry nothing; both kinds are in the file.
    let empty: Vec<u64> = (0..N)
        .filter(|&e| carried(e) && note_of(e) == Some(String::new()))
        .collect();
    let absent: Vec<u64> = (0..N)
        .filter(|&e| carried(e) && note_of(e).is_none())
        .collect();
    assert!(
        !empty.is_empty() && !absent.is_empty(),
        "the fixture has both"
    );
    for source in empty {
        assert_eq!(
            rows[&source].get(&0),
            Some(&RecordValue::Utf8(String::new())),
            "source {source} carries the empty string, which is a value"
        );
    }
    for source in absent {
        assert_eq!(
            rows[&source].get(&0),
            None,
            "source {source} carries no note, which is not the empty string"
        );
    }
}

/// **The two builds agree about coverage exactly as they agree about bytes.**
///
/// The streaming pipeline tallies inside a scatter split across threads and the linear build
/// tallies in one serial loop; they are two readings of one join, and a lane's share going astray
/// is invisible in the bundle.
#[test]
fn the_linear_build_reports_the_same_coverage_and_places_the_same_values() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let streamed = dir.path().join("streamed");
    let linear = dir.path().join("linear");
    let a = build(&args(dir.path(), streamed.clone())).expect("the streaming build succeeds");
    let b = build_in_memory(&args(dir.path(), linear.clone())).expect("the linear build succeeds");

    assert_coverage(&a.attribute_coverage, "streaming");
    assert_coverage(&b.attribute_coverage, "linear");
    assert_eq!(rows_by_source(&streamed), rows_by_source(&linear));
}
