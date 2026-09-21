//! A corpus whose string columns carry 64-bit offsets builds into the same bundle as the same
//! corpus at 32-bit offsets.
//!
//! Arrow spells a string column `Utf8` or `LargeUtf8` and the bytes are the same either way. The
//! width is the writer's choice: a pandas 3 frame written to Parquet carries every string column
//! as `LargeUtf8`, which is what a notebook staging a DataFrame produces. So the two spellings are
//! one corpus, and the check and the build must say so.
//!
//! One fixture, written twice, through the real binary on `check_and_reports.rs`'s precedent.
//! Every string column the build reads is at the fixture's width: the points file's `text` and
//! `category` columns, its access list (`list<utf8>` against `large_list<large_utf8>`), the
//! vocabulary file's `key` and `title`, the artifact table's `key` and its `contents`, and the
//! member table's `key`. What is asserted is that `tessera check` prints the same report and the
//! build writes the same bundle, byte for byte, past the manifest's wall-clock timestamp.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float64Array, LargeListArray, LargeStringArray, ListArray, StringArray, UInt32Array,
    UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

const KEY: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 64;

/// Which offset width a fixture's string columns are written at.
#[derive(Clone, Copy, PartialEq)]
enum Width {
    /// `Utf8` and `List`: 32-bit offsets.
    Narrow,
    /// `LargeUtf8` and `LargeList`: 64-bit offsets, which is what pandas 3 writes.
    Wide,
}

impl Width {
    fn string(self) -> DataType {
        match self {
            Width::Narrow => DataType::Utf8,
            Width::Wide => DataType::LargeUtf8,
        }
    }

    fn list_of_strings(self) -> DataType {
        let item = Arc::new(Field::new("item", self.string(), true));
        match self {
            Width::Narrow => DataType::List(item),
            Width::Wide => DataType::LargeList(item),
        }
    }

    /// A string column of `values`.
    fn strings(self, values: Vec<Option<&str>>) -> ArrayRef {
        match self {
            Width::Narrow => Arc::new(StringArray::from(values)),
            Width::Wide => Arc::new(LargeStringArray::from(values)),
        }
    }

    /// A list column, one element per row, at this width's list and string types.
    fn one_per_row(self, values: Vec<&str>) -> ArrayRef {
        let rows = values.len();
        let item = Arc::new(Field::new("item", self.string(), true));
        let inner = self.strings(values.into_iter().map(Some).collect());
        match self {
            Width::Narrow => Arc::new(ListArray::new(
                item,
                OffsetBuffer::from_lengths((0..rows).map(|_| 1usize)),
                inner,
                None,
            )),
            Width::Wide => Arc::new(LargeListArray::new(
                item,
                OffsetBuffer::from_lengths((0..rows).map(|_| 1usize)),
                inner,
                None,
            )),
        }
    }

    /// The `contents` column's type: one entry per rank, each a list of values.
    ///
    /// The list itself is a plain `List` at both widths, the values inside it the fixture's. A
    /// frame written by pandas carries 64-bit offsets on its strings and 32-bit ones on the list
    /// holding them, so that is the pairing the fixture puts through this reader.
    fn ranked_type(self) -> DataType {
        DataType::List(Arc::new(Field::new(
            "item",
            DataType::List(Arc::new(Field::new("item", self.string(), true))),
            true,
        )))
    }

    /// The `contents` column: one entry, carrying one value.
    fn ranked(self, value: &str) -> ArrayRef {
        let item = Arc::new(Field::new("item", self.string(), true));
        let entries = Arc::new(ListArray::new(
            item,
            OffsetBuffer::from_lengths([1usize]),
            self.strings(vec![Some(value)]),
            None,
        )) as ArrayRef;
        Arc::new(ListArray::new(
            Arc::new(Field::new(
                "item",
                DataType::List(Arc::new(Field::new("item", self.string(), true))),
                true,
            )),
            OffsetBuffer::from_lengths([1usize]),
            entries,
            None,
        ))
    }
}

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn write(path: &Path, fields: Vec<Field>, columns: Vec<ArrayRef>) {
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

const DECLARATION: &str = r#"
[sources]
points           = "points.parquet"
severity_values  = "severity-values.parquet"
clusters         = "clusters.parquet"
cluster_members  = "cluster-members.parquet"
topics           = "topics.parquet"
topic_members    = "topic-members.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { field = "access", default = "public" }

[[vocabulary]]
name       = "severity"
width      = "u8"
value_set  = "closed"
visibility = "public"
source     = "severity_values"

[[attribute]]
name       = "severity"
type       = "category"
vocabulary = "severity"
render     = true
index      = true

[[attribute]]
name  = "title"
type  = "text"
index = true

[[layer]]
name       = "clusters/a"
title      = "Clusters"
views      = ["s0"]
source     = "clusters"
membership = "enumerated"
hierarchy  = { kind = "flat" }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
content                   = { computed = ["centroid"] }

  [layer.members]
  source = "cluster_members"

  [layer.labels]
  source                    = "topics"
  name                      = "topics/a"
  type                      = "text"
  membership                = "enumerated"
  artifact_visibility       = { default = "inherited" }
  require_member_visibility = "all"
  visibility                = "public"

    [layer.labels.content]
    require_member_visibility = "all"

    [layer.labels.members]
    source = "topic_members"
"#;

const DEPLOYMENT: &str = r#"
[bundle]
path  = "bundles/corpus"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37601"
session = "127.0.0.1:49321"
control = "127.0.0.1:45741"
"#;

/// The severity each item carries, and the prose in its `title`. Written from the entity id, so
/// the two fixtures describe one corpus.
fn severity(entity: u64) -> &'static str {
    ["low", "medium", "high"][(entity % 3) as usize]
}

/// The whole project at one width: the deployment file, the declaration, and every source.
fn project(dir: &Path, width: Width) {
    std::fs::write(dir.join("tessera.toml"), DEPLOYMENT).unwrap();
    std::fs::write(dir.join("schema.toml"), DECLARATION).unwrap();

    let ids: Vec<u64> = (0..N).collect();
    let titles: Vec<String> = ids
        .iter()
        .map(|e| format!("a title about item {e} and its neighbours"))
        .collect();
    write(
        &dir.join("points.parquet"),
        vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("x", DataType::Float64, false),
            Field::new("y", DataType::Float64, false),
            Field::new("access", width.list_of_strings(), true),
            Field::new("severity", width.string(), true),
            Field::new("title", width.string(), true),
        ],
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| (e * 13 % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| (e * 29 % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            width.one_per_row(ids.iter().map(|_| "public").collect()),
            width.strings(ids.iter().map(|e| Some(severity(*e))).collect()),
            width.strings(titles.iter().map(|t| Some(t.as_str())).collect()),
        ],
    );

    // The vocabulary as a file: `key`, `code` and `title`, with the codes pinned so nothing is
    // minted and the two bundles are comparable byte for byte.
    write(
        &dir.join("severity-values.parquet"),
        vec![
            Field::new("key", width.string(), false),
            Field::new("code", DataType::UInt32, false),
            Field::new("title", width.string(), true),
        ],
        vec![
            width.strings(vec![Some("low"), Some("medium"), Some("high")]),
            Arc::new(UInt32Array::from(vec![11u32, 23, 37])),
            width.strings(vec![Some("Low"), Some("Medium"), Some("High")]),
        ],
    );

    write(
        &dir.join("clusters.parquet"),
        vec![Field::new("key", width.string(), false)],
        vec![width.strings(vec![Some("c0"), Some("c1")])],
    );
    // One row per `(artifact, entity)`: the key at the fixture's width, the entity an integer.
    let member_keys: Vec<&str> = ids
        .iter()
        .map(|e| if e % 2 == 0 { "c0" } else { "c1" })
        .collect();
    write(
        &dir.join("cluster-members.parquet"),
        vec![
            Field::new("key", width.string(), false),
            Field::new("entity", DataType::UInt64, false),
        ],
        vec![
            width.strings(member_keys.into_iter().map(Some).collect()),
            Arc::new(UInt64Array::from(ids.clone())),
        ],
    );

    write(
        &dir.join("topics.parquet"),
        vec![
            Field::new("key", width.string(), false),
            Field::new("contents", width.ranked_type(), true),
            Field::new("attached_layer", width.string(), true),
            Field::new("attached_key", width.string(), true),
        ],
        vec![
            width.strings(vec![Some("t0")]),
            width.ranked("a topic over the first cluster"),
            width.strings(vec![Some("clusters/a")]),
            width.strings(vec![Some("c0")]),
        ],
    );
    write(
        &dir.join("topic-members.parquet"),
        vec![
            Field::new("key", width.string(), false),
            Field::new("entity", DataType::UInt64, false),
            Field::new("rank", DataType::UInt32, false),
        ],
        vec![
            width.strings(vec![Some("t0"), Some("t0")]),
            Arc::new(UInt64Array::from(vec![0u64, 2])),
            Arc::new(UInt32Array::from(vec![0u32, 0])),
        ],
    );
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    tessera()
        .args(args)
        .current_dir(cwd)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .expect("failed to run tessera")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Every file under `root`, keyed by its `root`-relative slash-separated path.
fn collect(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn a_corpus_at_both_offset_widths_checks_and_builds_the_same() {
    let narrow = tempfile::tempdir().unwrap();
    let wide = tempfile::tempdir().unwrap();
    project(narrow.path(), Width::Narrow);
    project(wide.path(), Width::Wide);

    // The check reads the schema alone, so this is the type check at both widths.
    let left = run(narrow.path(), &["check"]);
    let right = run(wide.path(), &["check"]);
    assert!(left.status.success(), "{}", stderr(&left));
    assert!(
        right.status.success(),
        "a large_utf8 corpus must check clean:\n{}",
        stderr(&right)
    );
    // The page names each source by the path it was read from, and the two projects sit in two
    // temporary directories, so the root is the whole of the difference allowed.
    assert_eq!(
        stderr(&left).replace(&narrow.path().display().to_string(), "<root>"),
        stderr(&right).replace(&wide.path().display().to_string(), "<root>"),
        "the two widths must produce one report"
    );

    let left = run(narrow.path(), &["build"]);
    let right = run(wide.path(), &["build"]);
    assert!(left.status.success(), "{}", stderr(&left));
    assert!(
        right.status.success(),
        "a large_utf8 corpus must build:\n{}",
        stderr(&right)
    );

    let a = collect(&narrow.path().join("bundles/corpus"));
    let b = collect(&wide.path().join("bundles/corpus"));
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "the two bundles do not contain the same files"
    );
    assert!(
        a.len() > 6,
        "expected a full bundle, found {} files",
        a.len()
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        // The manifest carries a wall-clock `created_at`, and `CURRENT` is its digest.
        if name.ends_with("MANIFEST.json") {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "MANIFEST.json differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue;
        }
        assert_eq!(
            left_bytes,
            right_bytes,
            "{name} is not byte-identical ({} vs {} bytes)",
            left_bytes.len(),
            right_bytes.len()
        );
    }
}
