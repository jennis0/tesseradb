//! **The route a string column's characters take is invisible in the bundle** — which is what lets
//! the build choose it from the free space (`build-column-extents.md` §2).
//!
//! A bundle-wide `keyword` or `utf8` column has two homes: an entity-ordered arena under
//! `.build-tmp/`, or one record-blob extent per join chunk. Which it takes is
//! `residency::plan_routes`, and the input is the modelled scratch against the space free on the
//! output filesystem — so the same corpus on two machines, or on one machine either side of a
//! large build, can take either. Everything downstream depends on that being unobservable: the
//! bundle's digests, the entity assignment under I9, and the byte-equality the reference oracle is
//! compared with.
//!
//! The corpus here is built down each route with `build_routed`, and the two bundles must be the
//! same bundle. The route is read back from the build's own report rather than assumed, so a seam
//! that stopped forcing anything would fail here rather than pass vacuously.
//!
//! A corpus small enough for a test cannot reach either end of the free-space test — the modelled
//! arena is 256 MiB and the filesystem has rather more than 512 MiB free — which is why the route
//! is named here and derived everywhere else (`ExtentRoute`).

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Attribute, Schema};
use tessera_build::{build, build_in_memory, build_routed, BuildArgs, BuildReport, ExtentRoute};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 400;

/// The blob-resident keyword, absent on one entity in seven so the presence bits are on the line
/// too.
fn note_of(e: u64) -> Option<String> {
    (!e.is_multiple_of(7)).then(|| format!("note-{}-{}", e % 13, "x".repeat((e % 11) as usize)))
}

/// The indexed keyword. Its dictionary reads it once in entity order, which an extent merge
/// answers as readily as an arena, so it has two routes like `note` and this column is what pins
/// that its dictionary, its ordinals and its presence bitmap come out the same down both.
fn code_of(e: u64) -> Option<String> {
    (!e.is_multiple_of(5)).then(|| format!("code-{}", e % 17))
}

/// The `text` column: spilled down both routes, its second decode being the source permutation
/// rather than the arena's size.
fn prose_of(e: u64) -> Option<String> {
    match e % 9 {
        0 => None,
        1 => Some(String::new()),
        _ => Some(format!("the quick brown fox {e} jumps over {} lazy dogs", e % 4)),
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("code", DataType::Utf8, true),
        Field::new("prose", DataType::Utf8, true),
        Field::new("flag", DataType::Int64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| note_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| code_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| prose_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| (e % 3) as i64).collect::<Vec<_>>(),
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

/// The three string families the routing distinguishes, beside a render column: a keyword with
/// neither flag (two routes), an indexed keyword (the arena, always), and a `text` column (the
/// extents, always).
///
/// Constructed rather than parsed, as `tests/record_blob.rs` constructs its own: a neither-column
/// is what this file is about and the schema parse is not the subject.
fn route_schema() -> Schema {
    let string = |name: &str, ty: ScalarType, index: bool| Attribute {
        field: None,
        name: name.to_string(),
        title: None,
        ty,
        analyser: (ty == ScalarType::Text).then(|| "unicode/icu4x-2.2/p1".to_string()),
        vocabulary: None,
        value_set: None,
        index,
        render: false,
    };
    Schema {
        attributes: vec![
            string("note", ScalarType::Keyword, false),
            string("code", ScalarType::Keyword, true),
            string("prose", ScalarType::Text, true),
            Attribute {
                field: None,
                name: "flag".to_string(),
                title: None,
                ty: ScalarType::I64,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: false,
                render: true,
            },
        ],
        vocabularies: Default::default(),
    }
}

fn args(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    let schema = route_schema();
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

/// Compare two bundles file for file, with `MANIFEST.json`'s wall-clock `created_at` blanked and
/// the `CURRENT` that carries its digest skipped. `tests/build_equivalence.rs` holds the same
/// comparison for the oracle; it is repeated here rather than shared because a test binary is its
/// own crate.
fn assert_bundles_identical(left: &Path, right: &Path, what: &str) {
    let a = collect(left);
    let b = collect(right);
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{what}: the two bundles do not contain the same files"
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        if name.ends_with("MANIFEST.json") {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "{what}: {name} differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue;
        }
        assert_eq!(
            left_bytes,
            right_bytes,
            "{what}: {name} is not byte-identical ({} vs {} bytes)",
            left_bytes.len(),
            right_bytes.len()
        );
    }
    assert!(
        a.len() > 6,
        "{what}: expected a full bundle, found {} files",
        a.len()
    );
}

/// Build the fixture down one route.
fn built(dir: &Path, name: &str, route: ExtentRoute) -> (PathBuf, BuildReport) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let out = dir.join(name);
    let args = args(&points, &pairs, out.clone());
    let report =
        build_routed(&args, route).unwrap_or_else(|e| panic!("{name}: the build succeeds: {e}"));
    (out, report)
}

/// **The bundle does not record which route its columns took.** One corpus, both routes, one set
/// of bytes.
///
/// The routes are read back from the reports rather than assumed to have moved: a seam that
/// stopped forcing anything would make this test pass while checking nothing.
#[test]
fn the_route_a_string_column_takes_is_not_in_the_bundle() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"));
    write_empty_pairs(&dir.path().join("pairs.parquet"));

    let (spilling, spilling_report) = built(dir.path(), "spilling", ExtentRoute::Extents);
    let (arena, arena_report) = built(dir.path(), "arena", ExtentRoute::Arena);

    // `prose` is the `text` column and spills either way; `note` and `code` are the two with a
    // choice — the blob-resident keyword and the indexed one.
    assert_eq!(
        spilling_report.spilled_columns,
        vec!["note".to_string(), "code".to_string(), "prose".to_string()],
        "the extent route spills every column it is available to"
    );
    assert_eq!(
        arena_report.spilled_columns,
        vec!["prose".to_string()],
        "the arena route leaves `text` where it is and takes back the rest"
    );

    assert_bundles_identical(&spilling, &arena, "the two routes");
}

/// **Both build paths route the same way**, so the byte-equality oracle and the streaming pipeline
/// put a column's values in the blob under one tag rather than two. The routes are derived here,
/// which is the case the equality tests in `tests/build_equivalence.rs` run under.
#[test]
fn the_oracle_and_the_streaming_build_derive_the_same_routes() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"));
    write_empty_pairs(&dir.path().join("pairs.parquet"));
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");

    let streaming = build(&args(&points, &pairs, dir.path().join("streaming")))
        .expect("the streaming build succeeds");
    let oracle = build_in_memory(&args(&points, &pairs, dir.path().join("oracle")))
        .expect("the oracle build succeeds");
    assert_eq!(streaming.spilled_columns, oracle.spilled_columns);
    // And `text` is in it whatever the free space said.
    assert!(streaming.spilled_columns.contains(&"prose".to_string()));
    assert_bundles_identical(
        &dir.path().join("streaming"),
        &dir.path().join("oracle"),
        "the derived route, both build paths",
    );
}
