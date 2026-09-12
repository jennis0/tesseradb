//! The segment's **row-order tail** — one render column of every type the hot column can hold,
//! written into `columns.arrow`'s own values buffer a row bucket at a time.
//!
//! The tail was eight `Vec`s built by `push`, then one mapped array per column written at a
//! scattered row index; it is now two partitions a column and a sequential window write
//! (`assembly::render_tail`). Nothing about that is visible in a bundle comparison **unless a
//! type's bytes are wrong**, which is exactly the failure mode a per-type handover has: a width
//! taken from the wrong arm, a bit order reversed, a lane's output landing under another column's
//! name. So every renderable type is declared here, given values a wrong width would mangle, and
//! read back through `ColumnsRef` — the reader the serving path uses.
//!
//! `bool` gets its own attention because it is the one member Arrow does not take as a flat array
//! of itself: the lane fills a byte a row and packs it into `rows` bits on the way out, least
//! significant bit first. A reversed bit order is a column of plausible booleans, every one of
//! them another row's.
//!
//! **Absence is here too**, because the tail leaves an absent slot as the mapping's zero rather
//! than writing the render placeholder over it: the two are the same bytes
//! (`column::tests::the_render_placeholder_is_the_zero_a_mapping_reads_as` holds them together)
//! and the presence bitmap beside the column is what says the zero means nothing (decision 0064).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    BinaryArray, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    Int8Array, TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Attribute, Schema};
use tessera_build::{build, BuildArgs};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_store::read::{ColumnsRef, ScalarSlice};
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// Not a multiple of 64, and not a multiple of 8 — the two word sizes the presence bitmap and the
/// packed booleans are stored in.
const N: u64 = 1_001;

/// A source id's values, chosen so that a column read at the wrong width is a different number:
/// every one uses the whole of its declared range rather than a small integer that fits in all of
/// them.
fn u8_of(e: u64) -> u8 {
    (e % 251) as u8
}
fn u16_of(e: u64) -> u16 {
    (e.wrapping_mul(37) % 65_521) as u16
}
fn u32_of(e: u64) -> u32 {
    (e.wrapping_mul(2_654_435_761) % 4_294_967_291) as u32
}
fn u64_of(e: u64) -> u64 {
    u64::MAX - e.wrapping_mul(1_000_003)
}
fn i8_of(e: u64) -> i8 {
    ((e % 251) as i64 - 125) as i8
}
fn i16_of(e: u64) -> i16 {
    ((e.wrapping_mul(37) % 65_521) as i64 - 32_760) as i16
}
fn i32_of(e: u64) -> i32 {
    i32::MIN.wrapping_add((e.wrapping_mul(7_919) % 4_000_000_000) as i32)
}
fn i64_of(e: u64) -> i64 {
    i64::MIN.wrapping_add(e.wrapping_mul(1_000_000_007) as i64)
}
fn f32_of(e: u64) -> f32 {
    e as f32 * 1.5e18 + 0.25
}
fn f64_of(e: u64) -> f64 {
    e as f64 * -1.5e250 - 0.125
}
fn when_of(e: u64) -> i64 {
    1_700_000_000_000_000 + e as i64 * 1_000_003
}

/// The one column with absences, and the one that is bit-packed. Absent on a stride coprime with
/// both 8 and 64, so absences straddle every word boundary of both.
fn flag_of(e: u64) -> Option<bool> {
    (!e.is_multiple_of(11)).then(|| e.is_multiple_of(3))
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("flag", DataType::Boolean, true),
        Field::new("a_u8", DataType::UInt8, false),
        Field::new("a_u16", DataType::UInt16, false),
        Field::new("a_u32", DataType::UInt32, false),
        Field::new("a_u64", DataType::UInt64, false),
        Field::new("a_i8", DataType::Int8, false),
        Field::new("a_i16", DataType::Int16, false),
        Field::new("a_i32", DataType::Int32, false),
        Field::new("a_i64", DataType::Int64, false),
        Field::new("a_f32", DataType::Float32, false),
        Field::new("a_f64", DataType::Float64, false),
        Field::new(
            "a_when",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
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
            Arc::new(BooleanArray::from(
                ids.iter().map(|&e| flag_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt8Array::from(
                ids.iter().map(|&e| u8_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt16Array::from(
                ids.iter().map(|&e| u16_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                ids.iter().map(|&e| u32_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                ids.iter().map(|&e| u64_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int8Array::from(
                ids.iter().map(|&e| i8_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int16Array::from(
                ids.iter().map(|&e| i16_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                ids.iter().map(|&e| i32_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| i64_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float32Array::from(
                ids.iter().map(|&e| f32_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| f64_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(TimestampMicrosecondArray::from(
                ids.iter().map(|&e| when_of(e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
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

fn schema() -> Schema {
    let render = |name: &str, ty: ScalarType| Attribute {
        field: None,
        name: name.to_string(),
        title: None,
        ty,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: true,
    };
    Schema {
        attributes: vec![
            render("flag", ScalarType::Bool),
            render("a_u8", ScalarType::U8),
            render("a_u16", ScalarType::U16),
            render("a_u32", ScalarType::U32),
            render("a_u64", ScalarType::U64),
            render("a_i8", ScalarType::I8),
            render("a_i16", ScalarType::I16),
            render("a_i32", ScalarType::I32),
            render("a_i64", ScalarType::I64),
            render("a_f32", ScalarType::F32),
            render("a_f64", ScalarType::F64),
            render("a_when", ScalarType::TimestampUs),
        ],
        vocabularies: HashMap::new(),
    }
}

fn args(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    let schema = schema();
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out,
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

fn build_bundle() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_empty_pairs(&pairs);
    build(&args(&points, &pairs, dir.path().join("bundle"))).expect("the build succeeds");
    dir
}

fn segment_dir(out: &Path) -> PathBuf {
    out.join("v00000/partitions/default/views/s0/segments/seg-0")
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// Entity id → source id, through the external-id sidecar.
fn source_of_entity(out: &Path) -> HashMap<u32, u64> {
    let bundle = tessera_store::open_bundle(out).unwrap();
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
                    ent.value(i),
                    u64::from_le_bytes(ext.value(i).try_into().unwrap()),
                );
            }
        }
    }
    map
}

/// The row → source id map: through `tessera_id` and the identity key's inverse to the entity, and
/// through the external-id sidecar from there. **A row is not its entity and an entity is not its
/// source id** (§11.1), so the values a row carries can only be checked against the item they
/// belong to by going back through both.
fn source_of_row(out: &Path, columns: &ColumnsRef) -> Vec<u64> {
    let key = IdentityKey::from_hex(TEST_KEY_HEX).unwrap();
    let sources = source_of_entity(out);
    columns
        .tessera_id()
        .iter()
        .map(|&id| {
            let (_, entity) = key.invert(tessera_types::TesseraId::new(id));
            sources[&(entity.raw() as u32)]
        })
        .collect()
}

/// **Every renderable type round-trips at its own width**, from the source file through the
/// entity-order column, the row permutation and the mapped tail into `columns.arrow`.
#[test]
fn every_render_type_reaches_the_segment_at_its_own_width() {
    let dir = build_bundle();
    let out = dir.path().join("bundle");
    let columns = ColumnsRef::load(&segment_dir(&out).join("columns.arrow")).expect("columns");
    let source = source_of_row(&out, &columns);
    assert_eq!(source.len(), N as usize);

    macro_rules! check {
        ($name:literal, $variant:ident, $of:ident) => {{
            let ScalarSlice::$variant(values) = columns.scalar($name).expect($name) else {
                panic!("{} is not stored as {}", $name, stringify!($variant));
            };
            assert_eq!(values.len(), N as usize, "{}", $name);
            for (row, &e) in source.iter().enumerate() {
                assert_eq!(values[row], $of(e), "{} at row {row} (source {e})", $name);
            }
        }};
    }
    check!("a_u8", U8, u8_of);
    check!("a_u16", U16, u16_of);
    check!("a_u32", U32, u32_of);
    check!("a_u64", U64, u64_of);
    check!("a_i8", I8, i8_of);
    check!("a_i16", I16, i16_of);
    check!("a_i32", I32, i32_of);
    check!("a_i64", I64, i64_of);
    check!("a_f32", F32, f32_of);
    check!("a_f64", F64, f64_of);
    check!("a_when", TimestampUs, when_of);
}

/// **The bit-packed member**, and the absence beside it.
///
/// A boolean render column is `rows` bits rather than `rows` bytes, so it is the one type whose
/// handover can be wrong in a way that still reads as booleans. The fixture's absences are on a
/// stride coprime with 8 and with 64, so they fall inside packed bytes and across their edges; an
/// absent row carries `false` — the render placeholder, which is the zero the mapping already held
/// — and the presence bitmap is what distinguishes it from a row that carries `false` as a value.
#[test]
fn a_boolean_column_is_packed_in_arrows_own_bit_order_and_keeps_its_absences() {
    let dir = build_bundle();
    let out = dir.path().join("bundle");
    let columns = ColumnsRef::load(&segment_dir(&out).join("columns.arrow")).expect("columns");
    let source = source_of_row(&out, &columns);

    let ScalarSlice::Bool(flags) = columns.scalar("flag").expect("flag column") else {
        panic!("flag is declared bool");
    };
    let presence = columns.presence("flag");
    assert_eq!(flags.len(), N as usize);
    let mut absent = 0usize;
    let mut carried_false = 0usize;
    for (row, &e) in source.iter().enumerate() {
        match flag_of(e) {
            Some(value) => {
                assert!(
                    presence.contains(row as u32),
                    "row {row} (source {e}) carries a value"
                );
                assert_eq!(flags.value(row), value, "flag at row {row} (source {e})");
                if !value {
                    carried_false += 1;
                }
            }
            None => {
                absent += 1;
                assert!(
                    !presence.contains(row as u32),
                    "row {row} (source {e}) carries nothing"
                );
                assert!(
                    !flags.value(row),
                    "an absent boolean is written as the render placeholder, which is false"
                );
            }
        }
    }
    assert!(absent > 0, "a fixture with no absence pins nothing here");
    assert!(
        carried_false > 0,
        "a fixture where false only ever means absent cannot tell the two apart"
    );
}
