//! The sort-merge numbers every file as the rule does, over the files a linear build refuses: two
//! views of a group beside a plain one, a later view's points naming items the first created, an
//! attribute file, each view's rows of a group-scoped attribute's file, the access relation, a
//! members file and a layer's inline lists.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, Int64Array, StringArray, UInt32Array};
use arrow::array::UInt64Array;
use arrow::datatypes::{Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

use super::{reads, ReadInput};
use crate::config::Config;
use crate::row_groups::FileGroups;

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

/// A generator with small value ranges, so values repeat, collide across fields and go missing.
struct Draws(u64);

impl Draws {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % below
    }

    fn a(&mut self, range: u64, rows: usize) -> ArrayRef {
        let values: Vec<Option<u64>> = (0..rows)
            .map(|_| (self.next(10) > 0).then(|| self.next(range)))
            .collect();
        Arc::new(UInt64Array::from(values))
    }

    fn b(&mut self, range: u64, rows: usize) -> ArrayRef {
        let values: Vec<Option<String>> = (0..rows)
            .map(|_| (self.next(10) > 1).then(|| format!("b{}", self.next(range))))
            .collect();
        Arc::new(StringArray::from(values))
    }
}

fn positions(rows: usize) -> Vec<(&'static str, ArrayRef)> {
    let at = |i: usize| (i * 37 % 100) as f64;
    vec![
        ("x", Arc::new(Float64Array::from_iter_values((0..rows).map(at))) as ArrayRef),
        ("y", Arc::new(Float64Array::from_iter_values((0..rows).map(|i| at(i + 11))))),
    ]
}

const DECLARATION: &str = r#"
[sources]
world   = "world.parquet"
q2      = "q2.parquet"
q3      = "q3.parquet"
notes   = "notes.parquet"
scores  = "scores.parquet"
pairs   = "pairs.parquet"
members = "members.parquet"

[defaults]
source          = "world"
allocation_view = "world"

[[view]]
name             = "world"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { source = "pairs", default = "public" }

[[view_group]]
name             = "quarter"
extent           = { min = 0.0, max = 100.0 }
visibility       = "public"
point_visibility = { source = "pairs", default = "public" }

[[view_group.view]]
key    = "q2"
source = "q2"

[[view_group.view]]
key    = "q3"
source = "q3"

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
unique = true
source = "notes"

[[attribute]]
name   = "score"
type   = "f32"
scope  = { group = "quarter" }
source = "scores"
fields = { view = "quarter" }

[[layer]]
name                      = "groups"
views                     = ["world"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

  [layer.members]
  source = "members"

[[layer]]
name                      = "picked"
views                     = ["world"]
membership                = "enumerated"
value_set                 = "closed"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"
artifacts                 = [
  { key = "p", members = { a = [1, 2, 3, 900, 4, 5, 2] } },
  { key = "r", members = { b = ["b1", "b7", "b1"] } },
]
"#;

fn corpus(dir: &Path, seed: u64, rows: usize) {
    let mut draws = Draws(seed);
    let range = rows as u64;
    for (name, count) in [("world", rows), ("q2", rows / 2), ("q3", rows / 2)] {
        let mut columns = vec![("a", draws.a(range, count)), ("b", draws.b(range, count))];
        columns.extend(positions(count));
        write(&dir.join(format!("{name}.parquet")), columns);
    }
    let notes = rows / 2;
    write(
        &dir.join("notes.parquet"),
        vec![
            ("a", draws.a(range + range / 4, notes)),
            ("b", draws.b(range + range / 4, notes)),
            ("note", Arc::new(Int64Array::from_iter_values(0..notes as i64))),
        ],
    );
    let scores = rows;
    let quarters: Vec<&str> = (0..scores).map(|_| ["q2", "q3"][draws.next(2) as usize]).collect();
    write(
        &dir.join("scores.parquet"),
        vec![
            ("a", draws.a(range, scores)),
            ("b", draws.b(range, scores)),
            ("quarter", Arc::new(StringArray::from(quarters))),
            ("score", Arc::new(Float32Array::from_iter_values((0..scores).map(|i| i as f32)))),
        ],
    );
    let pairs = rows * 2;
    write(
        &dir.join("pairs.parquet"),
        vec![
            ("a", draws.a(range + range / 4, pairs)),
            ("term_id", Arc::new(UInt32Array::from_iter_values((0..pairs).map(|i| (i % 3) as u32)))),
        ],
    );
    let members = rows;
    let keys: Vec<String> = (0..members).map(|_| format!("k{}", draws.next(5))).collect();
    write(
        &dir.join("members.parquet"),
        vec![
            ("key", Arc::new(StringArray::from(keys))),
            ("b", draws.b(range + range / 4, members)),
        ],
    );
    std::fs::write(dir.join("corpus.toml"), DECLARATION).unwrap();
}

/// The build's arguments for the declaration in `dir`, as `tessera build` assembles them.
fn args(dir: &Path, budget: Option<u64>) -> crate::BuildArgs {
    let config = Config::parse(&dir.join("corpus.toml"), &HashMap::new()).expect("parses");
    let registry = config.build_views().expect("the views compile");
    let anchor = config.anchor_view(&registry).expect("an anchor");
    let acquired = config.acquire().expect("the files acquire");
    let views: Vec<crate::ViewArgs> = registry
        .iter()
        .map(|view| {
            let acquired = crate::config::acquire_view(view).expect("the view acquires");
            crate::ViewArgs {
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
    let scoped_attributes = config
        .scoped_attributes
        .iter()
        .map(|scoped| crate::ScopedColumnFamily {
            attribute: scoped.attribute.clone(),
            group: scoped.group.clone(),
            views: registry
                .iter()
                .enumerate()
                .filter(|(_, view)| view.group.as_ref().is_some_and(|g| g.group == scoped.group))
                .map(|(index, _)| index)
                .collect(),
            source: scoped.source.clone(),
        })
        .collect();
    let groups = config.group_registry(&registry, &views);
    let mut layers = config.layers.clone();
    for layer in &mut layers {
        layer.views = Config::expand_layer_views(&registry, &layer.views);
    }
    crate::BuildArgs {
        views,
        anchor,
        groups,
        scoped_attributes,
        attribute_sources: acquired.attribute_sources,
        out: dir.join("out"),
        limit: None,
        strict: false,
        identity_key: IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap(),
        shard_id: 0,
        layers,
        layer_inputs: acquired.layers,
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: budget,
        band_rows: None,
        schema: config.schema,
    }
}

/// Every read's numbers, row by row, and what each numbering refused.
fn numbers_of(args: &crate::BuildArgs, numbering: &super::Numbering) -> Vec<Vec<u64>> {
    reads(args)
        .unwrap()
        .iter()
        .map(|read| {
            let len = match &read.input {
                ReadInput::File { path, .. } => FileGroups::open(path).unwrap().rows() as usize,
                ReadInput::Lists(lists) => lists.len(),
            };
            let rows = numbering.of(read.kind).expect("every read is numbered");
            let mut out = Vec::with_capacity(len);
            rows.numbers.extend(0, len, &mut out);
            out
        })
        .collect()
}

fn same_numbers(seed: u64, rows: usize, budget: Option<u64>) {
    same_numbers_within(seed, rows, budget, None)
}

/// [`same_numbers`], built with `--limit` where `limit` is given: `note` is then no longer unique,
/// so `a` is the one unique integer the limit is over.
fn same_numbers_within(seed: u64, rows: usize, budget: Option<u64>, limit: Option<u64>) {
    let tmp = tempfile::tempdir().unwrap();
    corpus(tmp.path(), seed, rows);
    if limit.is_some() {
        let declaration = DECLARATION.replace(
            "name   = \"note\"\ntype   = \"i64\"\nunique = true\n",
            "name   = \"note\"\ntype   = \"i64\"\n",
        );
        std::fs::write(tmp.path().join("corpus.toml"), declaration).unwrap();
    }
    let args = crate::BuildArgs {
        limit,
        ..args(tmp.path(), budget)
    };
    let kinds: Vec<super::ReadKind> = reads(&args).unwrap().iter().map(|read| read.kind).collect();
    use super::ReadKind::*;
    assert_eq!(
        kinds,
        [
            Points(0),
            Points(1),
            Points(2),
            Attributes(1),
            Scoped { family: 0, view: 1 },
            Scoped { family: 0, view: 2 },
            Access,
            Members(0),
            Lists(1),
        ]
    );
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let streamed = super::stream::number(&args, &scratch).expect("the sort-merge numbers");
    let linear = super::linear::number(&args).expect("the rule numbers");
    assert_eq!(streamed.items, linear.items, "seed {seed}: items");
    assert_eq!(numbers_of(&args, &streamed), numbers_of(&args, &linear), "seed {seed}");
    assert_eq!(streamed.refused, linear.refused, "seed {seed}: refused");
    assert!(!streamed.refused.is_empty(), "seed {seed} plants refusals");
}

#[test]
fn the_sort_merge_numbers_every_kind_of_file_as_the_rule_does() {
    for seed in 1..=10 {
        same_numbers(seed, 60, None);
    }
}

/// At the smallest sort budget a keyword field's sort holds about 65,000 values, so this corpus's
/// spill to disk.
#[test]
fn the_sort_merge_numbers_as_the_rule_does_when_its_sorts_spill() {
    same_numbers(77, 200_000, Some(16 << 20));
}

/// **Under `--limit` the sort-merge decides each row by the item it names, as the rule does**: in
/// every kind of file, with values that repeat, go missing, move an item's `a` past the limit and
/// name items the limit left out.
#[test]
fn the_sort_merge_numbers_as_the_rule_does_within_a_limit() {
    for seed in 1..=10 {
        for limit in [0, 20, 45, 1 << 40] {
            same_numbers_within(seed, 60, None, Some(limit));
        }
    }
    same_numbers_within(78, 200_000, Some(16 << 20), Some(120_000));
}

const SCOPED: &str = r#"
[sources]
world  = "world.parquet"
q2     = "q2.parquet"
q3     = "q3.parquet"
scores = "scores.parquet"

[defaults]
source          = "world"
allocation_view = "world"

[[view]]
name             = "world"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[view_group]]
name             = "quarter"
extent           = { min = 0.0, max = 100.0 }
visibility       = "public"
point_visibility = { default = "public" }

[[view_group.view]]
key    = "q2"
source = "q2"

[[view_group.view]]
key    = "q3"
source = "q3"

[[attribute]]
name   = "a"
type   = "u64"
unique = true

[[attribute]]
name   = "b"
type   = "keyword"
unique = true

[[attribute]]
name   = "score"
type   = "f32"
scope  = { group = "quarter" }
source = "scores"
fields = { view = "quarter" }
"#;

/// **A group-scoped attribute's file sets the unique values its rows carry**, as an ingest naming
/// each view does: a value of `b` only that file carries is the item's, in the built index.
#[test]
fn a_group_scoped_file_sets_the_unique_values_it_carries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let a = |values: &[u64]| -> ArrayRef { Arc::new(UInt64Array::from(values.to_vec())) };
    for (name, ids) in [("world", vec![1u64, 2, 3]), ("q2", vec![1, 2]), ("q3", vec![3])] {
        let mut columns = vec![("a", a(&ids))];
        if name == "world" {
            columns.push(("b", Arc::new(StringArray::from(vec![None::<&str>; ids.len()]))));
        }
        columns.extend(positions(ids.len()));
        write(&dir.join(format!("{name}.parquet")), columns);
    }
    write(
        &dir.join("scores.parquet"),
        vec![
            ("a", a(&[1, 3])),
            ("b", Arc::new(StringArray::from(vec!["z1", "z3"]))),
            ("quarter", Arc::new(StringArray::from(vec!["q2", "q3"]))),
            ("score", Arc::new(Float32Array::from(vec![0.5f32, 0.25]))),
        ],
    );
    std::fs::write(dir.join("corpus.toml"), SCOPED).unwrap();
    let args = args(dir, None);
    crate::build(&args).expect("the build runs");

    let root = dir.join("out");
    let bundle = tessera_store::read::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let indexes = tessera_store::unique::UniqueIndexes::open(
        &bundle.manifest,
        &partition.manifest,
        &root.join("v00000"),
        None,
    )
    .expect("the indexes open");
    let holder = |attribute: &str, key: tessera_store::unique::UniqueKey| -> Vec<u32> {
        let index = indexes.get(attribute).expect("indexed");
        index.lookup(&[key]).expect("reads").into_iter().map(|(_, e)| e).collect()
    };
    use tessera_store::unique::UniqueKey;
    for (id, value) in [(1u64, "z1"), (3, "z3")] {
        let by_a = holder("a", UniqueKey::unsigned(id));
        assert_eq!(by_a.len(), 1);
        assert_eq!(holder("b", UniqueKey::keyword(value)), by_a, "b = {value}");
    }
}

const ALIGNED: &str = r#"
[sources]
world   = "world.parquet"
members = "members.parquet"

[defaults]
source = "world"

[[view]]
name             = "world"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "a"
type   = "u64"
unique = true

[[layer]]
name                      = "groups"
views                     = ["world"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

  [layer.members]
  source = "members"
"#;

/// `columns` as a Parquet file of row groups of `rows_per_group` rows.
fn write_grouped(path: &Path, columns: Vec<(&str, ArrayRef)>, rows_per_group: usize) {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch =
        RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, a)| a).collect())
            .unwrap();
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(rows_per_group))
        .build();
    let mut writer =
        ArrowWriter::try_new(File::create(path).unwrap(), schema, Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// One points file and one members file keyed by `a`: the points' values from `points`, and each
/// members row the value at its own position with probability `aligned` in 20, else a moved,
/// repeated, null or unheld one.
fn aligned_corpus(dir: &Path, seed: u64, points: &[Option<u64>], aligned: u64, groups: (usize, usize)) {
    let mut draws = Draws(seed);
    let rows = points.len();
    let mut columns = vec![("a", Arc::new(UInt64Array::from(points.to_vec())) as ArrayRef)];
    columns.extend(positions(rows));
    write_grouped(&dir.join("world.parquet"), columns, groups.0);
    let members: Vec<Option<u64>> = (0..rows + 40)
        .map(|row| match draws.next(20) {
            roll if roll < aligned => points.get(row).copied().flatten().or(Some(9)),
            roll if roll == aligned => None,
            roll if roll == aligned + 1 => Some(draws.next(1 << 40) * 7 + 5),
            _ => points[draws.next(rows as u64) as usize],
        })
        .collect();
    let keys: Vec<String> = (0..members.len()).map(|_| format!("k{}", draws.next(4))).collect();
    write_grouped(
        &dir.join("members.parquet"),
        vec![
            ("key", Arc::new(StringArray::from(keys))),
            ("a", Arc::new(UInt64Array::from(members))),
        ],
        groups.1,
    );
    std::fs::write(dir.join("corpus.toml"), ALIGNED).unwrap();
}

fn same_as_the_rule(dir: &Path, trial: u64, what: &str) {
    let args = args(dir, None);
    let scratch = dir.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let streamed = super::stream::number_with(&args, &scratch, trial).expect("the sort-merge numbers");
    let linear = super::linear::number(&args).expect("the rule numbers");
    assert_eq!(numbers_of(&args, &streamed), numbers_of(&args, &linear), "{what}");
    assert_eq!(streamed.refused, linear.refused, "{what}: refused");
}

/// **A members file in the points' own order is compared with them row by row**, and numbers
/// what the rule does: rows at their own position, rows moved, repeated, null, naming no item and
/// past the points' last row alike, across row groups of different sizes in the two files.
#[test]
fn a_members_file_in_the_points_order_numbers_as_the_rule_does() {
    let rows = 5_000usize;
    // Distinct values in the points, so every points row creates an item with its value.
    let points: Vec<Option<u64>> = (0..rows as u64).map(|i| Some(i * 7 + 3)).collect();
    for seed in 1..=4u64 {
        for groups in [(97, 131), (1 << 20, 64), (500, 500)] {
            let tmp = tempfile::tempdir().unwrap();
            aligned_corpus(tmp.path(), seed, &points, 16, groups);
            same_as_the_rule(tmp.path(), 1 << 20, &format!("seed {seed}, groups {groups:?}"));
        }
    }
}

/// **The comparison stops where fewer than half the first rows match, whichever row reaches the
/// trial's end**, and a file that stops being compared numbers as the rule does all the same.
#[test]
fn a_members_file_mostly_out_of_order_stops_being_compared() {
    let rows = 3_000usize;
    let points: Vec<Option<u64>> = (0..rows as u64).map(|i| Some(i * 7 + 3)).collect();
    for seed in 1..=3u64 {
        let tmp = tempfile::tempdir().unwrap();
        aligned_corpus(tmp.path(), seed, &points, 7, (256, 300));
        for trial in [1, 2, 3, 40, 41, 64, 1 << 20] {
            same_as_the_rule(tmp.path(), trial, &format!("seed {seed}, trial {trial}"));
        }
    }
}

/// **Points that refuse a row are not compared with**: their rows are not items `base + row`, so
/// the members file goes through the sort, and numbers as the rule does.
#[test]
fn points_refusing_a_row_are_not_compared_with() {
    let rows = 2_000usize;
    let mut points: Vec<Option<u64>> = (0..rows as u64).map(|i| Some(i * 7 + 3)).collect();
    points[700] = points[10];
    points[900] = None;
    for seed in 1..=3u64 {
        let tmp = tempfile::tempdir().unwrap();
        aligned_corpus(tmp.path(), seed, &points, 16, (128, 200));
        same_as_the_rule(tmp.path(), 1 << 20, &format!("seed {seed}"));
    }
}

/// **A points file rewritten with as many rows is not compared with as the one numbered**: its
/// length or modification time tells it apart.
#[test]
fn a_file_rewritten_with_as_many_rows_is_told_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("world.parquet");
    let values = |offset: u64| -> ArrayRef {
        Arc::new(UInt64Array::from_iter_values((0..100).map(|i| i + offset)))
    };
    write(&path, vec![("a", values(0))]);
    let stamp = super::stream::Stamp::of(&path).unwrap();
    assert!(stamp.check(&path, 100, 100).is_ok());
    assert!(stamp.check(&path, 100, 99).is_err());
    write(&path, vec![("a", values(1 << 40))]);
    let file = File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)).unwrap();
    assert!(stamp.check(&path, 100, 100).is_err());
}

/// **The identity pass's scratch stays within its forecast**: the most bytes its scratch directory
/// held, sampled while it numbers a corpus whose sorts spill, is no more than the modelled peak.
/// One corpus has keys in no order across every kind of file; the other has two views of keys in
/// file order, the second naming half the first's items and creating as many again, so a sort's
/// keys arrive sorted and the second view's rows name items.
#[test]
fn the_identity_scratch_stays_within_its_forecast() {
    let tmp = tempfile::tempdir().unwrap();
    corpus(tmp.path(), 79, 200_000);
    scratch_within_forecast(tmp.path());

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let rows = 300_000u64;
    for (name, first) in [("world", 0), ("near", rows / 2)] {
        let a: Vec<u64> = (first..first + rows).collect();
        let b: Vec<String> = a.iter().map(|v| format!("b{v:09}")).collect();
        let mut columns = vec![
            ("a", Arc::new(UInt64Array::from(a)) as ArrayRef),
            ("b", Arc::new(StringArray::from(b))),
        ];
        columns.extend(positions(rows as usize));
        write(&dir.join(format!("{name}.parquet")), columns);
    }
    std::fs::write(dir.join("corpus.toml"), SORTED).unwrap();
    scratch_within_forecast(dir);
}

const SORTED: &str = r#"
[sources]
world = "world.parquet"
near  = "near.parquet"

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
name   = "b"
type   = "keyword"
unique = true
"#;

const OUT_OF_ORDER: &str = r#"
[sources]
world = "world.parquet"
notes = "notes.parquet"

[defaults]
source = "world"

[[view]]
name             = "world"
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

/// **An attribute file carrying two fields that names items in no order numbers as the rule
/// does**, with its sorts spilling: rows naming an item two or three times far apart, by either
/// field or both, naming two items, and giving a value several rows give; and its scratch stays
/// within the forecast.
#[test]
fn an_attribute_file_naming_items_out_of_order_numbers_as_the_rule_does() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let items = 150_000u64;
    let a: Vec<u64> = (0..items).collect();
    let b: Vec<String> = a.iter().map(|v| format!("b{v:09}")).collect();
    let mut columns = vec![
        ("a", Arc::new(UInt64Array::from(a)) as ArrayRef),
        ("b", Arc::new(StringArray::from(b))),
    ];
    columns.extend(positions(items as usize));
    write(&dir.join("world.parquet"), columns);
    let mut draws = Draws(91);
    let rows = 2 * items as usize;
    let (mut a, mut b) = (Vec::with_capacity(rows), Vec::with_capacity(rows));
    for _ in 0..rows {
        let item = draws.next(items);
        let (by_a, by_b) = match draws.next(10) {
            0..=4 => (Some(item), Some(format!("b{item:09}"))),
            5 | 6 => (Some(item), None),
            7 => (None, Some(format!("b{item:09}"))),
            8 => (Some(item), Some(format!("c{}", draws.next(items / 4)))),
            _ => (Some(item), Some(format!("b{:09}", draws.next(items)))),
        };
        a.push(by_a);
        b.push(by_b);
    }
    write(
        &dir.join("notes.parquet"),
        vec![
            ("a", Arc::new(UInt64Array::from(a)) as ArrayRef),
            ("b", Arc::new(StringArray::from(b))),
            ("note", Arc::new(Int64Array::from_iter_values(0..rows as i64))),
        ],
    );
    std::fs::write(dir.join("corpus.toml"), OUT_OF_ORDER).unwrap();

    let args = args(dir, Some(16 << 20));
    let scratch = dir.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let streamed = super::stream::number(&args, &scratch).expect("the sort-merge numbers");
    let linear = super::linear::number(&args).expect("the rule numbers");
    assert_eq!(numbers_of(&args, &streamed), numbers_of(&args, &linear));
    assert_eq!(streamed.refused, linear.refused);
    let twice = |reason: &str| {
        streamed
            .refused
            .iter()
            .filter(|refused| refused.reason == reason)
            .map(|refused| refused.rows)
            .sum::<u64>()
    };
    assert!(twice(tessera_lifecycle::resolve::Refusal::ONE_ITEM_TWICE) > 0);
    assert!(twice(tessera_lifecycle::resolve::Refusal::ONE_VALUE_TWICE) > 0);
    std::fs::remove_dir_all(&scratch).unwrap();
    scratch_within_forecast(dir);
}

/// Number the corpus in `dir` at the smallest sort budget, sampling the scratch directory's bytes
/// throughout, and hold the most it held within the forecast.
fn scratch_within_forecast(dir: &Path) {
    fn bytes_under(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.file_type() {
                Ok(kind) if kind.is_dir() => bytes_under(&entry.path()),
                _ => entry.metadata().map_or(0, |m| m.len()),
            })
            .sum()
    }
    let args = args(dir, Some(16 << 20));
    let scratch = dir.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let (forecast, _) = crate::residency::identity_disk(&args).unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let most = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut most = 0u64;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                most = most.max(bytes_under(&scratch));
            }
            most
        });
        let numbered = super::stream::number(&args, &scratch);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        numbered.expect("the pass numbers");
        sampler.join().unwrap()
    });
    assert!(most > 0, "the sampler saw the pass's scratch");
    assert!(most <= forecast, "{most} bytes of scratch against a forecast of {forecast}");
}

/// **A build numbering past the most items a bundle holds is refused**, at the item past it.
#[test]
fn numbering_past_the_item_cap_is_refused() {
    assert!(super::check_cap(super::ITEMS_MAX).is_ok());
    assert!(super::check_cap(super::ITEMS_MAX + 1).is_err());
}
