//! **Random sequences of writes, checked against a model after every step.** Each case builds a
//! small bundle with two views and a label layer whose one artifact carries a content generated
//! from two items, then runs a random sequence of ingest batches, change batches, flushes, folds,
//! restarts, resent batches and declarations of `unique` on and off one column. After every step
//! it compares what the service serves with what a model of the items says it should: every
//! view's points for two principals, `in` over every unique value, every item's card, the
//! artifact's content, and that no two items' `tessera_id`s name one entity. A second strategy
//! runs long sequences that edit the same items again and again, so that folds free the entities
//! the edits leave and later edits take them.
//!
//! An ingest row names an item by its `tessera_id`, its external id or a unique value, and carries
//! any of the item's fields, its label and a position in the batch's view. The model decides each
//! row the way the service must: a row naming nothing creates an item, one naming an item it
//! leaves unchanged is counted unchanged, one adding the item to a view it is not in is added, and
//! one that changes the item edits it. An item added to a view keeps its entity and stays served in
//! its other views, and is served in the new one from the next flush. An edit keeps the item's
//! `tessera_id` and its suppression, and hides the item in every view from its acknowledgement
//! until a flush places it again. A batch naming two items in one row,
//! one item in two rows, one value in two rows or a `tessera_id` nobody holds is refused whole.
//! A fold runs with whatever the buffer holds, so it can fall between an edit and its flush.
//!
//! Beside the unique columns, the items carry one column of each other family a row can compare:
//! a float, a timestamp, a boolean, a category and a text. A float is compared bit for bit.
//!
//! A restart drops the engine with rows acknowledged and not yet flushed, and opens it again from
//! the log. `PROPTEST_CASES` sets how many sequences run and `PROPTEST_RNG_SEED` which ones, a
//! fixed seed otherwise; a failing sequence is kept in `identity_model.proptest-regressions`
//! beside this file and run first thereafter.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{BooleanArray, Float32Array, Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use proptest::prelude::*;

use common::*;
use tessera_engine::filter::{FilterExpr, FilterOperand, Scalar};
use tessera_engine::{
    AcceptError, AttributeRequest, Engine, EngineConfig, IngestReceipt, IngestRequest, ScalarOut,
    Session, ViewportRequest,
};
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::{ChangeOp, IngestRow};
use tessera_spatial::tiler::ScalarType;
use tessera_types::layer::LayerScope;
use tessera_types::{EntityId, TesseraId};

const VIEWS: [&str; 2] = ["s0", "s1"];
/// The one key of the group `quarter`, whose view every built item is in beside [`VIEWS`].
const GROUP: (&str, &str) = ("quarter", "q1");
const GROUP_VIEW: &str = "quarter:q1";
/// The families scoped to the group, a float and a text: a row through [`GROUP_VIEW`] carries them
/// after the declared columns.
const SCOPED: [&str; 2] = ["heat", "memo"];
/// The declared columns: `gid`, `doi`, `score` and [`EXTRAS`].
const DECLARED: usize = 8;
const BUILT: u64 = 12;
const VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

const SCHEMA_TOML: &str = r#"
[[attribute]]
name   = "gid"
type   = "u64"
index  = true
unique = true

[[attribute]]
name   = "doi"
type   = "keyword"
unique = true

[[attribute]]
name   = "score"
type   = "f64"
render = true

[[vocabulary]]
name       = "kind"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  alpha = 1
  beta  = 2
  gamma = 3

[[attribute]]
name   = "weight"
type   = "f32"
render = true

[[attribute]]
name  = "when"
type  = "timestamp_us"
index = true

[[attribute]]
name = "flag"
type = "bool"

[[attribute]]
name       = "kind"
type       = "category"
index      = true
vocabulary = "kind"

[[attribute]]
name     = "note"
type     = "text"
index    = true
analyser = "unicode"
"#;

/// The columns after `gid`, `doi` and `score`, in declared order.
const EXTRAS: [&str; 5] = ["weight", "when", "flag", "kind", "note"];

/// Column `EXTRAS[at]`'s value for `seed`. The float takes a NaN and a negative zero among its
/// values, which compare bit for bit.
fn extra(at: usize, seed: u64) -> WalScalar {
    match at {
        0 => WalScalar::F32(match seed % 6 {
            4 => -0.0,
            5 => f32::NAN,
            n => n as f32 * 0.25,
        }),
        1 => WalScalar::TimestampUs(1_600_000_000_000_000 + (seed % 7) as i64 * 1_000),
        2 => WalScalar::Bool(seed.is_multiple_of(2)),
        3 => WalScalar::Utf8(["alpha", "beta", "gamma"][(seed % 3) as usize].to_string()),
        _ => WalScalar::Utf8(format!("note {}", seed % 5)),
    }
}

/// Family `SCOPED[at]`'s value for `seed`.
fn scoped_value(at: usize, seed: u64) -> WalScalar {
    match at {
        0 => WalScalar::F32((seed % 9) as f32 * 0.5),
        _ => WalScalar::Utf8(format!("memo w{}", seed % 5)),
    }
}

/// A built item's value in column `EXTRAS[at]`: never a NaN, which a build does not take.
fn built_extra(at: usize, source: u64) -> WalScalar {
    extra(at, source % 5)
}

/// Whether two held values are the same, a float bit for bit.
fn same(a: &Option<WalScalar>, b: &Option<WalScalar>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_as(b),
        (None, None) => true,
        _ => false,
    }
}

/// A value as an item's card shows it.
fn shown(value: &WalScalar) -> ScalarOut {
    match value {
        WalScalar::F32(x) => ScalarOut::F32(*x),
        WalScalar::TimestampUs(x) => ScalarOut::TimestampUs(*x),
        WalScalar::Bool(x) => ScalarOut::Bool(*x),
        WalScalar::Utf8(x) => ScalarOut::Utf8(x.clone()),
        other => panic!("no extra column holds {other:?}"),
    }
}

/// Whether a card's field is the value expected, a float bit for bit.
fn shows(field: &Option<ScalarOut>, value: &Option<WalScalar>) -> bool {
    match (field, value.as_ref().map(shown)) {
        (Some(ScalarOut::F32(a)), Some(ScalarOut::F32(b))) => a.to_bits() == b.to_bits(),
        (field, expected) => *field == expected,
    }
}

fn built_gid(source: u64) -> u64 {
    1_000 + source
}

fn built_doi(source: u64) -> String {
    format!("d{source}")
}

fn built_score(source: u64) -> f64 {
    source as f64 * 0.5
}

fn built_position(source: u64) -> (f64, f64) {
    (((source * 37) % 1000) as f64, ((source * 53) % 1000) as f64)
}

fn write_points(path: &Path) {
    let ids: Vec<u64> = (0..BUILT).collect();
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("gid", DataType::UInt64, false),
        Field::new("doi", DataType::Utf8, false),
        Field::new("score", DataType::Float64, false),
        Field::new("weight", DataType::Float32, false),
        Field::new("when", DataType::Int64, false),
        Field::new("flag", DataType::Boolean, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, false),
    ]));
    let extras = |at: usize| ids.iter().map(move |s| built_extra(at, *s));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|s| built_position(*s).0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|s| built_position(*s).1).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                ids.iter().map(|s| built_gid(*s)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|s| built_doi(*s)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|s| built_score(*s)).collect::<Vec<_>>(),
            )),
            Arc::new(Float32Array::from(
                extras(0)
                    .map(|v| match v {
                        WalScalar::F32(x) => x,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                extras(1)
                    .map(|v| match v {
                        WalScalar::TimestampUs(x) => x,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                extras(2)
                    .map(|v| match v {
                        WalScalar::Bool(x) => x,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                extras(3)
                    .map(|v| match v {
                        WalScalar::Utf8(x) => x,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                extras(4)
                    .map(|v| match v {
                        WalScalar::Utf8(x) => x,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Fixture {
    tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points);
    let group_points = tmp.path().join("group.parquet");
    write_group_points(&group_points);
    write_pairs_n(&pairs, BUILT);
    let schema_path = tmp.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the fixture schema parses")
        .schema;
    let view_args = |view: &str, points: &Path| tessera_build::ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.clone()),
    };
    let scoped = |at: usize| tessera_build::ScopedColumnFamily {
        attribute: tessera_build::config::Attribute {
            name: SCOPED[at].to_string(),
            title: None,
            field: None,
            ty: [ScalarType::F32, ScalarType::Text][at],
            analyser: (at == 1)
                .then(|| tessera_analyse::identity_of("unicode").expect("the analyser is carried")),
            vocabulary: None,
            value_set: None,
            index: true,
            render: false,
            unique: false,
        },
        group: GROUP.0.to_string(),
        views: vec![VIEWS.len()],
        source: None,
    };
    let e = extent();
    tessera_build::build(&tessera_build::BuildArgs {
        views: VIEWS
            .iter()
            .map(|view| view_args(view, &points))
            .chain([view_args(GROUP_VIEW, &group_points)])
            .collect(),
        anchor: 0,
        groups: vec![tessera_build::GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
            visibility: None,
            name: GROUP.0.to_string(),
            members_of: None,
            views: vec![tessera_build::GroupViewDescriptor {
                key: GROUP.1.to_string(),
                visibility: None,
                metadata: Default::default(),
            }],
            quantisation: tessera_build::Quantisation {
                x_min: e.x_min,
                x_max: e.x_max,
                y_min: e.y_min,
                y_max: e.y_max,
            },
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            scoped_scalars: Vec::new(),
        }],
        scoped_attributes: vec![scoped(0), scoped(1)],
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the build succeeds");
    Fixture { tmp, root }
}

/// The group's view: every built item at its position, with its value in each scoped family.
fn write_group_points(path: &Path) {
    let ids: Vec<u64> = (0..BUILT).collect();
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(SCOPED[0], DataType::Float32, false),
        Field::new(SCOPED[1], DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|s| built_position(*s).0),
            )),
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|s| built_position(*s).1),
            )),
            Arc::new(Float32Array::from_iter_values(ids.iter().map(
                |s| match built_scoped(0, *s) {
                    WalScalar::F32(x) => x,
                    _ => unreachable!(),
                },
            ))),
            Arc::new(StringArray::from_iter_values(ids.iter().map(
                |s| match built_scoped(1, *s) {
                    WalScalar::Utf8(x) => x,
                    _ => unreachable!(),
                },
            ))),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn built_scoped(at: usize, source: u64) -> WalScalar {
    scoped_value(at, source * 3 + 1)
}

fn open(fx: &Fixture) -> Engine {
    let mut engine = Engine::open(
        &fx.root,
        &fx.tmp.path().join("cache"),
        &fx.tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            ..config_uncapped()
        },
    )
    .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

// ---- the model ----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Item {
    tid: u64,
    external_id: Option<Vec<u8>>,
    labels: BTreeSet<String>,
    gid: Option<u64>,
    doi: Option<String>,
    score: Option<f64>,
    /// The item's value in each of [`EXTRAS`].
    extras: [Option<WalScalar>; 5],
    /// Its value in each of [`SCOPED`] under the group's key.
    scoped: [Option<WalScalar>; 2],
    /// Each view the item has a row in, with its position there.
    views: BTreeMap<String, (f64, f64)>,
    suppressed: bool,
    /// Its entity's place in creation order, which is the order of the engine's entity ids: an
    /// edit gives the item a new one.
    made: u64,
}

#[derive(Debug, Clone, Default)]
struct Model {
    /// Every live or suppressed item, by `tessera_id`.
    items: BTreeMap<u64, Item>,
    /// `tessera_id`s of deleted items, which name nothing.
    deleted: BTreeSet<u64>,
    /// The `(tessera_id, view)` rows a flush has given a position.
    flushed: BTreeSet<(u64, String)>,
    doi_unique: bool,
    /// Whether [`GROUP_VIEW`] has not been dropped.
    group: bool,
    /// Items made so far, built ones included.
    made: u64,
    /// Each view's `point_visibility.default`, the label of an item created without one.
    defaults: BTreeMap<String, Option<String>>,
    /// The items the artifact's content was generated from.
    content_from: BTreeSet<u64>,
    /// A delete took one of them, which the fold may withdraw the content for.
    content_lost: bool,
}

/// One row as the model reads it: what identifies an item, and what it carries. `None` is a
/// field the row leaves out, `Some(None)` one it sends as null.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    tid: Option<u64>,
    external_id: Option<Vec<u8>>,
    gid: Option<Option<u64>>,
    doi: Option<Option<String>>,
    score: Option<Option<f64>>,
    extras: [Option<Option<WalScalar>>; 5],
    scoped: [Option<Option<WalScalar>>; 2],
    labels: Option<BTreeSet<String>>,
    position: Option<(f64, f64)>,
}

/// What the model expects of one row of an accepted batch.
#[derive(Debug, Clone, PartialEq)]
enum Expect {
    Create,
    Unchanged(u64),
    /// The item joins the batch's view, keeping its entity.
    Added(u64),
    Edited(u64),
}

impl Model {
    fn built(engine: &Engine, root: &Path) -> Model {
        let mut model = Model {
            doi_unique: true,
            group: true,
            defaults: engine
                .meta()
                .views
                .iter()
                .map(|v| (v.id.clone(), v.point_default.clone()))
                .collect(),
            made: BUILT,
            ..Model::default()
        };
        for (source, entity) in source_to_new_map(root, "v00000") {
            let tid = engine.tessera_id_of(EntityId::new(entity)).unwrap().raw();
            let position = built_position(source);
            let labels = terms_of(source).iter().map(|t| t.to_string()).collect();
            model.items.insert(
                tid,
                Item {
                    tid,
                    external_id: Some(source_id_key(source)),
                    labels,
                    gid: Some(built_gid(source)),
                    doi: Some(built_doi(source)),
                    score: Some(built_score(source)),
                    extras: std::array::from_fn(|at| Some(built_extra(at, source))),
                    scoped: std::array::from_fn(|at| Some(built_scoped(at, source))),
                    views: VIEWS
                        .iter()
                        .chain([&GROUP_VIEW])
                        .map(|v| (v.to_string(), position))
                        .collect(),
                    suppressed: false,
                    made: entity,
                },
            );
            for view in VIEWS.iter().chain([&GROUP_VIEW]) {
                model.flushed.insert((tid, view.to_string()));
            }
        }
        model
    }

    /// The items each row's identifying values name.
    fn named(&self, row: &Row) -> Result<BTreeSet<u64>, ()> {
        let mut named = BTreeSet::new();
        if let Some(tid) = row.tid {
            if !self.items.contains_key(&tid) {
                return Err(());
            }
            named.insert(tid);
        }
        for item in self.items.values() {
            let by_external = row.external_id.is_some() && row.external_id == item.external_id;
            let by_gid = matches!(row.gid, Some(Some(g)) if item.gid == Some(g));
            let by_doi = self.doi_unique
                && matches!(&row.doi, Some(Some(d)) if item.doi.as_ref() == Some(d));
            if by_external || by_gid || by_doi {
                named.insert(item.tid);
            }
        }
        Ok(named)
    }

    /// What the service must do with a batch: `None` where it refuses it.
    fn decide(&self, view: Option<&str>, rows: &[Row]) -> Option<Vec<Expect>> {
        if view == Some(GROUP_VIEW) && !self.group {
            return None;
        }
        let mut named: Vec<Option<u64>> = Vec::new();
        for row in rows {
            let items = self.named(row).ok()?;
            if items.len() > 1 {
                return None;
            }
            named.push(items.into_iter().next());
        }
        let mut seen_items = BTreeSet::new();
        for tid in named.iter().flatten() {
            if !seen_items.insert(*tid) {
                return None;
            }
        }
        let mut values: BTreeSet<String> = BTreeSet::new();
        for row in rows {
            let mut set = Vec::new();
            if let Some(e) = &row.external_id {
                set.push(format!("x:{e:?}"));
            }
            if let Some(Some(g)) = row.gid {
                set.push(format!("g:{g}"));
            }
            if let (true, Some(Some(d))) = (self.doi_unique, &row.doi) {
                set.push(format!("d:{d}"));
            }
            for value in set {
                if !values.insert(value) {
                    return None;
                }
            }
        }
        let mut expect = Vec::new();
        for (row, item) in rows.iter().zip(&named) {
            let Some(tid) = item else {
                let unlabelled = row.labels.as_ref().is_none_or(|l| l.is_empty());
                let default = view.and_then(|v| self.defaults[v].as_ref());
                if row.position.is_none() || (unlabelled && default.is_none()) {
                    return None;
                }
                expect.push(Expect::Create);
                continue;
            };
            let item = &self.items[tid];
            let differs = row.external_id.is_some() && row.external_id != item.external_id
                || row.gid.is_some_and(|g| g != item.gid)
                || row.doi.as_ref().is_some_and(|d| *d != item.doi)
                || row.score.is_some_and(|s| s != item.score)
                || row
                    .extras
                    .iter()
                    .zip(&item.extras)
                    .any(|(sent, held)| sent.as_ref().is_some_and(|v| !same(v, held)))
                || row.labels.as_ref().is_some_and(|l| *l != item.labels);
            // A scoped value fills an empty cell where the row adds the item to the view, and
            // otherwise differs from no value.
            let adding =
                view.is_some_and(|v| row.position.is_some() && !item.views.contains_key(v));
            let scoped_differs =
                row.scoped
                    .iter()
                    .zip(&item.scoped)
                    .any(|(sent, held)| match (sent, held) {
                        (None, _) | (Some(None), None) => false,
                        (Some(Some(_)), None) => !adding,
                        (Some(sent), held) => !same(sent, held),
                    });
            let differs = differs || scoped_differs;
            let mut joins = false;
            let mut moves = false;
            if let (Some(view), Some(position)) = (view, row.position) {
                match item.views.get(view) {
                    Some(held) if *held == position => {}
                    Some(_) => moves = true,
                    None => joins = true,
                }
            }
            // A scoped value needs a row in the view.
            let unplaced = view.is_none_or(|v| !item.views.contains_key(v)) && !joins;
            if differs && unplaced && row.scoped.iter().any(|v| matches!(v, Some(Some(_)))) {
                return None;
            }
            expect.push(match (differs || moves, joins) {
                (true, _) => Expect::Edited(*tid),
                (false, true) => Expect::Added(*tid),
                (false, false) => Expect::Unchanged(*tid),
            });
        }
        Some(expect)
    }

    /// Apply an accepted batch whose rows answered `tessera_ids`.
    fn apply(&mut self, view: Option<&str>, rows: &[Row], expect: &[Expect], tessera_ids: &[u64]) {
        for ((row, expect), tid) in rows.iter().zip(expect).zip(tessera_ids) {
            match expect {
                Expect::Create => {
                    let view = view.expect("a create names a view").to_string();
                    let labels = match &row.labels {
                        Some(labels) if !labels.is_empty() => labels.clone(),
                        _ => self.defaults[&view].iter().cloned().collect(),
                    };
                    let item = Item {
                        tid: *tid,
                        external_id: row.external_id.clone(),
                        labels,
                        gid: row.gid.flatten(),
                        doi: row.doi.clone().flatten(),
                        score: row.score.flatten(),
                        extras: row.extras.clone().map(Option::flatten),
                        scoped: row.scoped.clone().map(Option::flatten),
                        views: BTreeMap::from([(view, row.position.unwrap())]),
                        suppressed: false,
                        made: self.made,
                    };
                    self.made += 1;
                    assert!(
                        !self.items.contains_key(tid) && !self.deleted.contains(tid),
                        "a new item was given tessera_id {tid}, which another item has held"
                    );
                    self.items.insert(*tid, item);
                }
                Expect::Unchanged(named) => assert_eq!(tid, named),
                Expect::Added(named) => {
                    assert_eq!(tid, named, "a row adding an item answers its tessera_id");
                    let view = view.expect("a row adding an item names a view").to_string();
                    let item = self.items.get_mut(named).unwrap();
                    item.views.insert(view, row.position.unwrap());
                    for (held, sent) in item.scoped.iter_mut().zip(&row.scoped) {
                        if let Some(Some(sent)) = sent {
                            *held = Some(sent.clone());
                        }
                    }
                }
                Expect::Edited(named) => {
                    assert_eq!(tid, named, "an edit never changes an item's tessera_id");
                    let item = self.items.get_mut(named).unwrap();
                    if let Some(labels) = &row.labels {
                        item.labels = labels.clone();
                    }
                    if row.external_id.is_some() {
                        item.external_id = row.external_id.clone();
                    }
                    if let Some(gid) = row.gid {
                        item.gid = gid;
                    }
                    if let Some(doi) = &row.doi {
                        item.doi = doi.clone();
                    }
                    if let Some(score) = row.score {
                        item.score = score;
                    }
                    for (held, sent) in item.extras.iter_mut().zip(&row.extras) {
                        if let Some(sent) = sent {
                            *held = sent.clone();
                        }
                    }
                    for (held, sent) in item.scoped.iter_mut().zip(&row.scoped) {
                        if let Some(sent) = sent {
                            *held = sent.clone();
                        }
                    }
                    if let (Some(view), Some(position)) = (view, row.position) {
                        item.views.insert(view.to_string(), position);
                    }
                    self.moved(*named);
                }
            }
        }
    }

    /// An item moved to a new entity: the newest, and in no view until a flush places it.
    fn moved(&mut self, tid: u64) {
        let made = self.made;
        self.made += 1;
        let item = self.items.get_mut(&tid).unwrap();
        item.made = made;
        for view in item.views.keys() {
            self.flushed.remove(&(tid, view.clone()));
        }
    }

    /// The views that exist: [`VIEWS`], and [`GROUP_VIEW`] until it is dropped.
    fn views(&self) -> Vec<&'static str> {
        let group = self.group.then_some(GROUP_VIEW);
        VIEWS.iter().copied().chain(group).collect()
    }

    fn live(&self) -> Vec<u64> {
        self.items.keys().copied().collect()
    }

    /// Every principal holds `public` beside the terms its credential grants.
    fn visible_to(&self, item: &Item, labels: &[&str]) -> bool {
        !item.suppressed
            && (item.labels.contains("public") || labels.iter().any(|l| item.labels.contains(*l)))
    }
}

// ---- generated operations -----------------------------------------------------------------------

/// One generated row. `about` picks an existing item the row is about, or a new one; the rest
/// decide how the row names it and what it carries, each read against the model when run.
#[derive(Debug, Clone)]
struct RowGen {
    about: Option<u16>,
    /// 0 `tessera_id`, 1 gid, 2 doi, 3 external id, 4 a `tessera_id` nobody holds.
    by: u8,
    /// Bits: 1 gid, 2 doi, 4 score, 8 labels, 16 external id.
    carry: u8,
    /// Bits of carried fields given a value other than the item's.
    differ: u8,
    /// Bits of carried fields sent as null.
    null: u8,
    /// Bits of [`EXTRAS`] the row carries, those given another value, and those sent as null; the
    /// two above them are [`SCOPED`], carried only through [`GROUP_VIEW`].
    extra_carry: u8,
    extra_differ: u8,
    extra_null: u8,
    /// 0 none, 1 the item's own position in the view where it has one, 2 a new position.
    position: u8,
    seed: u16,
}

#[derive(Debug, Clone)]
enum Op {
    /// A batch into view 0 or 1, naming no view (2), or into [`GROUP_VIEW`] (3).
    Ingest {
        view: u8,
        rows: Vec<RowGen>,
    },
    /// `(item, op)`: 0 delete, 1 suppress, 2 unsuppress.
    Changes(Vec<(u16, u8)>),
    Flush,
    Fold,
    /// A fold with a batch into view 0 or 1 sent while it is in flight and left unflushed, so the
    /// log is kept from before the fold's publication.
    PinnedFold {
        view: u8,
        rows: Vec<RowGen>,
    },
    Restart,
    /// Send an earlier batch again, under its own batch id and body.
    Resend(u16),
    /// Declare `unique` on `doi`, or take it off.
    DoiUnique(bool),
    /// Drop [`GROUP_VIEW`], deleting the items it leaves in no view.
    DropView,
}

fn row_gen() -> impl Strategy<Value = RowGen> {
    (
        prop::option::weighted(0.75, any::<u16>()),
        prop_oneof![24 => 0u8..4, 1 => Just(4u8)],
        any::<u8>(),
        prop_oneof![12 => Just(0u8), 1 => any::<u8>()],
        prop_oneof![12 => Just(0u8), 1 => any::<u8>()],
        0u8..3,
        any::<u16>(),
        any::<u8>(),
        prop_oneof![6 => Just(0u8), 1 => any::<u8>()],
        prop_oneof![8 => Just(0u8), 1 => any::<u8>()],
    )
        .prop_map(
            |(about, by, carry, differ, null, position, seed, extra_carry, extra_differ, extra_null)| {
                RowGen {
                    about,
                    by,
                    carry,
                    differ,
                    null,
                    position,
                    seed,
                    extra_carry,
                    extra_differ,
                    extra_null,
                }
            },
        )
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        48 => (0u8..4, prop::collection::vec(row_gen(), 1..5))
            .prop_map(|(view, rows)| Op::Ingest { view, rows }),
        12 => prop::collection::vec((any::<u16>(), 0u8..3), 1..4).prop_map(Op::Changes),
        12 => Just(Op::Flush),
        4 => Just(Op::Fold),
        2 => (0u8..2, prop::collection::vec(row_gen(), 1..3))
            .prop_map(|(view, rows)| Op::PinnedFold { view, rows }),
        8 => Just(Op::Restart),
        8 => any::<u16>().prop_map(Op::Resend),
        4 => any::<bool>().prop_map(Op::DoiUnique),
        1 => Just(Op::DropView),
    ]
}

// ---- the run ------------------------------------------------------------------------------------

/// A sent batch, kept so it can be sent again.
struct Sent {
    batch_id: String,
    view: Option<String>,
    rows: Vec<IngestRow>,
    model_rows: Vec<Row>,
    tessera_ids: Vec<u64>,
}

struct Run {
    fx: Fixture,
    engine: Option<Engine>,
    model: Model,
    sent: Vec<Sent>,
    batches: u64,
    /// One past the highest entity an item has been given so far.
    highest: u64,
    /// How many times an item was given an entity below `highest`: an id a fold freed.
    reused: u64,
    /// Every entity a suppressed item has held.
    held_suppressed: BTreeSet<u64>,
    /// How many times an item not suppressed was given an id a suppressed item held before.
    reused_suppressed: u64,
}

fn hash_of(batch_id: &str) -> [u8; 32] {
    let mut hash = [0u8; 32];
    let n = batch_id.len().min(32);
    hash[..n].copy_from_slice(&batch_id.as_bytes()[..n]);
    hash
}

impl Run {
    fn engine(&self) -> &Engine {
        self.engine.as_ref().unwrap()
    }

    /// The model row and the wire row a generated row makes, read against the model.
    fn build_row(&self, view: Option<&str>, gen: &RowGen) -> (Row, IngestRow) {
        let live = self.model.live();
        let subject = gen
            .about
            .filter(|_| !live.is_empty())
            .map(|i| &self.model.items[&live[i as usize % live.len()]]);
        let seed = u64::from(gen.seed);
        let fresh_gid = 5_000 + seed % 24;
        let fresh_doi = format!("n{}", seed % 24);
        let fresh_score = (seed % 7) as f64;
        let fresh_labels: BTreeSet<String> = if seed % 3 == 0 {
            ["0", "1"].iter().map(|s| s.to_string()).collect()
        } else {
            ["0"].iter().map(|s| s.to_string()).collect()
        };
        let fresh_external = format!("x{}", seed % 24).into_bytes();
        let fresh_position = (
            ((seed * 37) % 1000) as f64,
            ((seed * 53 + 11) % 1000) as f64,
        );

        let mut carry = gen.carry;
        let mut row = Row {
            tid: None,
            external_id: None,
            gid: None,
            doi: None,
            score: None,
            extras: Default::default(),
            scoped: Default::default(),
            labels: None,
            position: None,
        };
        match (subject, gen.by) {
            (_, 4) => row.tid = Some(u64::from(gen.seed) * 7_919 + 1),
            (Some(item), 0) => row.tid = Some(item.tid),
            (Some(_), 1) => carry |= 1,
            (Some(_), 2) => carry |= 2,
            (Some(_), _) => carry |= 16,
            (None, _) => carry |= 8,
        }
        let field = |bit: u8| {
            let carried = carry & bit != 0;
            let null = gen.null & bit != 0 && gen.by != bit.trailing_zeros() as u8 + 1;
            let differ = gen.differ & bit != 0 || subject.is_none();
            (carried, null, differ)
        };
        if let (true, null, differ) = field(1) {
            row.gid = Some(match (null, differ) {
                (true, _) => None,
                (false, true) => Some(fresh_gid),
                (false, false) => subject.and_then(|i| i.gid),
            });
        }
        if let (true, null, differ) = field(2) {
            row.doi = Some(match (null, differ) {
                (true, _) => None,
                (false, true) => Some(fresh_doi),
                (false, false) => subject.and_then(|i| i.doi.clone()),
            });
        }
        if let (true, null, differ) = field(4) {
            row.score = Some(match (null, differ) {
                (true, _) => None,
                (false, true) => Some(fresh_score),
                (false, false) => subject.and_then(|i| i.score),
            });
        }
        for at in 0..EXTRAS.len() {
            let bit = 1u8 << at;
            if gen.extra_carry & bit == 0 {
                continue;
            }
            row.extras[at] = Some(if gen.extra_null & bit != 0 {
                None
            } else if gen.extra_differ & bit != 0 || subject.is_none() {
                Some(extra(at, seed / 3 + at as u64))
            } else {
                subject.and_then(|i| i.extras[at].clone())
            });
        }
        if view == Some(GROUP_VIEW) {
            for at in 0..SCOPED.len() {
                let bit = 1u8 << (EXTRAS.len() + at);
                if gen.extra_carry & bit == 0 {
                    continue;
                }
                row.scoped[at] = Some(if gen.extra_null & bit != 0 {
                    None
                } else if gen.extra_differ & bit != 0 || subject.is_none() {
                    Some(scoped_value(at, seed / 5 + at as u64))
                } else {
                    subject.and_then(|i| i.scoped[at].clone())
                });
            }
        }
        if carry & 8 != 0 {
            row.labels = Some(match (subject, gen.differ & 8 != 0) {
                (Some(item), false) => item.labels.clone(),
                _ => fresh_labels,
            });
        }
        if carry & 16 != 0 {
            row.external_id = match subject {
                Some(item) if gen.differ & 16 == 0 => item.external_id.clone(),
                _ => Some(fresh_external),
            };
        }
        if let Some(view) = view {
            row.position = match gen.position {
                0 => None,
                1 => Some(
                    subject
                        .and_then(|i| i.views.get(view).copied())
                        .unwrap_or(fresh_position),
                ),
                _ => Some(fresh_position),
            };
        }

        let mut omitted = Vec::new();
        let mut scalars = Vec::new();
        for (at, value) in [
            row.gid.map(|g| g.map_or(WalScalar::Null, WalScalar::U64)),
            row.doi
                .clone()
                .map(|d| d.map_or(WalScalar::Null, WalScalar::Utf8)),
            row.score.map(|s| s.map_or(WalScalar::Null, WalScalar::F64)),
        ]
        .into_iter()
        .chain(
            row.extras
                .iter()
                .map(|v| v.clone().map(|v| v.unwrap_or(WalScalar::Null))),
        )
        .enumerate()
        {
            match value {
                Some(value) => scalars.push(value),
                None => {
                    omitted.push(at);
                    scalars.push(WalScalar::Null);
                }
            }
        }
        let wire = IngestRow {
            tessera_id: row.tid.map(TesseraId::new),
            external_id: row.external_id.clone(),
            labels: row
                .labels
                .as_ref()
                .map(|l| l.iter().map(|s| s.as_bytes().to_vec()).collect()),
            position: row.position,
            scalars,
            scoped: match view {
                Some(GROUP_VIEW) => row
                    .scoped
                    .iter()
                    .enumerate()
                    .map(|(at, value)| match value {
                        Some(value) => value.clone().unwrap_or(WalScalar::Null),
                        None => {
                            omitted.push(DECLARED + at);
                            WalScalar::Null
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            },
            omitted,
        };
        (row, wire)
    }

    fn send(
        &self,
        batch_id: &str,
        view: Option<&str>,
        rows: Vec<IngestRow>,
    ) -> Result<IngestReceipt, AcceptError> {
        self.engine()
            .ingest(IngestRequest {
                batch_id: batch_id.to_string(),
                body_hash: hash_of(batch_id),
                view: view.map(str::to_string),
                rows,
                artifacts: Default::default(),
            })
    }

    fn ingest(&mut self, view: u8, gens: &[RowGen]) {
        let view = match view {
            3 => Some(GROUP_VIEW),
            view => VIEWS.get(view as usize).copied(),
        };
        let (rows, wire): (Vec<Row>, Vec<IngestRow>) =
            gens.iter().map(|g| self.build_row(view, g)).unzip();
        self.batches += 1;
        let batch_id = format!("b{}", self.batches);
        let expected = self.model.decide(view, &rows);
        let answered = self.send(&batch_id, view, wire.clone());
        match (expected, answered) {
            (None, Err(e)) => assert!(
                matches!(
                    e,
                    AcceptError::Conflict(_)
                        | AcceptError::Contract(_)
                        | AcceptError::UnknownView { .. }
                ),
                "batch {batch_id} is refused as the model refuses it: {e:?}"
            ),
            (None, Ok(receipt)) => panic!(
                "batch {batch_id} into {view:?} was accepted and the model refuses it:\n\
                 rows {rows:#?}\nreceipt {receipt:?}"
            ),
            (Some(expect), Err(e)) => panic!(
                "batch {batch_id} into {view:?} was refused and the model accepts it as \
                 {expect:?}: {e}\nrows {rows:#?}"
            ),
            (Some(expect), Ok(receipt)) => {
                if std::env::var_os("MODEL_TRACE").is_some() {
                    eprintln!(
                        "TRACE {batch_id} view {view:?} expect {expect:?} receipt {:?}",
                        receipt.tessera_ids
                    );
                }
                self.check_receipt(&expect, &receipt, &batch_id);
                let ids: Vec<u64> = receipt.tessera_ids.iter().map(|t| t.raw()).collect();
                self.model.apply(view, &rows, &expect, &ids);
                self.renumber(&expect, &ids);
                self.sent.push(Sent {
                    batch_id,
                    view: view.map(str::to_string),
                    rows: wire,
                    model_rows: rows,
                    tessera_ids: ids,
                });
            }
        }
    }

    /// Give each item a batch created or moved the place its entity takes: a window assigns its
    /// new entities in the order of their labels' terms, not the order of the rows.
    fn renumber(&mut self, expect: &[Expect], tessera_ids: &[u64]) {
        let moved: Vec<u64> = expect
            .iter()
            .zip(tessera_ids)
            .filter(|(e, _)| matches!(e, Expect::Create | Expect::Edited(_)))
            .map(|(_, tid)| *tid)
            .collect();
        let ids: Vec<TesseraId> = moved.iter().map(|t| TesseraId::new(*t)).collect();
        let entities = self.engine().resolve_tessera_ids(&ids).unwrap();
        let highest = self.highest;
        for (tid, entity) in moved.iter().zip(entities) {
            let entity = entity.expect("an accepted row's item is named by its tessera_id");
            let item = self.model.items.get_mut(tid).unwrap();
            item.made = entity.raw();
            if entity.raw() < highest {
                self.reused += 1;
                if !item.suppressed && self.held_suppressed.contains(&entity.raw()) {
                    self.reused_suppressed += 1;
                }
            }
            if item.suppressed {
                self.held_suppressed.insert(entity.raw());
            }
            self.highest = self.highest.max(entity.raw() + 1);
        }
    }

    fn check_receipt(&self, expect: &[Expect], receipt: &IngestReceipt, batch_id: &str) {
        let count = |f: fn(&Expect) -> bool| expect.iter().filter(|e| f(e)).count() as u64;
        assert!(!receipt.replayed, "batch {batch_id} is new");
        assert_eq!(receipt.tessera_ids.len(), expect.len());
        assert_eq!(
            receipt.created,
            count(|e| matches!(e, Expect::Create)),
            "{batch_id}"
        );
        assert_eq!(
            receipt.added,
            count(|e| matches!(e, Expect::Added(_))),
            "{batch_id}"
        );
        assert_eq!(
            receipt.unchanged,
            count(|e| matches!(e, Expect::Unchanged(_))),
            "{batch_id}"
        );
        assert_eq!(
            receipt.edited,
            count(|e| matches!(e, Expect::Edited(_))),
            "{batch_id}"
        );
        for (expect, tid) in expect.iter().zip(&receipt.tessera_ids) {
            if let Expect::Unchanged(named) | Expect::Added(named) | Expect::Edited(named) = expect
            {
                assert_eq!(
                    tid.raw(),
                    *named,
                    "{batch_id} answers the named item's tessera_id"
                );
            }
        }
    }

    /// Send an earlier batch again: its batch id answers what it answered, or, where the log has
    /// let the id go, the batch is decided again as a new one.
    fn resend(&mut self, which: u16) {
        if self.sent.is_empty() {
            return;
        }
        let at = which as usize % self.sent.len();
        let (batch_id, view, rows, model_rows, ids) = {
            let sent = &self.sent[at];
            (
                sent.batch_id.clone(),
                sent.view.clone(),
                sent.rows.clone(),
                sent.model_rows.clone(),
                sent.tessera_ids.clone(),
            )
        };
        let answered = self.send(&batch_id, view.as_deref(), rows);
        let expected = self.model.decide(view.as_deref(), &model_rows);
        match answered {
            Ok(receipt) if receipt.replayed => {
                let got: Vec<u64> = receipt.tessera_ids.iter().map(|t| t.raw()).collect();
                assert_eq!(
                    got, ids,
                    "a replay of {batch_id} answers its first tessera_ids"
                );
            }
            Ok(receipt) => {
                let expect = expected.unwrap_or_else(|| {
                    panic!(
                        "{batch_id} sent again was accepted and the model refuses it:\n\
                         rows {model_rows:#?}\nreceipt {receipt:?}\nitems {:#?}",
                        self.model.items
                    )
                });
                self.check_receipt(&expect, &receipt, &batch_id);
                let got: Vec<u64> = receipt.tessera_ids.iter().map(|t| t.raw()).collect();
                self.model
                    .apply(view.as_deref(), &model_rows, &expect, &got);
                self.renumber(&expect, &got);
                // The batch id now answers this acceptance.
                self.sent[at].tessera_ids = got;
            }
            Err(e) => assert!(
                expected.is_none()
                    && matches!(
                        e,
                        AcceptError::Conflict(_)
                            | AcceptError::Contract(_)
                            | AcceptError::UnknownView { .. }
                    ),
                "{batch_id} sent again was refused and the model accepts it: {e:?}"
            ),
        }
    }

    fn changes(&mut self, targets: &[(u16, u8)]) {
        let live = self.model.live();
        if live.is_empty() {
            return;
        }
        let mut changes = Vec::new();
        for (target, op) in targets {
            let tid = live[*target as usize % live.len()];
            let op = [ChangeOp::Delete, ChangeOp::Suppress, ChangeOp::Unsuppress][*op as usize];
            let entity = self
                .engine()
                .resolve_tessera_ids(&[TesseraId::new(tid)])
                .unwrap()[0]
                .expect("a live item's tessera_id names it");
            changes.push((entity, op));
            match op {
                ChangeOp::Delete => {
                    if self.model.items.remove(&tid).is_some() {
                        self.model.deleted.insert(tid);
                        if self.model.content_from.contains(&tid) {
                            self.model.content_lost = true;
                        }
                    }
                }
                ChangeOp::Suppress | ChangeOp::Unsuppress => {
                    if let Some(item) = self.model.items.get_mut(&tid) {
                        item.suppressed = op == ChangeOp::Suppress;
                        if item.suppressed {
                            self.held_suppressed.insert(item.made);
                        }
                    }
                }
            }
        }
        self.engine()
            .accept_changes(changes)
            .expect("a change batch naming live items is accepted");
    }

    /// Publish every buffered row. The model marks rows flushed only here, so the wait leaves no
    /// flush request behind to publish the next step's rows before the model expects them.
    fn flush(&mut self) {
        publish_buffered(self.engine());
        for item in self.model.items.values() {
            for view in item.views.keys() {
                self.model.flushed.insert((item.tid, view.clone()));
            }
        }
    }

    fn restart(&mut self) {
        drop(self.engine.take());
        self.engine = Some(open(&self.fx));
    }

    fn doi_unique(&mut self, unique: bool) {
        let dois: Vec<&String> = self
            .model
            .items
            .values()
            .filter_map(|i| i.doi.as_ref())
            .collect();
        let distinct = dois.iter().collect::<BTreeSet<_>>().len() == dois.len();
        let answered = self.engine().declare_attribute(AttributeRequest {
            name: "doi".to_string(),
            title: None,
            ty: "keyword".to_string(),
            vocabulary: None,
            analyser: None,
            index: false,
            render: false,
            scope: LayerScope::Entity,
            unique,
        });
        match (unique, distinct, answered) {
            (true, false, Err(_)) => {}
            (_, _, Ok(_)) if !unique || distinct => self.model.doi_unique = unique,
            (_, _, answered) => panic!(
                "declaring doi unique={unique} over values distinct={distinct} answered \
                 {answered:?}"
            ),
        }
    }

    /// Drop the group's view. An item it leaves in no view, flushed or buffered, is deleted; the
    /// rest stay in their other views. The scoped values go with the key.
    fn drop_view(&mut self) {
        let answered = self
            .engine()
            .drop_view(GROUP.0.to_string(), GROUP.1.to_string());
        if !self.model.group {
            assert!(
                answered.is_err(),
                "a dropped view is dropped again: {answered:?}"
            );
            return;
        }
        let dropped = answered.expect("the view drops");
        self.model.group = false;
        let only_there: Vec<u64> = self
            .model
            .items
            .values()
            .filter(|item| item.views.keys().eq([GROUP_VIEW]))
            .map(|item| item.tid)
            .collect();
        assert_eq!(
            dropped.deleted,
            only_there.len() as u64,
            "the drop deletes the items it leaves in no view: {only_there:?}"
        );
        for tid in only_there {
            self.model.items.remove(&tid);
            self.model.deleted.insert(tid);
            if self.model.content_from.contains(&tid) {
                self.model.content_lost = true;
            }
        }
        for item in self.model.items.values_mut() {
            item.views.remove(GROUP_VIEW);
            item.scoped = Default::default();
        }
        self.model.flushed.retain(|(_, view)| view != GROUP_VIEW);
    }

    // ---- what is served ----------------------------------------------------------------------

    /// A viewport's points. `ProjectionBuilding` is the shed a fold's refresh window can answer
    /// with, which a caller retries, so it is retried here on a bounded deadline.
    fn served(&self, session: &Session, view: &str, filter: Option<FilterExpr>) -> BTreeSet<u64> {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let mut req = ViewportRequest::new(view, 0, VIEWPORT, 10_000);
            req.filter = filter.clone();
            match self.engine().viewport(session, req) {
                Ok(out) => return out.points.iter().map(|(id, _)| id.raw()).collect(),
                Err(tessera_engine::EngineError::ProjectionBuilding)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    panic!("the viewport of {view} neither answered nor kept shedding: {e:?}")
                }
            }
        }
    }

    fn check(&self, after: &str) {
        let engine = self.engine();
        let principals = [
            (
                engine.authorise(&full_coverage_credential()).unwrap(),
                vec!["0"],
            ),
            (engine.authorise(&subset_credential()).unwrap(), vec!["1"]),
        ];
        let model = &self.model;
        let gids: Vec<i128> = model
            .items
            .values()
            .filter_map(|i| i.gid)
            .map(i128::from)
            .chain([7, 999_999])
            .collect();
        let dois: Vec<String> = model
            .items
            .values()
            .filter_map(|i| i.doi.clone())
            .chain(["nobody".to_string()])
            .collect();
        for (session, labels) in &principals {
            for view in model.views() {
                let visible: BTreeSet<u64> = model
                    .items
                    .values()
                    .filter(|i| i.views.contains_key(view))
                    .filter(|i| model.flushed.contains(&(i.tid, view.to_string())))
                    .filter(|i| model.visible_to(i, labels))
                    .map(|i| i.tid)
                    .collect();
                let served = self.served(session, view, None);
                if served != visible {
                    let differ: Vec<(u64, Option<&Item>, bool)> = served
                        .symmetric_difference(&visible)
                        .map(|t| {
                            (
                                *t,
                                model.items.get(t),
                                model.flushed.contains(&(*t, view.to_string())),
                            )
                        })
                        .collect();
                    panic!(
                        "{view} for {labels:?}, after {after}: served {served:?}, expected \
                         {visible:?}; differing (tid, model item, flushed): {differ:#?}; made {}",
                        model.made
                    );
                }
                let with_gid: BTreeSet<u64> = visible
                    .iter()
                    .filter(|t| model.items[t].gid.is_some())
                    .copied()
                    .collect();
                let gid_in = FilterExpr::Leaf {
                    column: "gid".to_string(),
                    operand: FilterOperand::NumIn(gids.iter().map(|g| Scalar::Int(*g)).collect()),
                };
                assert_eq!(
                    self.served(session, view, Some(gid_in)),
                    with_gid,
                    "gid in, {view} for {labels:?}, after {after}"
                );
                if view == GROUP_VIEW {
                    for n in 0..5 {
                        let memo = WalScalar::Utf8(format!("memo w{n}"));
                        let with_memo: BTreeSet<u64> = visible
                            .iter()
                            .filter(|t| same(&model.items[t].scoped[1], &Some(memo.clone())))
                            .copied()
                            .collect();
                        let matches = FilterExpr::Leaf {
                            column: format!("{}@{GROUP_VIEW}", SCOPED[1]),
                            operand: FilterOperand::Match {
                                query: format!("w{n}"),
                                minimum: None,
                            },
                        };
                        assert_eq!(
                            self.served(session, view, Some(matches)),
                            with_memo,
                            "memo matching w{n}, {view} for {labels:?}, after {after}"
                        );
                    }
                }
                if model.doi_unique {
                    let with_doi: BTreeSet<u64> = visible
                        .iter()
                        .filter(|t| model.items[t].doi.is_some())
                        .copied()
                        .collect();
                    let doi_in = FilterExpr::Leaf {
                        column: "doi".to_string(),
                        operand: FilterOperand::TextIn(dois.clone()),
                    };
                    assert_eq!(
                        self.served(session, view, Some(doi_in)),
                        with_doi,
                        "doi in, {view} for {labels:?}, after {after}"
                    );
                }
            }
        }
        self.check_cards(&principals, after);
        self.check_content(after);
        self.check_entities(after);
    }

    /// Every live item's `tessera_id` names an entity, and no two name the same one: an id a fold
    /// freed serves one item at a time. A deleted item's names nothing.
    fn check_entities(&self, after: &str) {
        let model = &self.model;
        let tids: Vec<u64> = model.items.keys().copied().collect();
        let ids: Vec<TesseraId> = tids.iter().map(|t| TesseraId::new(*t)).collect();
        let entities = self.engine().resolve_tessera_ids(&ids).unwrap();
        let mut held: BTreeMap<u64, u64> = BTreeMap::new();
        for (tid, entity) in tids.iter().zip(entities) {
            let entity = entity
                .unwrap_or_else(|| panic!("item {tid} names no entity, after {after}"))
                .raw();
            if let Some(other) = held.insert(entity, *tid) {
                panic!("items {other} and {tid} name entity {entity}, after {after}");
            }
        }
        let deleted: Vec<TesseraId> = model.deleted.iter().map(|t| TesseraId::new(*t)).collect();
        for (tid, entity) in deleted
            .iter()
            .zip(self.engine().resolve_tessera_ids(&deleted).unwrap())
        {
            assert!(
                entity.is_none(),
                "deleted item {} names entity {entity:?}, after {after}",
                tid.raw()
            );
        }
    }

    /// The artifact serves its content to a principal who can see every item it was generated
    /// from, wherever those items' entities have moved, until a delete takes one of them.
    fn check_content(&self, after: &str) {
        let model = &self.model;
        let every_one_seen = model.content_from.iter().all(|tid| {
            model.items.get(tid).is_some_and(|item| {
                model.flushed.contains(&(*tid, VIEWS[0].to_string()))
                    && model.visible_to(item, &["0"])
            })
        });
        if model.content_lost || !every_one_seen {
            return;
        }
        let served = artifacts_of(self.engine(), &full_coverage_credential())
            .into_iter()
            .find(|a| a.key.as_deref() == Some("t0"))
            .map(|a| a.content.first().cloned().unwrap_or_default());
        assert_eq!(
            served.as_deref(),
            Some(CONTENT),
            "the content of an artifact generated from {:?}, after {after}",
            model.content_from
        );
    }

    /// Every item's card, for each principal. An item is shown only to a principal whose labels
    /// reach it, and only in the views a flush has placed it in: an edited item has no card from
    /// its acknowledgement until its flush, and the card then shows only what the edit left. A
    /// deleted item's `tessera_id` answers nothing.
    fn check_cards(&self, principals: &[(Session, Vec<&str>)], after: &str) {
        let engine = self.engine();
        let model = &self.model;
        for (session, labels) in principals {
            for item in model.items.values() {
                let placed: BTreeMap<&String, &(f64, f64)> = item
                    .views
                    .iter()
                    .filter(|(view, _)| model.flushed.contains(&(item.tid, view.to_string())))
                    .collect();
                let card = engine
                    .item(session, TesseraId::new(item.tid))
                    .unwrap_or_else(|e| panic!("item {}'s card, after {after}: {e}", item.tid));
                if placed.is_empty() || !model.visible_to(item, labels) {
                    assert!(
                        card.is_none(),
                        "item {} has a card for {labels:?}, placed in {placed:?}, after {after}",
                        item.tid
                    );
                    continue;
                }
                let card = card.unwrap_or_else(|| {
                    panic!(
                        "item {} has no card for {labels:?}, after {after}",
                        item.tid
                    )
                });
                let field = |name: &str| {
                    card.fields
                        .iter()
                        .find(|f| f.name == name)
                        .map(|f| f.value.clone())
                };
                assert_eq!(
                    field("gid"),
                    item.gid.map(ScalarOut::U64),
                    "gid of {}, after {after}",
                    item.tid
                );
                assert_eq!(
                    field("doi"),
                    item.doi.clone().map(ScalarOut::Utf8),
                    "doi of {}, after {after}",
                    item.tid
                );
                assert_eq!(
                    field("score"),
                    item.score.map(ScalarOut::F64),
                    "score of {}, after {after}",
                    item.tid
                );
                for (at, name) in EXTRAS.iter().enumerate() {
                    let value = field(name);
                    assert!(
                        shows(&value, &item.extras[at]),
                        "{name} of {} is {value:?}, expected {:?}, after {after}",
                        item.tid,
                        item.extras[at]
                    );
                }
                // A card serves no scoped text family; its words are checked by `match`.
                let scoped: BTreeMap<&str, &[(String, ScalarOut)]> = card
                    .scoped
                    .iter()
                    .map(|f| (f.name.as_str(), f.values.as_slice()))
                    .collect();
                let heat = match scoped.get(SCOPED[0]).copied() {
                    None => None,
                    Some([(key, value)]) if key == GROUP.1 => Some(value.clone()),
                    Some(other) => panic!("heat of {} under {other:?}, after {after}", item.tid),
                };
                assert!(
                    shows(&heat, &item.scoped[0]) && scoped.len() <= 1,
                    "heat of {} is {heat:?}, expected {:?}; families {:?}, after {after}",
                    item.tid,
                    item.scoped[0],
                    scoped.keys()
                );
                let views: BTreeMap<String, (u32, u32)> = card
                    .views
                    .iter()
                    .map(|v| (v.id.clone(), (v.x, v.y)))
                    .collect();
                let expected: BTreeMap<String, (u32, u32)> = placed
                    .iter()
                    .map(|(view, (x, y))| {
                        (
                            view.to_string(),
                            (
                                tessera_spatial::fixed32(*x, 0.0, 1000.0),
                                tessera_spatial::fixed32(*y, 0.0, 1000.0),
                            ),
                        )
                    })
                    .collect();
                assert_eq!(views, expected, "positions of {}, after {after}", item.tid);
            }
            for tid in &model.deleted {
                assert!(
                    engine
                        .item(session, TesseraId::new(*tid))
                        .unwrap()
                        .is_none(),
                    "deleted item {tid} has no card, after {after}"
                );
            }
        }
    }

    fn step(&mut self, op: &Op) {
        if std::env::var_os("MODEL_TRACE").is_some() {
            eprintln!("TRACE op {op:?}");
        }
        match op {
            Op::Ingest { view, rows } => self.ingest(*view, rows),
            Op::Changes(targets) => self.changes(targets),
            Op::Flush => self.flush(),
            Op::Fold => {
                let engine = self.engine();
                let before = engine.write_executor_stats();
                engine.request_fold();
                wait_until("the fold publishes", Duration::from_secs(60), || {
                    let now = engine.write_executor_stats();
                    assert_eq!(
                        (now.fold_failures, now.fold_refusals),
                        (before.fold_failures, before.fold_refusals),
                        "the fold was refused or discarded: {:?}",
                        now.last_fold_refusal
                    );
                    now.folds > before.folds
                });
                // The tick a fold request brings flushes whatever the buffer held, beside the
                // fold or during its flight.
                self.flush();
            }
            Op::PinnedFold { view, rows } => {
                // Everything buffered is placed first, so the fold's tick places nothing.
                self.flush();
                let engine = self.engine();
                let before = engine.write_executor_stats();
                engine.set_fold_paused_for_test(true);
                engine.request_fold();
                wait_until("the fold holds", Duration::from_secs(60), || {
                    engine.fold_is_holding_for_test()
                });
                self.ingest(*view, rows);
                let engine = self.engine();
                engine.set_fold_paused_for_test(false);
                wait_until("the fold publishes", Duration::from_secs(60), || {
                    let now = engine.write_executor_stats();
                    assert_eq!(
                        (now.fold_failures, now.fold_refusals),
                        (before.fold_failures, before.fold_refusals),
                        "the fold was refused or discarded: {:?}",
                        now.last_fold_refusal
                    );
                    now.folds > before.folds
                });
            }
            Op::Restart => self.restart(),
            Op::Resend(which) => self.resend(*which),
            Op::DoiUnique(unique) => self.doi_unique(*unique),
            Op::DropView => self.drop_view(),
        }
        self.check(&format!("{op:?}"));
    }
}

/// The label layer the artifact is published into.
const LAYER: &str = "topics/a";

/// The artifact's content, served only to a principal who can see every item it was generated
/// from.
const CONTENT: &str = "a label";

/// The built items the content is generated from.
const CONTENT_SOURCES: [u64; 2] = [3, 6];

fn label_layer() -> tessera_types::layer::LayerDeclaration {
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
        MembershipSource, SuppliedContent, SuppliedRequirement,
    };
    LayerDeclaration {
        scope: Default::default(),
        name: LAYER.into(),
        title: None,
        views: vec![VIEWS[0].into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "label".into(),
                ty: "text".into(),
                require_member_visibility: SuppliedRequirement::All,
            }],
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// Register the layer and publish its artifact over every built item, its content generated
/// from [`CONTENT_SOURCES`], answering the content's items' `tessera_id`s.
fn publish_content(engine: &Engine, root: &Path) -> BTreeSet<u64> {
    use tessera_lifecycle::membership::IncomingContent;
    use tessera_lifecycle::IncomingArtifact;
    engine
        .register_layer(label_layer())
        .expect("the layer registers");
    let map = source_to_new_map(root, "v00000");
    let entity = |source: u64| EntityId::new(map[&source]);
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                (0..BUILT).map(entity).collect::<Vec<_>>(),
                vec![IncomingContent::new(
                    vec![CONTENT.to_string()],
                    CONTENT_SOURCES
                        .iter()
                        .map(|s| entity(*s))
                        .collect::<Vec<_>>(),
                )],
            )],
        )
        .expect("the artifact publishes");
    tick(engine);
    CONTENT_SOURCES
        .iter()
        .map(|s| engine.tessera_id_of(entity(*s)).unwrap().raw())
        .collect()
}

fn run(ops: &[Op]) -> Run {
    if std::env::var_os("MODEL_TRACE").is_some() {
        eprintln!("TRACE case");
    }
    let fx = fixture();
    let engine = open(&fx);
    engine.set_merge_for_test(false);
    let mut model = Model::built(&engine, &fx.root);
    model.content_from = publish_content(&engine, &fx.root);
    let highest = run_highest(&model);
    let mut run = Run {
        fx,
        engine: Some(engine),
        model,
        sent: Vec::new(),
        batches: 0,
        highest,
        reused: 0,
        held_suppressed: BTreeSet::new(),
        reused_suppressed: 0,
    };
    run.check("the build");
    for op in ops {
        run.step(op);
    }
    // Everything acknowledged survives a restart and a final flush.
    run.restart();
    run.flush();
    run.check("the last restart and flush");
    run
}

fn run_highest(model: &Model) -> u64 {
    model.items.values().map(|i| i.made + 1).max().unwrap_or(0)
}

proptest! {
    #![proptest_config({
        let config = ProptestConfig::default();
        ProptestConfig {
            cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(6),
            // The same sequences on every run unless `PROPTEST_RNG_SEED` names others.
            rng_seed: match config.rng_seed {
                proptest::test_runner::RngSeed::Random => proptest::test_runner::RngSeed::Fixed(7),
                seed => seed,
            },
            ..config
        }
    })]

    #[test]
    fn writes_agree_with_the_model(ops in prop::collection::vec(op(), 1..200)) {
        run(&ops);
    }
}

proptest! {
    #![proptest_config({
        let config = ProptestConfig::default();
        ProptestConfig {
            cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(2),
            rng_seed: match config.rng_seed {
                proptest::test_runner::RngSeed::Random => proptest::test_runner::RngSeed::Fixed(11),
                seed => seed,
            },
            ..config
        }
    })]

    /// Long sequences that edit the same items again and again, with flushes, folds and restarts
    /// between: a fold frees the entities the edits left, and later edits take them again.
    #[test]
    fn edit_heavy_sequences_reuse_ids_and_agree_with_the_model(
        ops in prop::collection::vec(edit_heavy_op(), 100..300),
    ) {
        let run = run(&ops);
        prop_assert!(run.reused > 0, "no edit took an id a fold freed");
    }
}

/// An edit of an existing item in view 0 or 1, by `tessera_id`, with a new score or a new
/// position.
fn edit_row() -> impl Strategy<Value = RowGen> {
    (any::<u16>(), any::<u16>(), prop_oneof![Just(0u8), Just(2u8)]).prop_map(
        |(about, seed, position)| RowGen {
            about: Some(about),
            by: 0,
            carry: 4,
            differ: 4,
            null: 0,
            position,
            seed,
            extra_carry: 0,
            extra_differ: 0,
            extra_null: 0,
        },
    )
}

fn edit_heavy_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        40 => (0u8..2, prop::collection::vec(edit_row(), 1..6))
            .prop_map(|(view, rows)| Op::Ingest { view, rows }),
        6 => (0u8..2, prop::collection::vec(row_gen(), 1..3))
            .prop_map(|(view, rows)| Op::Ingest { view, rows }),
        3 => prop::collection::vec((any::<u16>(), 0u8..3), 1..3).prop_map(Op::Changes),
        14 => Just(Op::Flush),
        8 => Just(Op::Fold),
        4 => (0u8..2, prop::collection::vec(edit_row(), 1..3))
            .prop_map(|(view, rows)| Op::PinnedFold { view, rows }),
        5 => Just(Op::Restart),
    ]
}

/// **The id space stops growing under repeated edits.** Every item is edited in every round, with
/// a flush and a fold between rounds and restarts among them. An item's first edit takes a new
/// id, and each later one takes an id an edit before left, which the fold between freed: after the
/// second round no round takes an id from the high-water. Two items are suppressed for the first
/// four rounds, so the ids their edits leave are freed too; lifted, they take those ids again and
/// are served on them.
#[test]
fn repeated_edits_and_folds_stop_the_id_space_growing() {
    let edit_everything = |seed: u16, position: u8| Op::Ingest {
        view: 0,
        rows: (0..BUILT as u16)
            .map(|i| RowGen {
                about: Some(i),
                by: 0,
                carry: 4,
                differ: 4,
                null: 0,
                position,
                seed: seed.wrapping_add(i),
                extra_carry: 0,
                extra_differ: 0,
                extra_null: 0,
            })
            .collect(),
    };
    let fx = fixture();
    let engine = open(&fx);
    engine.set_merge_for_test(false);
    let mut model = Model::built(&engine, &fx.root);
    model.content_from = publish_content(&engine, &fx.root);
    let highest = run_highest(&model);
    let mut run = Run {
        fx,
        engine: Some(engine),
        model,
        sent: Vec::new(),
        batches: 0,
        highest,
        reused: 0,
        held_suppressed: BTreeSet::new(),
        reused_suppressed: 0,
    };
    run.step(&Op::Changes(vec![(1, 1), (4, 1)]));
    let mut high_waters = Vec::new();
    for round in 0..8u16 {
        if round == 4 {
            run.step(&Op::Changes(vec![(1, 2), (4, 2)]));
        }
        run.step(&edit_everything(round * 100, (round % 2) as u8 * 2));
        run.step(&Op::Flush);
        if round % 3 == 2 {
            run.step(&Op::Restart);
        }
        run.step(&Op::Fold);
        high_waters.push(run.engine().allocator_high_water());
    }
    run.restart();
    run.flush();
    run.check("the last restart and flush");
    assert_eq!(
        high_waters[2..].iter().collect::<BTreeSet<_>>().len(),
        1,
        "the high-water grew after the second round: {high_waters:?}"
    );
    assert!(
        run.reused >= 6 * BUILT,
        "every edit after the second round takes a freed id; {} did",
        run.reused
    );
    assert!(
        run.reused_suppressed > 0,
        "no item that is not suppressed took an id a suppressed item left"
    );
}

/// A fixed sequence through each kind of row, so the model's rules are met on every run whatever
/// the random cases draw.
#[test]
fn each_kind_of_row_is_decided_as_the_model_decides_it() {
    let about = |i: u16, by: u8, carry: u8, position: u8| RowGen {
        about: Some(i),
        by,
        carry,
        differ: 0,
        null: 0,
        position,
        seed: i,
        extra_carry: if carry == 31 { 31 } else { 0 },
        extra_differ: 0,
        extra_null: 0,
    };
    let fresh = |seed: u16| RowGen {
        about: None,
        by: 0,
        carry: 1 | 2 | 4 | 8,
        differ: 0,
        null: 0,
        position: 2,
        seed,
        extra_carry: 31,
        extra_differ: 0,
        extra_null: 0,
    };
    let ops = vec![
        // Unchanged by each identifier, carrying every field as stored.
        Op::Ingest {
            view: 0,
            rows: vec![about(0, 0, 31, 1), about(1, 1, 31, 1), about(2, 2, 31, 1)],
        },
        // New items, then the same again: unchanged.
        Op::Ingest {
            view: 0,
            rows: vec![fresh(1), fresh(2)],
        },
        Op::Ingest {
            view: 0,
            rows: vec![about(12, 1, 31, 1), about(13, 1, 31, 1)],
        },
        // One of them added to the other view, then an edit refused.
        Op::Ingest {
            view: 1,
            rows: vec![about(12, 0, 0, 2)],
        },
        Op::Ingest {
            view: 0,
            rows: vec![RowGen {
                differ: 4,
                ..about(3, 0, 4, 0)
            }],
        },
        // Through the group's view: a scoped value edited with no position, a new item added
        // carrying its values, and one carried as held.
        Op::Ingest {
            view: 3,
            rows: vec![RowGen {
                extra_carry: 32 | 64,
                extra_differ: 32,
                ..about(6, 0, 0, 0)
            }],
        },
        Op::Ingest {
            view: 3,
            rows: vec![
                RowGen {
                    extra_carry: 32 | 64,
                    ..about(12, 0, 0, 2)
                },
                RowGen {
                    extra_carry: 32 | 64,
                    ..about(7, 0, 0, 1)
                },
            ],
        },
        // A tessera_id nobody holds, a row naming two items, two rows naming one.
        Op::Ingest {
            view: 0,
            rows: vec![RowGen { by: 4, ..fresh(3) }],
        },
        Op::Ingest {
            view: 0,
            rows: vec![about(4, 0, 0, 0), about(4, 1, 0, 0)],
        },
        Op::Flush,
        Op::Changes(vec![(0, 1), (1, 0)]),
        Op::Restart,
        Op::Resend(1),
        Op::DoiUnique(false),
        Op::Ingest {
            view: 2,
            rows: vec![about(5, 1, 31, 0)],
        },
        Op::DoiUnique(true),
        Op::Fold,
        // Two items in the group's view alone, one flushed and one buffered, which the drop
        // deletes; then the view is named again.
        Op::Ingest {
            view: 3,
            rows: vec![fresh(5)],
        },
        Op::Flush,
        Op::Ingest {
            view: 3,
            rows: vec![fresh(6)],
        },
        Op::DropView,
        Op::Ingest {
            view: 3,
            rows: vec![fresh(4)],
        },
        Op::DropView,
        Op::Fold,
    ];
    run(&ops);
}

/// **An item older than a view's newest rows joins it in place.** The item keeps its entity and its
/// row in the view it was in, and is served there throughout. The flush lists its new row beside a
/// segment whose range of entities spans it with no row, and it is served there from then on,
/// through a restart, a merge that takes that segment, a fold and a restart after the fold.
#[test]
fn an_item_older_than_a_views_newest_rows_joins_it_in_place() {
    let fx = fixture();
    let mut engine = open(&fx);
    engine.set_merge_for_test(false);
    let row = |external: &[u8], position: (f64, f64)| IngestRow {
        tessera_id: None,
        external_id: Some(external.to_vec()),
        labels: Some(vec![b"0".to_vec()]),
        position: Some(position),
        scalars: vec![WalScalar::Null; 3],
        scoped: Vec::new(),
        omitted: vec![0, 1, 2],
    };
    let send = |engine: &Engine, batch: &str, view: &str, rows: Vec<IngestRow>| {
        engine
            .ingest(IngestRequest {
                batch_id: batch.to_string(),
                body_hash: hash_of(batch),
                view: Some(view.to_string()),
                rows,
                artifacts: Default::default(),
            })
            .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"))
    };
    // `older` is given an entity between two items of the other view, so that view's segment spans
    // it with no row.
    send(&engine, "before", VIEWS[1], vec![row(b"before", (20.0, 20.0))]);
    let older = send(&engine, "older", VIEWS[0], vec![row(b"older", (10.0, 10.0))]).tessera_ids[0];
    send(&engine, "after", VIEWS[1], vec![row(b"after", (25.0, 25.0))]);
    publish_buffered(&engine);
    let entity = |engine: &Engine| engine.resolve_tessera_ids(&[older]).unwrap()[0];
    let first = entity(&engine);
    // Where the item is served, and at what position in each view.
    let placed = |engine: &Engine| -> BTreeMap<String, (u32, u32)> {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        let card: BTreeMap<String, (u32, u32)> = engine
            .item(&session, older)
            .unwrap()
            .map(|card| card.views.iter().map(|v| (v.id.clone(), (v.x, v.y))).collect())
            .unwrap_or_default();
        for view in VIEWS {
            let out = engine
                .viewport(&session, ViewportRequest::new(view, 0, VIEWPORT, 10_000))
                .unwrap();
            assert_eq!(
                out.points.iter().any(|(id, _)| id == older),
                card.contains_key(view),
                "{view}'s points and the item's card agree"
            );
        }
        card
    };
    let at = |x: f64, y: f64| {
        (
            tessera_spatial::fixed32(x, 0.0, 1000.0),
            tessera_spatial::fixed32(y, 0.0, 1000.0),
        )
    };

    let added = send(&engine, "add-older", VIEWS[1], vec![row(b"older", (30.0, 30.0))]);
    assert_eq!((added.added, added.edited), (1, 0));
    assert_eq!(added.tessera_ids, vec![older], "the item keeps its tessera_id");
    assert_eq!(entity(&engine), first, "and its entity");
    assert_eq!(
        placed(&engine),
        BTreeMap::from([(VIEWS[0].to_string(), at(10.0, 10.0))]),
        "the item stays served in the view it was in while the join is buffered"
    );
    let both = BTreeMap::from([
        (VIEWS[0].to_string(), at(10.0, 10.0)),
        (VIEWS[1].to_string(), at(30.0, 30.0)),
    ]);
    publish_buffered(&engine);
    assert_eq!(placed(&engine), both, "the flush places it in the view it joined");
    drop(engine);
    engine = open(&fx);
    assert_eq!(placed(&engine), both, "after a restart");

    engine.set_merge_for_test(true);
    let merges = engine.write_executor_stats().merges;
    for i in 0..4 {
        let key = format!("more{i}");
        send(&engine, &key, VIEWS[1], vec![row(key.as_bytes(), (40.0 + f64::from(i), 40.0))]);
        publish_buffered(&engine);
    }
    tick_until(&engine, "a merge", Duration::from_secs(60), || {
        engine.write_executor_stats().merges > merges
    });
    assert_eq!(placed(&engine), both, "after a merge");
    tessera_build::verify_deep(&fx.root, &tessera_build::VerifyOpts::default())
        .expect("the merged bundle verifies");

    fold(&engine);
    assert_eq!(placed(&engine), both, "after a fold");
    drop(engine);
    let engine = open(&fx);
    assert_eq!(placed(&engine), both, "after a restart past the fold");
    assert_eq!(entity(&engine), first, "the item's entity never changed");
}
