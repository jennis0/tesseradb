//! What `tessera build` does with the rows the identity rule refuses: leaves them out, prints them,
//! writes them to `reports/refused.json` in the bundle, and refuses the build under `--strict`.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

fn write(path: &Path, columns: Vec<(&str, ArrayRef)>) {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let batch =
        RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, a)| a).collect())
            .unwrap();
    let mut writer = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// A project whose points carry `ids` and whose members file names `members`.
fn project(dir: &Path, ids: &[u64], members: &[u64]) {
    std::fs::write(
        dir.join("tessera.toml"),
        r#"
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("schema.toml"),
        r#"
[sources]
points  = "points.parquet"
members = "members.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 10.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "id"
type   = "u64"
unique = true

[[layer]]
name                      = "groups"
views                     = ["s0"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

  [layer.members]
  source = "members"
"#,
    )
    .unwrap();
    let at = |i: usize| (i % 10) as f64;
    write(
        &dir.join("points.parquet"),
        vec![
            ("id", Arc::new(UInt64Array::from(ids.to_vec())) as ArrayRef),
            ("x", Arc::new(Float64Array::from_iter_values((0..ids.len()).map(at)))),
            ("y", Arc::new(Float64Array::from_iter_values((0..ids.len()).map(at)))),
        ],
    );
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", Arc::new(StringArray::from(vec!["k"; members.len()])) as ArrayRef),
            ("id", Arc::new(UInt64Array::from(members.to_vec()))),
        ],
    );
}

fn build(dir: &Path, strict: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tessera"));
    command.arg("build").current_dir(dir);
    if strict {
        command.arg("--strict");
    }
    command.output().expect("the binary runs")
}

fn refused(dir: &Path) -> Option<Vec<(String, String, u64)>> {
    let bytes = std::fs::read(dir.join("bundle/reports/refused.json")).ok()?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    let mut out: Vec<(String, String, u64)> = entries
        .iter()
        .map(|entry| {
            (
                entry["object"].as_str().unwrap().to_string(),
                entry["reason"].as_str().unwrap().to_string(),
                entry["rows"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    Some(out)
}

/// **A refused row is left out and reported, and the build goes on.** A points row repeating an
/// id and a member naming an id no row created are both in the report file, with their counts.
#[test]
fn refused_rows_are_reported_and_the_build_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), &[1, 2, 3, 2, 4], &[1, 9, 9, 3]);
    let output = build(tmp.path(), false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        refused(tmp.path()).expect("the report is written"),
        [
            ("layer 'groups' members".to_string(), "names_no_item".to_string(), 2),
            ("view 's0'".to_string(), "one_value_twice".to_string(), 1),
        ]
    );
}

/// **`--strict` refuses the build at the first refused row**, and leaves no bundle to serve.
#[test]
fn strict_refuses_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), &[1, 2, 3, 2, 4], &[1, 3]);
    let output = build(tmp.path(), true);
    assert!(!output.status.success());
    assert!(!tmp.path().join("bundle/CURRENT").exists());
}

/// **A build that refuses nothing writes no report**, and `--strict` builds it.
#[test]
fn a_build_refusing_nothing_writes_no_report() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), &[1, 2, 3, 4], &[1, 3]);
    let output = build(tmp.path(), true);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(refused(tmp.path()), None);
}

/// **`--limit` fits an `auto` frame over the rows the build keeps**, and a member naming an item it
/// left out is outside the limit, so `--strict` builds.
#[test]
fn a_limited_build_frames_the_rows_it_keeps() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), &[1, 2, 3, 4], &[1, 3]);
    let schema = std::fs::read_to_string(tmp.path().join("schema.toml")).unwrap();
    let schema = schema.replace("{ min = 0.0, max = 10.0 }", "\"auto\"");
    std::fs::write(tmp.path().join("schema.toml"), schema).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tessera"))
        .args(["build", "--strict", "--limit", "3"])
        .current_dir(tmp.path())
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        refused(tmp.path()).expect("the member left out is reported"),
        [("layer 'groups' members".to_string(), "outside_limit".to_string(), 1)]
    );
    let bundle = tessera_store::read::open_bundle(&tmp.path().join("bundle")).unwrap();
    assert_eq!(bundle.manifest.entity_id_high_water, 2);
    // The kept rows sit at (0, 0) and (1, 1), and the rows left out at (2, 2) and (3, 3).
    let frame = bundle.manifest.views[0].quantisation;
    assert!(frame.x_min <= 0.0 && frame.x_max >= 1.0 && frame.x_max < 2.0, "{frame:?}");
}

/// **A build that refuses nothing removes a report left at its output**, so the bundle's report
/// is the rows its own build refused.
#[test]
fn a_build_refusing_nothing_removes_a_report_left_there() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), &[1, 2, 3, 2], &[1, 3]);
    let output = build(tmp.path(), false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report = std::fs::read(tmp.path().join("bundle/reports/refused.json")).unwrap();
    std::fs::remove_dir_all(tmp.path().join("bundle")).unwrap();
    std::fs::create_dir_all(tmp.path().join("bundle/reports")).unwrap();
    std::fs::write(tmp.path().join("bundle/reports/refused.json"), report).unwrap();

    project(tmp.path(), &[1, 2, 3, 4], &[1, 3]);
    let output = build(tmp.path(), false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(refused(tmp.path()), None);
}
