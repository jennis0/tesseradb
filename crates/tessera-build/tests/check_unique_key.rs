//! `tessera check` and an indexed keyword's cardinality: the check reads Parquet footers and no
//! row, so it warns exactly where a footer records a distinct count, and nowhere else
//! (`tessera_build::unique_key`).
//!
//! Arrow's writer records no distinct count, so the footers that carry one are written here with
//! the Parquet crate's column writer, statistics supplied. That is also what a writer which tracks
//! cardinality produces, and the check cannot tell the two apart.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{IntType, LogicalType, Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use tessera_build::check::check;
use tessera_build::config::Config;

const N: usize = 60;

const DECLARATION: &str = r#"
[sources]
points = "points.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = "auto"
point_visibility = { default = "public" }

[[attribute]]
name  = "key"
type  = "keyword"
index = true
"#;

fn primitive(name: &str, physical: PhysicalType, logical: Option<LogicalType>) -> Arc<Type> {
    Arc::new(
        Type::primitive_type_builder(name, physical)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(logical)
            .build()
            .unwrap(),
    )
}

/// A points file whose footer records `key`'s distinct count, as a writer tracking cardinality
/// would record it.
fn write_points_with_distinct_count(path: &Path, key_of: &dyn Fn(usize) -> String) {
    let schema = Arc::new(
        Type::group_type_builder("schema")
            .with_fields(vec![
                primitive(
                    "entity_id",
                    PhysicalType::INT64,
                    Some(LogicalType::Integer(IntType {
                        bit_width: 64,
                        is_signed: false,
                    })),
                ),
                primitive("x", PhysicalType::DOUBLE, None),
                primitive("y", PhysicalType::DOUBLE, None),
                primitive("key", PhysicalType::BYTE_ARRAY, Some(LogicalType::String)),
            ])
            .build()
            .unwrap(),
    );
    let properties = Arc::new(
        WriterProperties::builder()
            .set_statistics_enabled(EnabledStatistics::Chunk)
            .build(),
    );
    let mut writer =
        SerializedFileWriter::new(File::create(path).unwrap(), schema, properties).unwrap();
    let mut group = writer.next_row_group().unwrap();

    let ids: Vec<i64> = (0..N as i64).collect();
    let mut column = group.next_column().unwrap().unwrap();
    column
        .typed::<Int64Type>()
        .write_batch(&ids, None, None)
        .unwrap();
    column.close().unwrap();
    for stride in [37usize, 53] {
        let values: Vec<f64> = (0..N).map(|e| ((e * stride) % 1000) as f64).collect();
        let mut column = group.next_column().unwrap().unwrap();
        column
            .typed::<DoubleType>()
            .write_batch(&values, None, None)
            .unwrap();
        column.close().unwrap();
    }

    let keys: Vec<String> = (0..N).map(key_of).collect();
    let mut distinct: Vec<&str> = keys.iter().map(String::as_str).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let values: Vec<ByteArray> = keys.iter().map(|k| ByteArray::from(k.as_str())).collect();
    let min = ByteArray::from(distinct[0]);
    let max = ByteArray::from(*distinct.last().unwrap());
    let mut column = group.next_column().unwrap().unwrap();
    column
        .typed::<ByteArrayType>()
        .write_batch_with_statistics(
            &values,
            None,
            None,
            Some(&min),
            Some(&max),
            Some(distinct.len() as u64),
        )
        .unwrap();
    column.close().unwrap();
    group.close().unwrap();
    writer.close().unwrap();
}

/// The same table through Arrow's writer, whose footer carries no distinct count.
fn write_points_with_arrow(path: &Path, key_of: &dyn Fn(usize) -> String) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("key", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from((0..N as u64).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(
                (0..N).map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                (0..N).map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                (0..N).map(key_of).collect::<Vec<String>>(),
            )),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn declaration(dir: &Path) -> Config {
    let path = dir.join("schema.toml");
    std::fs::write(&path, DECLARATION).unwrap();
    Config::parse(&path, &HashMap::new()).expect("the declaration parses")
}

/// A footer recording as many distinct values as rows warns, names the attribute, says the figure
/// is the footer's upper bound, and leaves the check clean.
#[test]
fn a_footer_recording_a_unique_key_warns_and_the_check_stays_clean() {
    let dir = tempfile::tempdir().unwrap();
    write_points_with_distinct_count(&dir.path().join("points.parquet"), &|e| format!("k{e:04}"));
    let report = check(&declaration(dir.path()));
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    let warning = &report.warnings[0];
    assert_eq!(warning.object.block, "attribute");
    assert_eq!(warning.object.name, "key");
    assert!(
        warning
            .detail
            .contains("UNIQUE KEY INDEXED, by the source's footer"),
        "{}",
        warning.detail
    );
    assert!(
        warning
            .detail
            .contains("60 distinct value(s) over 60 non-null row(s) (100.0%)"),
        "{}",
        warning.detail
    );
    assert!(warning.detail.contains("upper bound"), "{}", warning.detail);
}

/// A footer recording a key that repeats says nothing.
#[test]
fn a_footer_recording_a_repeating_key_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    write_points_with_distinct_count(&dir.path().join("points.parquet"), &|e| {
        format!("k{}", e % 5)
    });
    let report = check(&declaration(dir.path()));
    assert!(report.is_clean(), "{:?}", report.findings);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

/// A footer with no distinct count says nothing, however unique the key: the check reads no row,
/// so the build's report is where that figure is certain.
#[test]
fn a_footer_without_a_distinct_count_says_nothing_about_a_unique_key() {
    let dir = tempfile::tempdir().unwrap();
    write_points_with_arrow(&dir.path().join("points.parquet"), &|e| format!("k{e:04}"));
    let report = check(&declaration(dir.path()));
    assert!(report.is_clean(), "{:?}", report.findings);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        tessera_build::footer_distinct_count(&dir.path().join("points.parquet"), "key").unwrap(),
        None
    );
}
