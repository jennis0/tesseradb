//! **A build joins its files on a declared unique field.**
//!
//! `[defaults].join_field` names a `unique` attribute, and every file the build reads names its
//! item by that field's value: a string or an integer. With no join field each row of the points
//! file is an item of its own, and anything that would need a join is refused.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, ListArray, StringArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, verify_deep, BuildArgs, VerifyOpts};
use tessera_spatial::Bounds;
use tessera_store::unique::{UniqueIndexes, UniqueKey};
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: usize = 24;

/// The keys this corpus names its rows by, **not** in the order the file writes them. A build
/// interns them bytewise, and a fixture whose file order already agreed could not tell the two
/// apart.
fn keys() -> Vec<String> {
    (0..N).map(|i| format!("doc-{:02}", (i * 7) % N)).collect()
}

fn integers() -> Vec<u64> {
    (0..N as u64).map(|i| i * 0x0100).collect()
}

fn write(path: &Path, fields: Vec<Field>, columns: Vec<ArrayRef>) {
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The points file under a join column named `doc_id`, carrying its own access labels and one
/// declared attribute, or, where `ids` is `None`, carrying no join column at all.
fn write_points(path: &Path, ids: Option<ArrayRef>) {
    let mut fields = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();
    if let Some(ids) = ids {
        fields.push(Field::new("doc_id", ids.data_type().clone(), false));
        columns.push(ids);
    }
    fields.push(Field::new("x", DataType::Float64, false));
    fields.push(Field::new("y", DataType::Float64, false));
    fields.push(Field::new("visibility", DataType::Utf8, false));
    fields.push(Field::new("flag", DataType::Int64, false));
    columns.push(Arc::new(Float64Array::from(
        (0..N).map(|i| ((i * 37) % 1000) as f64).collect::<Vec<_>>(),
    )));
    columns.push(Arc::new(Float64Array::from(
        (0..N).map(|i| ((i * 53) % 1000) as f64).collect::<Vec<_>>(),
    )));
    columns.push(Arc::new(StringArray::from(vec!["public"; N])));
    columns.push(Arc::new(Int64Array::from(
        (0..N).map(|i| (i % 3) as i64).collect::<Vec<_>>(),
    )));
    write(path, fields, columns);
}

/// `(key, doc_id)`: one row per `(artifact, item)`, naming items the way the points file does.
fn write_members(path: &Path, ids: ArrayRef) {
    let cluster: Vec<String> = (0..ids.len()).map(|i| format!("c{}", i % 3)).collect();
    write(
        path,
        vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("doc_id", ids.data_type().clone(), false),
        ],
        vec![Arc::new(StringArray::from(cluster)), ids],
    );
}

/// One row per artifact, its membership a list of items.
fn write_artifacts(path: &Path) {
    let members = ListArray::new(
        Arc::new(Field::new("item", DataType::UInt64, true)),
        OffsetBuffer::from_lengths([3usize, 2]),
        Arc::new(UInt64Array::from(vec![0u64, 3, 5, 7, 9])) as ArrayRef,
        None,
    );
    write(
        path,
        vec![
            Field::new("key", DataType::Utf8, false),
            Field::new(
                "members",
                DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
                true,
            ),
        ],
        vec![
            Arc::new(StringArray::from(vec!["c0", "c1"])),
            Arc::new(members),
        ],
    );
}

const SOURCES: &str = r#"
[sources]
points   = "points.parquet"
members  = "members.parquet"
clusters = "clusters.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { field = "visibility", default = "public" }

[[attribute]]
name = "flag"
type = "i64"
"#;

/// A declaration joining on `doc`, a unique attribute of type `ty` in the `doc_id` column.
fn joined(ty: &str) -> String {
    format!(
        "[defaults]\nsource = \"points\"\njoin_field = \"doc\"\n{SOURCES}\n\
         [[attribute]]\nname = \"doc\"\ntype = \"{ty}\"\nunique = true\nfield = \"doc_id\"\n"
    )
}

/// The same declaration with no join field.
fn unjoined() -> String {
    format!("[defaults]\nsource = \"points\"\n{SOURCES}")
}

const LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"

  [layer.members]
  source = "members"
  fields = { entity = "doc_id" }
"#;

/// A layer whose artifacts file carries the membership as a list per artifact.
const ARTIFACT_LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
source     = "clusters"
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
"#;

/// The same layer with its artifacts, and their memberships, written in the document.
const INLINE_LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"
artifacts  = [{ key = "c0", members = [0, 3, 5] }, { key = "c1", members = [7, 9] }]

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
"#;

/// One build over `declaration`, parsed against the files in `dir`, into `out`.
fn args(dir: &Path, declaration: &str, out: &Path) -> BuildArgs {
    let path = dir.join("schema.toml");
    std::fs::write(&path, declaration).unwrap();
    let config = tessera_build::config::Config::parse(&path, &Default::default())
        .expect("the declaration parses");
    let view = &config.views[0];
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: view.name.clone(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: view.source.clone().expect("the view names a source"),
            point_fields: view.fields.clone(),
            select: None,
            access: tessera_build::config::AccessInput {
                source: tessera_build::config::AccessSource::Field("visibility".to_string()),
                default: Some("public".to_string()),
            },
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: config.attribute_sources.clone(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        shard_id: 0,
        layers: config.layers.clone(),
        layer_inputs: config.layer_sources.clone(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema.clone(),
    }
}

/// The entities `doc`'s unique index holds under each of `keys`, in order.
fn holders(root: &Path, keys: &[UniqueKey]) -> Vec<Vec<u32>> {
    let bundle = tessera_store::read::open_bundle(root).expect("the built bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let indexes = UniqueIndexes::open(
        &bundle.manifest,
        &partition.manifest,
        &root.join("v00000"),
        None,
    )
    .expect("the indexes open");
    let index = indexes.get("doc").expect("the join field is indexed");
    let mut out = vec![Vec::new(); keys.len()];
    for (at, entity) in index.lookup(keys).expect("the index reads") {
        out[at].push(entity);
    }
    out
}

/// Every key names exactly one item, and every item is named by one key.
fn assert_one_item_per_key(root: &Path, keys: &[UniqueKey]) {
    let held = holders(root, keys);
    let mut entities: Vec<u32> = held
        .iter()
        .map(|found| {
            assert_eq!(found.len(), 1, "one item per join value: {found:?}");
            found[0]
        })
        .collect();
    entities.sort_unstable();
    assert_eq!(entities, (0..N as u32).collect::<Vec<_>>());
}

// -------------------------------------------------------------------------------------------

/// **A keyword join field joins the members table on its strings, and each string is the unique
/// value of the item it names.**
#[test]
fn a_keyword_join_field_joins_the_members_and_names_one_item_per_value() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let keys = keys();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys.clone()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(StringArray::from(keys.clone())),
    );
    let out = dir.join("bundle");
    let report = build(&args(dir, &(joined("keyword") + LAYER), &out)).expect("the build succeeds");
    assert_eq!(report.items, N as u64);
    let unique: Vec<UniqueKey> = keys.iter().map(|k| UniqueKey::keyword(k)).collect();
    assert_one_item_per_key(&out, &unique);

    // The members table named those strings: a key naming no row is a refusal, so a build that
    // got here joined all of them, and the artifacts the open value set minted are the clusters.
    assert_eq!(report.minted_artifacts, 3, "three cluster keys");
    assert_eq!(report.unclustered_member_rows, 0);

    verify_deep(&out, &VerifyOpts::default()).expect("the string-keyed bundle verifies");
}

/// **An integer join field joins the same way**, each integer the unique value of its item.
#[test]
fn an_integer_join_field_joins_the_members_and_names_one_item_per_value() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ids = integers();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(UInt64Array::from(ids.clone()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(UInt64Array::from(ids.clone())),
    );
    let out = dir.join("bundle");
    let report = build(&args(dir, &(joined("u64") + LAYER), &out)).expect("the build succeeds");
    assert_eq!(report.minted_artifacts, 3);
    let unique: Vec<UniqueKey> = ids.iter().map(|id| UniqueKey::unsigned(*id)).collect();
    assert_one_item_per_key(&out, &unique);
    verify_deep(&out, &VerifyOpts::default()).expect("the integer-keyed bundle verifies");
}

/// **One value on two rows of one view is refused**, naming how many values and up to ten.
#[test]
fn one_value_twice_in_one_view_is_refused_naming_the_count_and_the_values() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut keys = keys();
    // Twelve values held twice: more than the ten a refusal lists.
    for i in 0..12 {
        keys[N - 1 - i] = keys[i].clone();
    }
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys.clone()))),
    );
    let said = build(&args(dir, &joined("keyword"), &dir.join("bundle")))
        .expect_err("a value held twice is refused")
        .to_string();
    assert!(said.contains("12 join value(s)"), "{said}");
    let listed = keys[..12]
        .iter()
        .filter(|key| said.contains(key.as_str()))
        .count();
    assert_eq!(listed, 10, "up to ten values: {said}");
}

/// **A signed value held twice is named as it is written**, not as its two's-complement bits.
#[test]
fn a_negative_value_held_twice_is_named_as_written() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut ids: Vec<i64> = (0..N as i64).map(|i| i - 5).collect();
    ids[N - 1] = -1;
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(Int64Array::from(ids))),
    );
    let said = build(&args(dir, &joined("i64"), &dir.join("bundle")))
        .expect_err("a value held twice is refused")
        .to_string();
    assert!(said.contains(": -1."), "{said}");
}

/// **A member naming a value the points file does not carry is refused.**
#[test]
fn a_member_naming_an_unknown_value_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let keys = keys();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys.clone()))),
    );
    let mut named = keys.clone();
    named[0] = "doc-99".to_string();
    write_members(
        &dir.join("members.parquet"),
        Arc::new(StringArray::from(named)),
    );
    let said = build(&args(
        dir,
        &(joined("keyword") + LAYER),
        &dir.join("bundle"),
    ))
    .expect_err("a member naming no item is refused")
    .to_string();
    assert!(said.contains("doc-99"), "{said}");
}

/// **A members table holding the join value at another type family is refused once**, naming
/// both types.
#[test]
fn a_members_column_in_the_other_family_is_refused_naming_both_types() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(UInt64Array::from(integers())),
    );
    let said = build(&args(
        dir,
        &(joined("keyword") + LAYER),
        &dir.join("bundle"),
    ))
    .expect_err("a members column in the other family is refused")
    .to_string();
    assert!(said.contains("UInt64"), "{said}");
    assert!(said.contains("Utf8"), "{said}");
}

/// **A membership written beside an artifact names items**, and without a join field there is
/// nothing for it to name. Both spellings are refused, naming the layer and, where there is one,
/// the file.
#[test]
fn a_membership_beside_an_artifact_is_refused_without_a_join_field() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(&dir.join("points.parquet"), None);
    write_artifacts(&dir.join("clusters.parquet"));
    let said = build(&args(
        dir,
        &(unjoined() + ARTIFACT_LAYER),
        &dir.join("from-file"),
    ))
    .expect_err("an artifacts file carrying a membership is refused")
    .to_string();
    assert!(said.contains("clusters/a"), "{said}");
    assert!(said.contains("clusters.parquet"), "{said}");
    assert!(said.contains("join_field"), "{said}");

    let said = build(&args(
        dir,
        &(unjoined() + INLINE_LAYER),
        &dir.join("inline"),
    ))
    .expect_err("an inline membership is refused")
    .to_string();
    assert!(said.contains("clusters/a"), "{said}");
    assert!(said.contains("join_field"), "{said}");
}

/// **A points file that stores Morton codes builds without a join field.**
#[test]
fn a_morton_points_file_without_a_join_field_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(
        &dir.join("points.parquet"),
        vec![
            Field::new("morton", DataType::UInt64, false),
            Field::new("visibility", DataType::Utf8, false),
            Field::new("flag", DataType::Int64, false),
        ],
        vec![
            Arc::new(UInt64Array::from(
                (0..N)
                    .map(|i| (i as u64 * 131) % 65_536)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec!["public"; N])),
            Arc::new(Int64Array::from(
                (0..N).map(|i| (i % 3) as i64).collect::<Vec<_>>(),
            )),
        ],
    );
    let out = dir.join("bundle");
    let mut args = args(dir, &unjoined(), &out);
    args.views[0].extent = tessera_build::input::IDENTITY_EXTENT;
    let report = build(&args).expect("a Morton points file with no join field builds");
    assert_eq!(report.items, N as u64);
}

/// **Without a join field each row of the points file is an item**, even where the file carries
/// a column a join field could have named.
#[test]
fn without_a_join_field_each_row_is_an_item() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut keys = keys();
    keys[1] = keys[0].clone();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys))),
    );
    let report = build(&args(dir, &unjoined(), &dir.join("bundle")))
        .expect("a points file with no join field builds");
    assert_eq!(report.items, N as u64);
}
