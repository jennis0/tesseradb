//! **A build reads its files under the rule an ingest reads a batch under**, in declaration order.
//!
//! A row names the item holding each unique value it carries. A points row naming none creates an
//! item, numbered in the order the files create them; a row of any other file naming none, a row
//! naming two items, and a later row naming an item or setting a value an earlier row of its file
//! did are refused, reported, and left out. The cases here are the rule's, each through the
//! declaration parser and the build, and each asserted on what the bundle stores.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::Config;
use tessera_build::{build, build_in_memory, BuildArgs, BuildReport, RefusedRows};
use tessera_filter::{Access, RecordBlob, RecordValue};
use tessera_spatial::Bounds;
use tessera_store::unique::{UniqueIndexes, UniqueKey};
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// A Parquet file of the named columns.
fn write(path: &Path, columns: Vec<(&str, ArrayRef)>) {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch =
        RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, a)| a).collect())
            .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn u64s(values: &[Option<u64>]) -> ArrayRef {
    Arc::new(UInt64Array::from(values.to_vec()))
}

fn strings(values: &[Option<&str>]) -> ArrayRef {
    Arc::new(StringArray::from(values.to_vec()))
}

fn i64s(values: &[i64]) -> ArrayRef {
    Arc::new(Int64Array::from(values.to_vec()))
}

/// Points at `a` values: positions spread over the frame, one per row.
fn points(path: &Path, a: &[Option<u64>], extra: Vec<(&str, ArrayRef)>) {
    let at = |i: usize| (i * 37 % 100) as f64;
    let mut columns = vec![
        ("a", u64s(a)),
        (
            "x",
            Arc::new(Float64Array::from((0..a.len()).map(at).collect::<Vec<_>>())) as ArrayRef,
        ),
        (
            "y",
            Arc::new(Float64Array::from(
                (0..a.len()).map(|i| at(i + 3)).collect::<Vec<_>>(),
            )),
        ),
    ];
    columns.extend(extra);
    write(path, columns);
}

/// Every build argument a declaration over plain views implies, each view's frame 0..100.
fn args(dir: &Path, declaration: &str, out: &Path) -> BuildArgs {
    let path = dir.join("corpus.toml");
    std::fs::write(&path, declaration).unwrap();
    let config = Config::parse(&path, &Default::default()).expect("the declaration parses");
    let registry = config.build_views().expect("the views compile");
    let anchor = config.anchor_view(&registry).expect("an anchor");
    let acquired = config.acquire().expect("the files acquire");
    let views: Vec<tessera_build::ViewArgs> = registry
        .iter()
        .map(|view| {
            let acquired = tessera_build::config::acquire_view(view).expect("the view acquires");
            tessera_build::ViewArgs {
                visibility: None,
                view_id: view.id.clone(),
                projection: view.projection,
                extent: Bounds {
                    x_min: 0.0,
                    x_max: 100.0,
                    y_min: 0.0,
                    y_max: 100.0,
                },
                points: acquired.points,
                point_fields: acquired.point_fields,
                select: acquired.select,
                access: acquired.access,
            }
        })
        .collect();
    let mut layers = config.layers.clone();
    for layer in &mut layers {
        layer.views = Config::expand_layer_views(&registry, &layer.views);
    }
    BuildArgs {
        views,
        anchor,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: acquired.attribute_sources,
        out: out.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        shard_id: 0,
        layers,
        layer_inputs: acquired.layers,
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    }
}

/// The items each of `keys` names in the unique column `attribute`.
fn holders(root: &Path, attribute: &str, keys: &[UniqueKey]) -> Vec<Vec<u32>> {
    let bundle = tessera_store::read::open_bundle(root).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let indexes = UniqueIndexes::open(
        &bundle.manifest,
        &partition.manifest,
        &root.join("v00000"),
        None,
    )
    .expect("the indexes open");
    let index = indexes.get(attribute).expect("the column is indexed");
    let mut out = vec![Vec::new(); keys.len()];
    for (at, entity) in index.lookup(keys).expect("the index reads") {
        out[at].push(entity);
    }
    out
}

/// The one item holding `value` in the u64 unique column `attribute`.
fn item(root: &Path, attribute: &str, value: u64) -> u32 {
    let held = holders(root, attribute, &[UniqueKey::unsigned(value)]);
    assert_eq!(held[0].len(), 1, "{attribute} = {value} names one item");
    held[0][0]
}

/// Each item's record, as `(declared position, value)`.
fn records(root: &Path) -> HashMap<u32, Vec<(u16, RecordValue)>> {
    let dir = root
        .join("v00000")
        .join("partitions")
        .join("default")
        .join("attrs")
        .join("record");
    let blob = RecordBlob::open_dir(&dir, Access::Read).expect("the blob opens");
    let bundle = tessera_store::read::open_bundle(root).expect("the bundle opens");
    (0..bundle.manifest.entity_id_high_water as u32)
        .map(|entity| {
            let fields = blob
                .fields_of(entity)
                .expect("a well-formed read")
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f.tag, f.value))
                .collect();
            (entity, fields)
        })
        .collect()
}

/// The report's rows for one object and reason.
fn refused<'a>(report: &'a BuildReport, object: &str, reason: &str) -> Option<&'a RefusedRows> {
    report
        .refused
        .iter()
        .find(|entry| entry.object == object && entry.reason == reason)
}

/// Build `declaration` in `dir` both ways and hold the two bundles and reports equal.
fn both_ways(dir: &Path, declaration: &str) -> (PathBuf, BuildReport) {
    let streamed = dir.join("streamed");
    let linear = dir.join("linear");
    let report = build(&args(dir, declaration, &streamed)).expect("the streaming build runs");
    let reference = build_in_memory(&args(dir, declaration, &linear)).expect("the linear build runs");
    assert_eq!(report.refused, reference.refused, "both builds refuse the same rows");
    common::assert_bundles_identical(&streamed, &linear, "streaming against linear");
    (streamed, report)
}

const ONE_VIEW: &str = r#"
[sources]
points = "points.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "a"
type   = "u64"
unique = true
"#;

// -------------------------------------------------------------------------------------------

/// **(a) One unique field across two views, an attribute file and a members file.** A value in
/// both views' points is one item in both, and the other files name items by it.
#[test]
fn one_unique_field_names_one_item_across_every_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("world.parquet"), &[Some(1), Some(2), Some(3)], Vec::new());
    points(&dir.join("near.parquet"), &[Some(3), Some(4)], Vec::new());
    write(
        &dir.join("scores.parquet"),
        vec![
            ("a", u64s(&[Some(4), Some(3), Some(2), Some(1)])),
            ("score", i64s(&[40, 30, 20, 10])),
        ],
    );
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", strings(&[Some("k"), Some("k"), Some("k")])),
            ("entity", u64s(&[Some(1), Some(3), Some(4)])),
        ],
    );
    let declaration = r#"
[sources]
world   = "world.parquet"
near    = "near.parquet"
scores  = "scores.parquet"
members = "members.parquet"

[defaults]
source          = "world"
allocation_view = "world"

[[view]]
name             = "world"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[view]]
name             = "near"
source           = "near"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "a"
type   = "u64"
unique = true

[[attribute]]
name   = "score"
type   = "i64"
source = "scores"

[[layer]]
name                      = "groups"
views                     = ["world", "near"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

  [layer.members]
  source = "members"
  fields = { a = "entity" }
"#;
    let out = dir.join("bundle");
    let report = build(&args(dir, declaration, &out)).expect("the build succeeds");
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.items, 4, "a value in both views is one item");
    let rows: BTreeMap<&str, u64> = report
        .views
        .iter()
        .map(|view| (view.view_id.as_str(), view.rows))
        .collect();
    assert_eq!(rows, BTreeMap::from([("near", 2), ("world", 3)]));
    let records = records(&out);
    for (a, score) in [(1, 10), (2, 20), (3, 30), (4, 40)] {
        let fields = &records[&item(&out, "a", a)];
        assert!(
            fields.contains(&(1, RecordValue::I64(score))),
            "a = {a} holds its own score: {fields:?}"
        );
    }
    assert_eq!(report.minted_artifacts, 1);
}

/// **(b) A value twice in one view's points**: the first row is the item and the later ones are
/// refused, reported once with the value; the value in a second view's points puts that item in
/// the second view.
#[test]
fn a_value_twice_in_one_view_keeps_the_first_row() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(
        &dir.join("points.parquet"),
        &[Some(1), Some(2), Some(2), Some(3), Some(2)],
        vec![("tag", i64s(&[10, 20, 21, 30, 22]))],
    );
    let declaration =
        format!("{ONE_VIEW}\n[[attribute]]\nname = \"tag\"\ntype = \"i64\"\n");
    let (out, report) = both_ways(dir, &declaration);
    assert_eq!(report.items, 3);
    let entry = refused(&report, "view 's0'", "one_value_twice").expect("the later rows");
    assert_eq!(entry.rows, 2);
    assert_eq!(entry.values, vec!["a = 2".to_string()]);
    let records = records(&out);
    assert!(
        records[&item(&out, "a", 2)].contains(&(1, RecordValue::I64(20))),
        "the first row of the value is the item"
    );

    // The same value in a second view's points names the item there.
    points(&dir.join("near.parquet"), &[Some(2)], Vec::new());
    let two_views = declaration.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nnear   = \"near.parquet\"\n",
    ) + "\n[[view]]\nname = \"near\"\nsource = \"near\"\nextent = { min = 0.0, max = 100.0 }\n\
         point_visibility = { default = \"public\" }\n";
    let two_views = two_views.replace(
        "source = \"points\"\n",
        "source = \"points\"\nallocation_view = \"s0\"\n",
    );
    let out = dir.join("two-views");
    let report = build(&args(dir, &two_views, &out)).expect("the build succeeds");
    assert_eq!(report.items, 3, "the second view names the item and creates none");
    let near = report.views.iter().find(|v| v.view_id == "near").unwrap();
    assert_eq!(near.rows, 1);
}

/// **`--strict` refuses the build at the first file with a refused row** instead of leaving the
/// row out.
#[test]
fn strict_refuses_the_build_at_the_first_refused_row() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(1)], Vec::new());
    let mut strict = args(dir, ONE_VIEW, &dir.join("strict"));
    strict.strict = true;
    assert!(build(&strict).is_err(), "the streaming build refuses");
    let mut strict = args(dir, ONE_VIEW, &dir.join("strict-linear"));
    strict.strict = true;
    assert!(build_in_memory(&strict).is_err(), "and so does the linear one");
    let report = build(&args(dir, ONE_VIEW, &dir.join("lenient"))).expect("without it, it builds");
    assert_eq!(report.items, 1);
}

/// **(c) A null in the points creates an item without the value**, and the item is in the
/// bundle beside the ones that carry theirs.
#[test]
fn a_null_unique_value_creates_an_item_without_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(
        &dir.join("points.parquet"),
        &[Some(1), None, Some(3), None],
        vec![("tag", i64s(&[10, 20, 30, 40]))],
    );
    let declaration =
        format!("{ONE_VIEW}\n[[attribute]]\nname = \"tag\"\ntype = \"i64\"\n");
    let (out, report) = both_ways(dir, &declaration);
    assert!(report.refused.is_empty(), "a null names nothing and collides with nothing");
    assert_eq!(report.items, 4);
    let records = records(&out);
    let without: Vec<i64> = records
        .values()
        .filter(|fields| !fields.iter().any(|(tag, _)| *tag == 0))
        .filter_map(|fields| {
            fields.iter().find_map(|(tag, value)| match (tag, value) {
                (1, RecordValue::I64(v)) => Some(*v),
                _ => None,
            })
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(without, vec![20, 40], "the rows with no value are items of their own");
}

/// **(d) An attribute file's row naming no item, and a later row naming an item an earlier row
/// named, are refused**; the item keeps the first row's value.
#[test]
fn an_attribute_row_naming_no_item_or_an_item_twice_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2), Some(3)], Vec::new());
    write(
        &dir.join("scores.parquet"),
        vec![
            ("a", u64s(&[Some(1), Some(99), Some(2), Some(2), Some(3)])),
            ("score", i64s(&[10, 990, 20, 21, 30])),
        ],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nscores = \"scores.parquet\"\n",
    ) + "\n[[attribute]]\nname = \"score\"\ntype = \"i64\"\nsource = \"scores\"\n";
    let (out, report) = both_ways(dir, &declaration);
    let object = "attribute source 'scores'";
    let none = refused(&report, object, "names_no_item").expect("the row naming 99");
    assert_eq!((none.rows, none.values.clone()), (1, vec!["a = 99".to_string()]));
    let twice = refused(&report, object, "one_item_twice").expect("the second row of 2");
    assert_eq!(twice.rows, 1);
    let records = records(&out);
    assert!(records[&item(&out, "a", 2)].contains(&(1, RecordValue::I64(20))));
}

/// **(e) A member naming no item is refused, and its artifact holds the rest.**
#[test]
fn a_member_naming_no_item_is_refused_and_the_artifact_holds_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2), Some(3)], Vec::new());
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", strings(&[Some("k"), Some("k"), Some("k")])),
            ("a", u64s(&[Some(1), Some(99), Some(3)])),
        ],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nmembers = \"members.parquet\"\n",
    ) + r#"
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
"#;
    let (_out, report) = both_ways(dir, &declaration);
    let entry =
        refused(&report, "layer 'groups' members", "names_no_item").expect("the member 99");
    assert_eq!((entry.rows, entry.values.clone()), (1, vec!["a = 99".to_string()]));
    assert_eq!(report.minted_artifacts, 1, "the artifact is published with the rest");
}

/// **(f) Two unique fields.** The points create (a=1, b=p) and (a=2, b=y). In the attribute file
/// (a=1, b=y) names both items and is refused, and (a=1, b=q) names the first and sets its `b`.
/// A second view's (a=3, b=r) creates an item. So `b = p` names nothing, `b = q` the first item.
#[test]
fn two_unique_fields_name_edit_and_refuse_by_the_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(
        &dir.join("points.parquet"),
        &[Some(1), Some(2)],
        vec![("b", strings(&[Some("p"), Some("y")]))],
    );
    points(
        &dir.join("near.parquet"),
        &[Some(3)],
        vec![("b", strings(&[Some("r")]))],
    );
    write(
        &dir.join("notes.parquet"),
        vec![
            ("a", u64s(&[Some(1), Some(1), Some(2), Some(3)])),
            ("b", strings(&[Some("y"), Some("q"), None, None])),
            ("note", i64s(&[0, 1, 2, 3])),
        ],
    );
    let declaration = r#"
[sources]
points = "points.parquet"
near   = "near.parquet"
notes  = "notes.parquet"

[defaults]
source          = "points"
allocation_view = "s0"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[view]]
name             = "near"
source           = "near"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "a"
type   = "u64"
unique = true

[[attribute]]
name   = "b"
type   = "keyword"
unique = true

[[attribute]]
name   = "note"
type   = "i64"
source = "notes"
"#;
    let out = dir.join("bundle");
    let report = build(&args(dir, declaration, &out)).expect("the build succeeds");
    assert_eq!(report.items, 3);
    let two = refused(&report, "attribute source 'notes'", "names_two_items")
        .expect("(a=1, b=y) names two items");
    assert_eq!(two.rows, 1);
    let b = |value: &str| holders(&out, "b", &[UniqueKey::keyword(value)]).remove(0);
    assert_eq!(b("p"), Vec::<u32>::new(), "the first item's `b` is no longer p");
    assert_eq!(b("q"), vec![item(&out, "a", 1)], "(a=1, b=q) set it");
    assert_eq!(b("y"), vec![item(&out, "a", 2)]);
    assert_eq!(b("r"), vec![item(&out, "a", 3)], "the second view created (a=3, b=r)");
}

/// **(f), one view**: the same edits and refusals in a corpus the linear build takes, so the two
/// builds are held to one bundle over them.
#[test]
fn two_unique_fields_build_one_bundle_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(
        &dir.join("points.parquet"),
        &[Some(1), Some(2), Some(3), Some(4)],
        vec![("b", strings(&[Some("p"), Some("y"), Some("r"), Some("p")]))],
    );
    write(
        &dir.join("notes.parquet"),
        vec![
            ("a", u64s(&[Some(1), Some(1), Some(2), Some(3), None])),
            ("b", strings(&[Some("y"), Some("q"), None, None, Some("r")])),
            ("note", i64s(&[0, 1, 2, 3, 4])),
        ],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nnotes  = \"notes.parquet\"\n",
    ) + "\n[[attribute]]\nname = \"b\"\ntype = \"keyword\"\nunique = true\n\
         \n[[attribute]]\nname = \"note\"\ntype = \"i64\"\nsource = \"notes\"\n";
    let (out, report) = both_ways(dir, &declaration);
    // (a=4, b=p) sets a value the first row already set, so it is the later of two.
    assert_eq!(
        refused(&report, "view 's0'", "one_value_twice").map(|e| e.rows),
        Some(1)
    );
    assert_eq!(
        refused(&report, "attribute source 'notes'", "names_two_items").map(|e| e.rows),
        Some(1)
    );
    // (b=r) alone names the item a=3, which an earlier row named.
    assert_eq!(
        refused(&report, "attribute source 'notes'", "one_item_twice").map(|e| e.rows),
        Some(1)
    );
    let b = |value: &str| holders(&out, "b", &[UniqueKey::keyword(value)]).remove(0);
    assert_eq!(b("q"), vec![item(&out, "a", 1)]);
    assert_eq!(b("p"), Vec::<u32>::new());
}

/// **(g) A file whose rows address items and that carries no unique column is refused**, at the
/// build and at `tessera check`, in one sentence.
#[test]
fn a_file_with_nothing_to_name_items_by_is_refused_by_build_and_check() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2)], Vec::new());
    write(
        &dir.join("scores.parquet"),
        vec![("score", i64s(&[10, 20]))],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nscores = \"scores.parquet\"\n",
    ) + "\n[[attribute]]\nname = \"score\"\ntype = \"i64\"\nsource = \"scores\"\n";
    let sentence = tessera_lifecycle::resolve::NoIdentifier.to_string();
    let said = build(&args(dir, &declaration, &dir.join("bundle")))
        .expect_err("the build refuses")
        .to_string();
    assert!(said.contains(&sentence), "{said}");
    let config = Config::parse(&dir.join("corpus.toml"), &Default::default()).unwrap();
    let report = tessera_build::check::check(&config);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.detail.contains(&sentence)),
        "{:?}",
        report.findings
    );
}

/// A generator for [`the_sort_merge_decides_what_the_rule_decides`]: small value ranges, so values
/// repeat, collide across fields and go missing.
struct Draws(u64);

impl Draws {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % below
    }

    fn a(&mut self, range: u64) -> Option<u64> {
        (self.next(10) > 0).then(|| self.next(range))
    }

    fn b(&mut self, range: u64) -> Option<String> {
        (self.next(10) > 1).then(|| format!("b{}", self.next(range)))
    }
}

/// One random corpus of `rows` points over two unique fields, an attribute file and a members file.
fn random_corpus(dir: &Path, seed: u64, rows: usize) -> String {
    let mut draws = Draws(seed);
    let range = rows as u64;
    let a: Vec<Option<u64>> = (0..rows).map(|_| draws.a(range)).collect();
    let b: Vec<Option<String>> = (0..rows).map(|_| draws.b(range)).collect();
    points(
        &dir.join("points.parquet"),
        &a,
        vec![("b", strings(&b.iter().map(Option::as_deref).collect::<Vec<_>>()))],
    );
    let notes = rows / 2 + 1;
    let na: Vec<Option<u64>> = (0..notes).map(|_| draws.a(range + range / 4)).collect();
    let nb: Vec<Option<String>> = (0..notes).map(|_| draws.b(range + range / 4)).collect();
    write(
        &dir.join("notes.parquet"),
        vec![
            ("a", u64s(&na)),
            ("b", strings(&nb.iter().map(Option::as_deref).collect::<Vec<_>>())),
            ("note", i64s(&(0..notes as i64).collect::<Vec<_>>())),
        ],
    );
    let members = rows;
    let ma: Vec<Option<u64>> = (0..members).map(|_| draws.a(range + range / 4)).collect();
    let keys: Vec<String> = (0..members).map(|_| format!("k{}", draws.next(5))).collect();
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", strings(&keys.iter().map(|k| Some(k.as_str())).collect::<Vec<_>>())),
            ("a", u64s(&ma)),
        ],
    );
    // The notes file need not cover every item: its own column is unique, so it owes none a row.
    ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nnotes = \"notes.parquet\"\nmembers = \"members.parquet\"\n",
    ) + r#"
[[attribute]]
name   = "b"
type   = "keyword"
unique = true

[[attribute]]
name   = "note"
type   = "i64"
unique = true
source = "notes"

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
"#
}

/// **The streaming build's sort-merge decides every row as the rule does**: over random corpora
/// with repeated, colliding and missing values in two fields, an attribute file that edits and
/// names items and a members file, it builds the bundle and refuses the rows the linear build does
/// by asking [`tessera_lifecycle::resolve::resolve`] file by file.
#[test]
fn the_sort_merge_decides_what_the_rule_decides() {
    for seed in 1..=12u64 {
        eprintln!("seed {seed}");
        let tmp = tempfile::tempdir().unwrap();
        let declaration = random_corpus(tmp.path(), seed, 60);
        let (_, report) = both_ways(tmp.path(), &declaration);
        assert!(!report.refused.is_empty(), "seed {seed} plants refusals");
    }
}

/// **The same where a field's sort spills to disk**: at a 128 MiB budget a sort holds about half a
/// million keyword values, and this corpus carries more.
#[test]
fn the_sort_merge_decides_what_the_rule_decides_when_it_spills() {
    let tmp = tempfile::tempdir().unwrap();
    let declaration = random_corpus(tmp.path(), 99, 800_000);
    let streamed = tmp.path().join("streamed");
    let linear = tmp.path().join("linear");
    let small = |out: &Path| BuildArgs {
        memory_budget: Some(128 << 20),
        ..args(tmp.path(), &declaration, out)
    };
    let report = build(&small(&streamed)).expect("the streaming build runs");
    let reference = build_in_memory(&small(&linear)).expect("the linear build runs");
    common::assert_bundles_identical(&streamed, &linear, "streaming against linear");
    assert_eq!(report.refused, reference.refused);
}

/// **An id's value does not order items; the order the files create them in does.** The same rows
/// named by dense ids in file order and by sparse ids in no order give each row the same entity
/// and the same place in every view.
#[test]
fn creation_order_and_not_an_ids_value_numbers_items() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let n = 300u64;
    let dense: Vec<Option<u64>> = (0..n).map(Some).collect();
    let sparse: Vec<Option<u64>> = (0..n)
        .map(|i| Some((i * 7919 % n) * 1_000_003 + 17))
        .collect();
    let built = |ids: &[Option<u64>], name: &str| {
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).unwrap();
        points(
            &sub.join("points.parquet"),
            ids,
            vec![("tag", i64s(&(0..n as i64).collect::<Vec<_>>()))],
        );
        let declaration =
            format!("{ONE_VIEW}\n[[attribute]]\nname = \"tag\"\ntype = \"i64\"\nrender = true\n");
        let out = sub.join("bundle");
        build(&args(&sub, &declaration, &out)).expect("the build succeeds");
        out
    };
    let a = built(&dense, "dense");
    let b = built(&sparse, "sparse");
    let view = |root: &Path, file: &str| {
        std::fs::read(root.join("v00000/partitions/default/views/s0").join(file)).unwrap()
    };
    for file in ["permutation.bin", tessera_store::ROW_ENTITY_FILE] {
        assert_eq!(view(&a, file), view(&b, file), "{file}");
    }
}

/// **A unique attribute needs no source of its own**: its values come from every file that
/// carries its column, so a declaration naming none for it, and no default, builds with them.
#[test]
fn a_unique_attribute_with_no_source_takes_its_values_from_the_points() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(5), Some(9)], Vec::new());
    let declaration = ONE_VIEW
        .replace("[defaults]\nsource = \"points\"\n", "")
        .replace(
            "extent           = { min = 0.0, max = 100.0 }\n",
            "extent           = { min = 0.0, max = 100.0 }\nsource           = \"points\"\n",
        );
    let (root, report) = both_ways(dir, &declaration);
    assert_eq!(report.items, 2);
    assert_ne!(item(&root, "a", 5), item(&root, "a", 9));
}

/// **`--limit` leaves out the members naming items it left out, and reports them as outside it**:
/// not refused, so `--strict` builds.
#[test]
fn a_member_outside_the_limit_is_reported_and_not_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2), Some(30), Some(40)], Vec::new());
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", strings(&[Some("k"), Some("k"), Some("k")])),
            ("a", u64s(&[Some(1), Some(30), Some(40)])),
        ],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nmembers = \"members.parquet\"\n",
    ) + r#"
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
"#;
    let limited = |out: &str| BuildArgs {
        limit: Some(10),
        strict: true,
        ..args(dir, &declaration, &dir.join(out))
    };
    let report = build(&limited("streamed")).expect("rows outside the limit refuse nothing");
    let reference = build_in_memory(&limited("linear")).expect("the linear build agrees");
    common::assert_bundles_identical(&dir.join("streamed"), &dir.join("linear"), "limited");
    assert_eq!(report.refused, reference.refused);
    assert_eq!(report.items, 2);
    let outside = refused(&report, "layer 'groups' members", "outside_limit").expect("reported");
    assert_eq!(outside.rows, 2);
    assert!(!outside.is_refusal());
}

/// **A member naming no item still refuses a `--strict` build under `--limit`**: an id below the
/// limit that no points row holds is not outside it.
#[test]
fn a_member_below_the_limit_naming_no_item_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2), Some(30)], Vec::new());
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", strings(&[Some("k"), Some("k"), Some("k")])),
            ("a", u64s(&[Some(1), Some(30), Some(7)])),
        ],
    );
    let declaration = ONE_VIEW.replace(
        "points = \"points.parquet\"\n",
        "points = \"points.parquet\"\nmembers = \"members.parquet\"\n",
    ) + r#"
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
"#;
    let limited = |out: &str, strict: bool| BuildArgs {
        limit: Some(10),
        strict,
        ..args(dir, &declaration, &dir.join(out))
    };
    assert!(build(&limited("strict", true)).is_err());
    let report = build(&limited("lenient", false)).expect("without --strict it builds");
    let no_item = refused(&report, "layer 'groups' members", "names_no_item").expect("reported");
    assert_eq!(no_item.rows, 1);
}

/// **A member a layer lists is outside `--limit` as a members file's row is**: reported as outside
/// it, not refused, where a listed id below the limit that no row holds is refused.
#[test]
fn a_listed_member_outside_the_limit_is_reported_as_a_members_row_is() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    points(&dir.join("points.parquet"), &[Some(1), Some(2), Some(30)], Vec::new());
    let declaration = ONE_VIEW.to_string()
        + r#"
[[layer]]
name                      = "picked"
views                     = ["s0"]
membership                = "enumerated"
value_set                 = "closed"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"
artifacts                 = [{ key = "p", members = { a = [1, 30, 7] } }]
"#;
    let limited = |out: &str| BuildArgs {
        limit: Some(10),
        ..args(dir, &declaration, &dir.join(out))
    };
    let report = build(&limited("streamed")).expect("the streaming build runs");
    let reference = build_in_memory(&limited("linear")).expect("the linear build runs");
    assert_eq!(report.refused, reference.refused);
    let object = "layer 'picked' memberships";
    assert_eq!(refused(&report, object, "outside_limit").map(|e| e.rows), Some(1));
    assert_eq!(refused(&report, object, "names_no_item").map(|e| e.rows), Some(1));
}
