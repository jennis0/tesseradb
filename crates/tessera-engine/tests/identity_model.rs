//! **Random sequences of writes, checked against a model after every step.** Each case builds a
//! small bundle with two views, then runs a random sequence of ingest batches, change batches,
//! flushes, folds, restarts, resent batches and declarations of `unique` on and off one column.
//! After every step it compares what the service serves with what a model of the items says it
//! should: every view's points for two principals, `in` over every unique value, and every item's
//! card.
//!
//! An ingest row names an item by its `tessera_id`, its external id or a unique value, and carries
//! any of the item's fields, its label and a position in the batch's view. The model decides each
//! row the way the service must: a row naming nothing creates an item, one naming an item it
//! leaves unchanged is counted unchanged, one adding the item to a view it is not in is added, and
//! one that would change the item is refused, since editing is not available yet. A batch naming
//! two items in one row, one item in two rows, one value in two rows or a `tessera_id` nobody holds
//! is refused whole.
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
use tessera_types::layer::LayerScope;
use tessera_types::{EntityId, TesseraId};

const VIEWS: [&str; 2] = ["s0", "s1"];
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
    write_pairs_n(&pairs, BUILT);
    let schema_path = tmp.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the fixture schema parses")
        .schema;
    tessera_build::build(&tessera_build::BuildArgs {
        views: VIEWS
            .iter()
            .map(|view| tessera_build::ViewArgs {
                visibility: None,
                view_id: view.to_string(),
                projection: tessera_spatial::Projection::None,
                extent: extent(),
                points: points.clone(),
                point_fields: Default::default(),
                select: None,
                access: tessera_build::config::AccessInput::relation(pairs.clone()),
            })
            .collect(),
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
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
    /// Each view the item has a row in, with its position there.
    views: BTreeMap<String, (f64, f64)>,
    suppressed: bool,
    /// Its place in creation order, which is the order of the engine's entity ids.
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
    /// Items made so far, built ones included.
    made: u64,
    /// Per view, one past the newest item a flush has given a row there. An item older than that
    /// is added to the view by moving it, which is refused as an edit.
    floors: BTreeMap<String, u64>,
    /// Each view's `point_visibility.default`, the label of an item created without one.
    defaults: BTreeMap<String, Option<String>>,
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
    labels: Option<BTreeSet<String>>,
    position: Option<(f64, f64)>,
}

/// What the model expects of one row of an accepted batch.
#[derive(Debug, Clone, PartialEq)]
enum Expect {
    Create,
    Unchanged(u64),
    Added(u64),
}

impl Model {
    fn built(engine: &Engine, root: &Path) -> Model {
        let mut model = Model {
            doi_unique: true,
            defaults: engine
                .meta()
                .views
                .iter()
                .map(|v| (v.id.clone(), v.point_default.clone()))
                .collect(),
            made: BUILT,
            floors: VIEWS.iter().map(|v| (v.to_string(), BUILT)).collect(),
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
                    views: VIEWS.iter().map(|v| (v.to_string(), position)).collect(),
                    suppressed: false,
                    made: entity,
                },
            );
            for view in VIEWS {
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
            let mut joins = false;
            if let (Some(view), Some(position)) = (view, row.position) {
                match item.views.get(view) {
                    Some(held) if *held == position => {}
                    Some(_) => return None,
                    None if item.made < self.floors[view] => return None,
                    None => joins = true,
                }
            }
            if differs {
                return None;
            }
            expect.push(if joins {
                Expect::Added(*tid)
            } else {
                Expect::Unchanged(*tid)
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
                    assert_eq!(tid, named);
                    let view = view.expect("a row adding an item names a view").to_string();
                    self.items
                        .get_mut(named)
                        .unwrap()
                        .views
                        .insert(view, row.position.unwrap());
                }
            }
        }
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
    /// Bits of [`EXTRAS`] the row carries, those given another value, and those sent as null.
    extra_carry: u8,
    extra_differ: u8,
    extra_null: u8,
    /// 0 none, 1 the item's own position in the view where it has one, 2 a new position.
    position: u8,
    seed: u16,
}

#[derive(Debug, Clone)]
enum Op {
    /// A batch into view 0 or 1, or naming no view.
    Ingest {
        view: u8,
        rows: Vec<RowGen>,
    },
    /// `(item, op)`: 0 delete, 1 suppress, 2 unsuppress.
    Changes(Vec<(u16, u8)>),
    Flush,
    Fold,
    Restart,
    /// Send an earlier batch again, under its own batch id and body.
    Resend(u16),
    /// Declare `unique` on `doi`, or take it off.
    DoiUnique(bool),
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
        12 => (0u8..3, prop::collection::vec(row_gen(), 1..5))
            .prop_map(|(view, rows)| Op::Ingest { view, rows }),
        3 => prop::collection::vec((any::<u16>(), 0u8..3), 1..4).prop_map(Op::Changes),
        3 => Just(Op::Flush),
        1 => Just(Op::Fold),
        2 => Just(Op::Restart),
        2 => any::<u16>().prop_map(Op::Resend),
        1 => any::<bool>().prop_map(Op::DoiUnique),
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
            scoped: Vec::new(),
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
        let view = VIEWS.get(view as usize).copied();
        let (rows, wire): (Vec<Row>, Vec<IngestRow>) =
            gens.iter().map(|g| self.build_row(view, g)).unzip();
        self.batches += 1;
        let batch_id = format!("b{}", self.batches);
        let expected = self.model.decide(view, &rows);
        let answered = self.send(&batch_id, view, wire.clone());
        match (expected, answered) {
            (None, Err(e)) => assert!(
                matches!(e, AcceptError::Conflict(_) | AcceptError::Contract(_)),
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
                self.check_receipt(&expect, &receipt, &batch_id);
                let ids: Vec<u64> = receipt.tessera_ids.iter().map(|t| t.raw()).collect();
                self.model.apply(view, &rows, &expect, &ids);
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
        assert_eq!(receipt.edited, 0, "{batch_id}");
        for (expect, tid) in expect.iter().zip(&receipt.tessera_ids) {
            if let Expect::Unchanged(named) | Expect::Added(named) = expect {
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
                // The batch id now answers this acceptance.
                self.sent[at].tessera_ids = got;
            }
            Err(e) => assert!(
                expected.is_none()
                    && matches!(e, AcceptError::Conflict(_) | AcceptError::Contract(_)),
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
            let entity = self.engine().resolve_tessera_ids(&[TesseraId::new(tid)])[0]
                .expect("a live item's tessera_id names it");
            changes.push((entity, op));
            match op {
                ChangeOp::Delete => {
                    if self.model.items.remove(&tid).is_some() {
                        self.model.deleted.insert(tid);
                    }
                }
                ChangeOp::Suppress | ChangeOp::Unsuppress => {
                    if let Some(item) = self.model.items.get_mut(&tid) {
                        item.suppressed = op == ChangeOp::Suppress;
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
        publish(self.engine());
        for item in self.model.items.values() {
            for view in item.views.keys() {
                self.model.flushed.insert((item.tid, view.clone()));
                let floor = self.model.floors.get_mut(view).expect("a declared view");
                *floor = (*floor).max(item.made + 1);
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

    // ---- what is served ----------------------------------------------------------------------

    fn served(&self, session: &Session, view: &str, filter: Option<FilterExpr>) -> BTreeSet<u64> {
        let mut req = ViewportRequest::new(view, 0, VIEWPORT, 10_000);
        req.filter = filter;
        self.engine()
            .viewport(session, req)
            .unwrap()
            .points
            .iter()
            .map(|(id, _)| id.raw())
            .collect()
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
            for view in VIEWS {
                let visible: BTreeSet<u64> = model
                    .items
                    .values()
                    .filter(|i| i.views.contains_key(view))
                    .filter(|i| model.flushed.contains(&(i.tid, view.to_string())))
                    .filter(|i| model.visible_to(i, labels))
                    .map(|i| i.tid)
                    .collect();
                assert_eq!(
                    self.served(session, view, None),
                    visible,
                    "{view} for {labels:?}, after {after}"
                );
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
        self.check_cards(&principals[0].0, after);
    }

    /// Every item's card, for items whose every row a flush has placed: its fields and its
    /// positions. A deleted item's `tessera_id` answers nothing.
    fn check_cards(&self, session: &Session, after: &str) {
        let engine = self.engine();
        let model = &self.model;
        for item in model.items.values() {
            let placed = item
                .views
                .keys()
                .all(|v| model.flushed.contains(&(item.tid, v.clone())));
            if !placed {
                continue;
            }
            let card = engine
                .item(session, TesseraId::new(item.tid))
                .unwrap_or_else(|e| panic!("item {}'s card, after {after}: {e}", item.tid));
            if !model.visible_to(item, &["0"]) {
                assert!(
                    card.is_none(),
                    "item {} is not visible, after {after}",
                    item.tid
                );
                continue;
            }
            let card =
                card.unwrap_or_else(|| panic!("item {} has a card, after {after}", item.tid));
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
            let views: BTreeMap<String, (u32, u32)> = card
                .views
                .iter()
                .map(|v| (v.id.clone(), (v.x, v.y)))
                .collect();
            let expected: BTreeMap<String, (u32, u32)> = item
                .views
                .iter()
                .map(|(view, (x, y))| {
                    (
                        view.clone(),
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

    fn step(&mut self, op: &Op) {
        match op {
            Op::Ingest { view, rows } => self.ingest(*view, rows),
            Op::Changes(targets) => self.changes(targets),
            Op::Flush => self.flush(),
            Op::Fold => {
                self.flush();
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
            }
            Op::Restart => self.restart(),
            Op::Resend(which) => self.resend(*which),
            Op::DoiUnique(unique) => self.doi_unique(*unique),
        }
        self.check(&format!("{op:?}"));
    }
}

fn run(ops: &[Op]) {
    let fx = fixture();
    let engine = open(&fx);
    engine.set_merge_for_test(false);
    let model = Model::built(&engine, &fx.root);
    let mut run = Run {
        fx,
        engine: Some(engine),
        model,
        sent: Vec::new(),
        batches: 0,
    };
    run.check("the build");
    for op in ops {
        run.step(op);
    }
    // Everything acknowledged survives a restart and a final flush.
    run.restart();
    run.flush();
    run.check("the last restart and flush");
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
    ];
    run(&ops);
}

/// **An item is added to a view in place only when it is newer than the view's newest rows.** A
/// flush places rows above those, so an older item would be moved, which is an edit and refused
/// with nothing written; a newer one is added and flushes.
#[test]
fn an_item_older_than_a_views_newest_rows_is_not_added_in_place() {
    let fx = fixture();
    let engine = open(&fx);
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
    let send = |batch: &str, view: &str, rows: Vec<IngestRow>| {
        engine.ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash: hash_of(batch),
            view: Some(view.to_string()),
            rows,
            artifacts: Default::default(),
        })
    };
    send("older", VIEWS[0], vec![row(b"older", (10.0, 10.0))]).expect("a new item is created");
    send("newer", VIEWS[1], vec![row(b"newer", (20.0, 20.0))]).expect("a new item is created");
    publish(&engine);

    let refused = send("add-older", VIEWS[1], vec![row(b"older", (30.0, 30.0))]);
    assert!(
        matches!(refused, Err(AcceptError::Conflict(_))),
        "an item older than the view's newest rows is not added in place: {refused:?}"
    );
    assert_eq!(engine.buffered_items(), 0, "a refused batch writes nothing");

    send("newest", VIEWS[0], vec![row(b"newest", (40.0, 40.0))]).expect("a new item is created");
    publish(&engine);
    let added = send("add-newest", VIEWS[1], vec![row(b"newest", (50.0, 50.0))])
        .expect("an item newer than the view's newest rows is added");
    assert_eq!(added.added, 1);
    publish(&engine);
}
