//! `Engine::aggregate_stream` grouping by bins: a histogram of a number or timestamp field.
//!
//! The oracle is the fixture's own source data: each item's values are computed here from its
//! source id, and every expected count is taken by placing those values in the bins the response
//! names, never read back from the engine. Edges are checked for what they promise: they cover
//! every visible value, stay put under a filter or a region, and fall on readable values. That
//! invisible items move neither an edge nor a count is checked against a second bundle built from
//! the visible items alone.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{
    Array, BooleanArray, DictionaryArray, Float32Array, Float64Array, Int32Array, Int64Array,
    StringArray, TimestampMicrosecondArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Int8Type, Schema as ArrowSchema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use mosaica_build::config::Config;
use mosaica_build::{build, BuildArgs};
use mosaica_engine::filter::{FilterExpr, FilterOperand, RegionLeaf, Scalar};
use mosaica_engine::IngestRequest;
use mosaica_engine::{
    AggregateCaps, AggregateHead, AggregateRefused, AggregateRequest, AggregateSink,
    AggregateTrailer, By, Engine, EngineError, Grouping, PageEnd, RecordsLimits, Reference,
    Session, SinkResult, TableHead, ViewportRequest,
};
use mosaica_lifecycle::command::IngestRow;
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_lifecycle::UnallocatedRow;
use mosaica_spatial::shape::{ShapeF64, Space};
use mosaica_types::{ItemHigh, Tenancy};

const N: u64 = 3_000;

/// The first source id of an item a test ingests.
const INGESTED: u64 = 1_000_000;

/// `score` is drawn and indexed, `weight` and `seen` indexed alone, `rank` and `when` drawn alone.
/// `flag` is an indexed bool, which has no bins.
const SCHEMA: &str = r#"
[[vocabulary]]
name       = "kind"
width      = "u8"
value_set  = "open"
visibility = "public"

[[attribute]]
name       = "kind"
type       = "category"
render     = true
index      = true
vocabulary = "kind"

[[attribute]]
name   = "score"
type   = "f64"
render = true
index  = true

[[attribute]]
name  = "weight"
type  = "f32"
index = true

[[attribute]]
name   = "rank"
type   = "i32"
render = true

[[attribute]]
name  = "seen"
type  = "timestamp_us"
index = true

[[attribute]]
name   = "when"
type   = "timestamp_us"
render = true

[[attribute]]
name  = "flag"
type  = "bool"
index = true

[[attribute]]
name   = "id"
type   = "u64"
field  = "entity_id"
unique = true
"#;

const DAY: i64 = 86_400_000_000;
const HOUR: i64 = 3_600_000_000;

/// Microseconds at the start of a date, from the calendar as the oracle reckons it.
fn date(year: i64, month: u32, day: u32) -> i64 {
    // Days from 1970-01-01, counting whole years then months.
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut days = 0i64;
    if year >= 1970 {
        for y in 1970..year {
            days += if leap(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if leap(y) { 366 } else { 365 };
        }
    }
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    days += lengths[..(month - 1) as usize].iter().sum::<i64>();
    (days + i64::from(day) - 1) * DAY
}

/// Whether `t` is midnight on the first of a month.
fn month_start(t: i64) -> bool {
    (1900..2300).any(|y| (1..=12).any(|m| date(y, m, 1) == t))
}

/// One item as the oracle knows it.
#[derive(Debug, Clone)]
struct Item {
    source: u64,
    kind: &'static str,
    score: Option<f64>,
    weight: Option<f32>,
    rank: Option<i32>,
    seen: Option<i64>,
    when: Option<i64>,
    subset: bool,
    position: (f64, f64),
    /// Holds a row in the view.
    flushed: bool,
}

/// A value as the oracle compares it with an edge.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Val {
    F(f64),
    I(i128),
    T(i64),
}

impl Item {
    fn value(&self, column: &str) -> Option<Val> {
        match column {
            "score" => self.score.map(Val::F),
            "weight" => self.weight.map(|w| Val::F(f64::from(w))),
            "rank" => self.rank.map(|r| Val::I(i128::from(r))),
            "seen" => self.seen.map(Val::T),
            "when" => self.when.map(Val::T),
            _ => panic!("no column {column}"),
        }
    }
}

/// Every item's values. The smallest and largest value of every field are held by items the
/// subset viewer cannot see.
fn built(s: u64) -> Item {
    // Item `low` holds the smallest value and item `high` the largest.
    fn extreme<T>(s: u64, low: u64, high: u64, lo: T, hi: T) -> Option<T> {
        match s {
            s if s == low => Some(lo),
            s if s == high => Some(hi),
            _ => None,
        }
    }
    let score = extreme(s, 2, 1, -300.0, 500.0).or_else(|| {
        (!s.is_multiple_of(17)).then(|| match s % 101 {
            50 => f64::NAN,
            _ => ((s * 7919) % 1000) as f64 / 10.0 - 20.0,
        })
    });
    let weight = extreme(s, 5, 4, -1e6, 1e6)
        .or_else(|| (!s.is_multiple_of(13)).then(|| ((s * 31) % 500) as f32 * 0.25));
    let rank = extreme(s, 10, 8, -100_000, 100_000)
        .or_else(|| (!s.is_multiple_of(7)).then(|| ((s * 13) % 400) as i32 - 100));
    let seen = extreme(s, 11, 13, date(1900, 1, 1), date(2200, 1, 1)).or_else(|| {
        (!s.is_multiple_of(19))
            .then(|| date(2016, 1, 1) + ((s * 97) % 1100) as i64 * DAY + (s % 24) as i64 * HOUR)
    });
    let when = extreme(s, 14, 16, date(2000, 1, 1), date(2030, 1, 1)).or_else(|| {
        (!s.is_multiple_of(23)).then(|| date(2024, 3, 1) + ((s * 61) % 200) as i64 * HOUR)
    });
    Item {
        source: s,
        kind: ["a", "b", "c"][(s % 5 % 3) as usize],
        score,
        weight,
        rank,
        seen,
        when,
        subset: subset_sees(s),
        position: (((s * 37) % 1000) as f64, ((s * 53) % 1000) as f64),
        flushed: true,
    }
}

fn write_points(path: &Path, items: &[Item]) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("kind", DataType::Utf8, true),
        Field::new("score", DataType::Float64, true),
        Field::new("weight", DataType::Float32, true),
        Field::new("rank", DataType::Int32, true),
        Field::new(
            "seen",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new(
            "when",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("flag", DataType::Boolean, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(
                items.iter().map(|i| i.source),
            )),
            Arc::new(Float64Array::from_iter_values(
                items.iter().map(|i| i.position.0),
            )),
            Arc::new(Float64Array::from_iter_values(
                items.iter().map(|i| i.position.1),
            )),
            Arc::new(StringArray::from_iter_values(items.iter().map(|i| i.kind))),
            Arc::new(Float64Array::from(
                items.iter().map(|i| i.score).collect::<Vec<_>>(),
            )),
            Arc::new(Float32Array::from(
                items.iter().map(|i| i.weight).collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                items.iter().map(|i| i.rank).collect::<Vec<_>>(),
            )),
            Arc::new(TimestampMicrosecondArray::from(
                items.iter().map(|i| i.seen).collect::<Vec<_>>(),
            )),
            Arc::new(TimestampMicrosecondArray::from(
                items.iter().map(|i| i.when).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from_iter(
                items.iter().map(|i| Some(i.source.is_multiple_of(2))),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Each item's terms, as the shared fixture gives them.
fn write_pairs(path: &Path, items: &[Item]) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let (mut entities, mut terms) = (Vec::new(), Vec::new());
    for item in items {
        for t in terms_of(item.source) {
            entities.push(item.source);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_bundle(dir: &Path, items: &[Item]) -> PathBuf {
    build_views(dir, items, &[("s0", items)])
}

/// A bundle of `items`, each view holding the items listed beside it.
fn build_views(dir: &Path, items: &[Item], views: &[(&str, &[Item])]) -> PathBuf {
    let open: Vec<GatedView<'_>> = views
        .iter()
        .map(|&(view, held)| (view, held, None))
        .collect();
    build_gated_views(dir, items, &open)
}

/// A view's name, the items it holds, and the labels a viewer must hold one of to reach it.
type GatedView<'a> = (&'a str, &'a [Item], Option<Vec<String>>);

/// [`build_views`], each view reached by a viewer holding one of the labels beside it, or by every
/// viewer where there are none.
fn build_gated_views(dir: &Path, items: &[Item], views: &[GatedView<'_>]) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, items);
    // A label is the item's, so every view reads one relation.
    write_pairs(&pairs, items);
    std::fs::write(dir.join("config.toml"), SCHEMA).unwrap();
    let schema = Config::parse(&dir.join("config.toml"), &HashMap::new())
        .expect("the schema parses")
        .schema;
    let views = views
        .iter()
        .map(|(view, held, visibility)| {
            let points = dir.join(format!("{view}-points.parquet"));
            write_points(&points, held);
            mosaica_build::ViewArgs {
                visibility: visibility.clone(),
                view_id: view.to_string(),
                projection: mosaica_spatial::Projection::None,
                extent: extent(),
                points,
                point_fields: Default::default(),
                select: None,
                access: mosaica_build::config::AccessInput::relation(pairs.clone()),
            }
        })
        .collect();
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(points, &schema),
        out: out.clone(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");
    out
}

struct Fx {
    _tmp: tempfile::TempDir,
    engine: Engine,
    items: Vec<Item>,
}

fn fixture_of(items: Vec<Item>) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_bundle(tmp.path(), &items);
    let engine = engine_at(tmp.path(), &root, 3600);
    engine.set_background_refresh_for_test(false);
    Fx {
        _tmp: tmp,
        engine,
        items,
    }
}

fn fixture() -> Fx {
    fixture_of((0..N).map(built).collect())
}

impl Fx {
    fn session(&self, broad: bool) -> Session {
        let credential = if broad {
            full_coverage_credential()
        } else {
            subset_credential()
        };
        self.engine.authorise(&credential).unwrap()
    }

    fn visible<'a>(
        &'a self,
        broad: bool,
        keep: &'a dyn Fn(&Item) -> bool,
    ) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |item| item.flushed && (broad || item.subset) && keep(item))
    }

    fn kind_is(&self, key: &str) -> FilterExpr {
        let session = self.session(true);
        let page = self
            .engine
            .categories(
                &session,
                "kind",
                mosaica_engine::CategoryQuery::Page {
                    after: None,
                    limit: 100,
                },
            )
            .unwrap()
            .expect("a category");
        let code = page.values.iter().find(|v| v.key == key).unwrap().code;
        FilterExpr::Leaf {
            column: "kind".to_string(),
            operand: FilterOperand::In(vec![mosaica_types::AttrLocalId::new(code)]),
        }
    }
}

fn bbox(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> FilterExpr {
    let shape = ShapeF64::Bbox {
        min_x,
        min_y,
        max_x,
        max_y,
    }
    .canonical(Space::View, &extent())
    .expect("a well-formed box")
    .0;
    FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)))
}

fn in_box(item: &Item, b: [f64; 4]) -> bool {
    let (x, y) = item.position;
    x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]
}

// ---- the harness ----------------------------------------------------------------------------

#[derive(Default)]
struct Collect {
    head: Option<AggregateHead>,
    tables: Vec<TableHead>,
    pages: Vec<(u32, RecordBatch, PageEnd)>,
}

impl AggregateSink for Collect {
    fn head(&mut self, head: &AggregateHead) -> SinkResult {
        self.head = Some(head.clone());
        Ok(())
    }

    fn table(&mut self, head: &TableHead) -> SinkResult {
        self.tables.push(*head);
        Ok(())
    }

    fn page(&mut self, grouping: u32, batch: &RecordBatch, end: &PageEnd) -> SinkResult {
        self.pages.push((grouping, batch.clone(), end.clone()));
        Ok(())
    }
}

fn caps() -> AggregateCaps {
    AggregateCaps {
        groupings: 8,
        top: 100,
        named: 100,
        bins: 50,
        cells: u64::MAX,
    }
}

fn request(groupings: &[Grouping]) -> AggregateRequest<'_> {
    AggregateRequest {
        view: "s0",
        filter: None,
        reference: None,
        groupings,
        page_rows: None,
        pages: None,
        cursor: None,
        limits: RecordsLimits {
            max_page_rows: 100_000,
            max_page_bytes: 64 << 20,
            response_bytes: 256 << 20,
            response_time: Duration::from_secs(60),
        },
        caps: caps(),
        cancel: None,
    }
}

fn respond(
    engine: &Engine,
    session: &Session,
    req: AggregateRequest<'_>,
) -> Result<(Collect, AggregateTrailer), EngineError> {
    let mut sink = Collect::default();
    let trailer = engine.aggregate_stream(session, req, &mut sink)?;
    Ok((sink, trailer))
}

fn bins(column: &str, n: u32, range: Option<(Scalar, Scalar)>) -> Grouping {
    Grouping {
        by: Some(By::Bins {
            column: column.to_string(),
            bins: n,
            range,
            sample: None,
        }),
        cells: None,
        area: None,
    }
}

/// One row of a histogram.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    group: String,
    /// The bin's edges, on a listed row.
    edges: Option<(Val, Val)>,
    count: u64,
    reference: Option<u64>,
}

/// The value at row `i` of a column of bin edges or of a summary's smallest or largest value.
fn value_at(batch: &RecordBatch, name: &str, i: usize) -> Option<Val> {
    let column = batch.column_by_name(name).unwrap();
    if column.is_null(i) {
        return None;
    }
    Some(match column.data_type() {
        DataType::Float64 => Val::F(
            column
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(i),
        ),
        DataType::Int64 => Val::I(i128::from(
            column
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(i),
        )),
        DataType::UInt64 => Val::I(i128::from(
            column
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(i),
        )),
        DataType::Timestamp(TimeUnit::Microsecond, Some(tz)) if &**tz == "UTC" => Val::T(
            column
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap()
                .value(i),
        ),
        other => panic!("a value of type {other:?}"),
    })
}

fn rows_of(batch: &RecordBatch) -> Vec<Row> {
    let group = batch
        .column_by_name("group")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<Int8Type>>()
        .unwrap()
        .clone();
    let names = group
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .clone();
    let edge = |name: &str, i: usize| value_at(batch, name, i);
    let u64s = |name: &str| {
        batch
            .column_by_name(name)
            .map(|c| c.as_any().downcast_ref::<UInt64Array>().unwrap().clone())
    };
    let (count, reference) = (u64s("count").unwrap(), u64s("reference_count"));
    assert!(batch.column_by_name("key").is_none() && batch.column_by_name("title").is_none());
    (0..batch.num_rows())
        .map(|i| Row {
            group: names.value(group.keys().value(i) as usize).to_string(),
            edges: edge("lower", i).zip(edge("upper", i)),
            count: count.value(i),
            reference: reference.as_ref().map(|r| r.value(i)),
        })
        .collect()
}

/// One table read whole, following the cursor.
fn table(engine: &Engine, session: &Session, req: AggregateRequest<'_>) -> (TableHead, Vec<Row>) {
    let (head, rows, _) = read_table(engine, session, req);
    (head, rows)
}

/// One table read whole, following the cursor, and how many identity band entries it read.
fn read_table(
    engine: &Engine,
    session: &Session,
    req: AggregateRequest<'_>,
) -> (TableHead, Vec<Row>, u64) {
    let mut head = None;
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut band_entries = 0;
    loop {
        let this = AggregateRequest {
            cursor: cursor.as_deref(),
            ..req.clone()
        };
        let (collect, trailer) = respond(engine, session, this).expect("the read answers");
        if head.is_none() {
            head = collect.tables.first().copied();
        }
        for (_, batch, _) in &collect.pages {
            rows.extend(rows_of(batch));
        }
        band_entries += trailer.timings.band_entries;
        match trailer.next {
            None => return (head.expect("a table"), rows, band_entries),
            Some(next) => cursor = Some(next),
        }
    }
}

/// The bins' edges, ascending, from a table's listed rows.
fn edges_of(rows: &[Row]) -> Vec<Val> {
    let bins: Vec<(Val, Val)> = rows.iter().filter_map(|r| r.edges).collect();
    for pair in bins.windows(2) {
        assert_eq!(pair[0].1, pair[1].0, "the bins are contiguous");
    }
    bins.iter()
        .map(|b| b.0)
        .chain(bins.last().map(|b| b.1))
        .collect()
}

fn same(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::I(a), Val::F(b)) | (Val::F(b), Val::I(a)) => a as f64 == b,
        (a, b) => a == b,
    }
}

fn less(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::F(a), Val::F(b)) => a < b,
        (Val::I(a), Val::I(b)) => a < b,
        (Val::T(a), Val::T(b)) => a < b,
        // An integer against a float edge, where a fractional bound has the bins cut in float.
        (Val::I(a), Val::F(b)) => (a as f64) < b,
        (Val::F(a), Val::I(b)) => a < b as f64,
        _ => panic!("a value and an edge of different kinds"),
    }
}

/// The table the oracle gives for `items` over `edges`: every bin, then the rest and none where
/// either set has one.
fn expected(edges: &[Val], items: &[&Item], reference: Option<&[&Item]>, column: &str) -> Vec<Row> {
    let bins = edges.len().saturating_sub(1);
    let place = |item: &Item| -> usize {
        let Some(v) = item.value(column) else {
            return bins + 1;
        };
        (0..bins)
            .find(|&b| {
                !less(v, edges[b])
                    && (less(v, edges[b + 1]) || (b + 1 == bins && same(v, edges[b + 1])))
            })
            .unwrap_or(bins)
    };
    let tally = |items: &[&Item]| {
        let mut counts = vec![0u64; bins + 2];
        for item in items {
            counts[place(item)] += 1;
        }
        counts
    };
    let set = tally(items);
    let reference = reference.map(tally);
    let mut out = Vec::new();
    for g in 0..bins + 2 {
        let r = reference.as_ref().map(|r| r[g]);
        if g < bins || set[g] > 0 || r.unwrap_or(0) > 0 {
            out.push(Row {
                group: match g {
                    g if g < bins => "listed",
                    g if g == bins => "rest",
                    _ => "none",
                }
                .to_string(),
                edges: (g < bins).then(|| (edges[g], edges[g + 1])),
                count: set[g],
                reference: r,
            });
        }
    }
    out
}

/// Which items a filter admits, as the oracle reads it.
type Keep<'a> = &'a dyn Fn(&Item) -> bool;

const COLUMNS: [&str; 5] = ["score", "weight", "rank", "seen", "when"];

// ---- the tests ------------------------------------------------------------------------------

/// **Every bin's count is the oracle's**, on each route a field can be counted by, for either
/// viewer, with no filter, a category filter and a region, and against the whole visible set as
/// the reference. **The default edges cover every visible value in at most the bins asked for,
/// and are the same under every filter.**
#[test]
fn bin_counts_are_the_oracles_and_the_edges_hold_still() {
    let fx = fixture();
    let engine = &fx.engine;
    let area = [100.0, 200.0, 600.0, 750.0];
    for broad in [true, false] {
        let session = fx.session(broad);
        for column in COLUMNS {
            let groupings = [bins(column, 12, None)];
            let mut held: Option<Vec<Val>> = None;
            let filters: [(&str, Option<FilterExpr>, Keep); 3] = [
                ("none", None, &|_| true),
                ("kind", Some(fx.kind_is("b")), &|i| i.kind == "b"),
                (
                    "region",
                    Some(bbox(area[0], area[1], area[2], area[3])),
                    &|i| in_box(i, area),
                ),
            ];
            for (name, filter, keep) in filters {
                let mut req = request(&groupings);
                req.filter = filter;
                req.reference = Some(Reference::Visible);
                let (head, rows) = table(engine, &session, req);
                let what = format!("{column}, broad {broad}, filter {name}");
                let edges = edges_of(&rows);
                assert!(edges.len() >= 2 && edges.len() <= 13, "{what}: {edges:?}");
                let all: Vec<&Item> = fx.visible(broad, &|_| true).collect();
                for item in &all {
                    if let Some(v) = item.value(column).filter(|v| match v {
                        Val::F(f) => f.is_finite(),
                        Val::I(_) | Val::T(_) => true,
                    }) {
                        assert!(
                            !less(v, edges[0]) && !less(edges[edges.len() - 1], v),
                            "{what}: {v:?} outside {edges:?}"
                        );
                    }
                }
                match &held {
                    None => held = Some(edges.clone()),
                    Some(held) => assert_eq!(held, &edges, "{what}: the edges moved"),
                }
                let items: Vec<&Item> = fx.visible(broad, keep).collect();
                assert_eq!(rows, expected(&edges, &items, Some(&all), column), "{what}");
                assert_eq!(head.total, items.len() as u64, "{what}");
                assert_eq!(
                    head.groups,
                    Some(
                        rows.iter()
                            .filter(|r| r.edges.is_some() && r.count > 0)
                            .count() as u64
                    )
                );
            }
        }
    }
}

/// **Readable edges**: on a number field, multiples of a step of 1, 2, 2.5 or 5 times a power of
/// ten; on an integer field, whole numbers served as integers; on a timestamp field, the first of
/// a month or a Monday or a whole day.
#[test]
fn default_edges_are_readable() {
    let fx = fixture();
    let session = fx.session(false);
    let edges = |column: &str, n: u32| {
        let groupings = [bins(column, n, None)];
        edges_of(&table(&fx.engine, &session, request(&groupings)).1)
    };
    for (column, n) in [("score", 12), ("weight", 7), ("rank", 20), ("rank", 3)] {
        let edges: Vec<f64> = edges(column, n)
            .into_iter()
            .map(|v| match v {
                Val::F(f) if column != "rank" => f,
                Val::I(i) if column == "rank" => i as f64,
                other => panic!("{column}: an edge {other:?}"),
            })
            .collect();
        let step = edges[1] - edges[0];
        let power = 10f64.powf(step.log10().floor());
        let mantissa = step / power;
        assert!(
            [1.0, 2.0, 2.5, 5.0, 10.0]
                .iter()
                .any(|m| (mantissa - m).abs() < 1e-9),
            "{column}: a step of {step}"
        );
        for e in &edges {
            assert!(
                ((e / step) - (e / step).round()).abs() < 1e-6,
                "{column}: {e} off {step}"
            );
        }
    }
    // Three years of `seen` in at most 10 bins: half years.
    let seen = edges("seen", 10);
    assert!(
        seen.iter()
            .all(|v| matches!(v, Val::T(t) if month_start(*t))),
        "{seen:?}"
    );
    // Two hundred hours of `when` in at most 12 bins: whole days.
    let when = edges("when", 12);
    assert!(
        when.iter().all(|v| matches!(v, Val::T(t) if t % DAY == 0)),
        "{when:?}"
    );
}

/// **Items the viewer may not see move no edge and no count.** The subset viewer's tables over
/// the whole corpus are the tables over a bundle holding only the items it may see, though the
/// items it may not see hold every field's smallest and largest value.
#[test]
fn invisible_items_move_no_edge_and_no_count() {
    let fx = fixture();
    let alone = fixture_of((0..N).map(built).filter(|i| i.subset).collect());
    let (all, only) = (fx.session(false), alone.session(false));
    for column in COLUMNS {
        for n in [1, 5, 12, 50] {
            let groupings = [bins(column, n, None)];
            let mut req = request(&groupings);
            req.filter = Some(fx.kind_is("a"));
            req.reference = Some(Reference::Visible);
            let mut req_alone = request(&groupings);
            req_alone.filter = Some(alone.kind_is("a"));
            req_alone.reference = Some(Reference::Visible);
            let (head, rows) = table(&fx.engine, &all, req);
            let (head_alone, rows_alone) = table(&alone.engine, &only, req_alone);
            assert_eq!(rows, rows_alone, "{column} in {n} bins");
            assert_eq!(
                (head.total, head.reference_total, head.groups),
                (
                    head_alone.total,
                    head_alone.reference_total,
                    head_alone.groups
                )
            );
        }
    }
    // The broad viewer's edges do reach the extremes.
    let broad = fx.session(true);
    let groupings = [bins("score", 12, None)];
    let edges = edges_of(&table(&fx.engine, &broad, request(&groupings)).1);
    assert!(
        matches!((edges[0], edges[edges.len() - 1]), (Val::F(lo), Val::F(hi)) if lo <= -300.0 && hi >= 500.0)
    );
}

/// **A range is cut into equal bins**, an integer's and a timestamp's into whole numbers, and
/// **the values outside it are `rest`**.
#[test]
fn a_range_cuts_equal_bins_and_counts_what_is_outside_as_rest() {
    let fx = fixture();
    let session = fx.session(true);
    let items: Vec<&Item> = fx.visible(true, &|_| true).collect();

    let groupings = [bins(
        "score",
        5,
        Some((Scalar::Int(0), Scalar::Float(50.0))),
    )];
    let (_, rows) = table(&fx.engine, &session, request(&groupings));
    let edges = edges_of(&rows);
    assert_eq!(edges, [0.0, 10.0, 20.0, 30.0, 40.0, 50.0].map(Val::F));
    assert_eq!(rows, expected(&edges, &items, None, "score"));
    assert!(rows.iter().any(|r| r.group == "rest" && r.count > 0));

    let (lo, hi) = (date(2016, 1, 1), date(2017, 1, 1) + 1);
    let groupings = [bins(
        "seen",
        4,
        Some((Scalar::Int(i128::from(lo)), Scalar::Int(i128::from(hi)))),
    )];
    let (_, rows) = table(&fx.engine, &session, request(&groupings));
    let edges = edges_of(&rows);
    let width = i128::from(hi - lo);
    assert_eq!(
        edges,
        (0..=4)
            .map(|i| Val::T(lo + (width * i / 4) as i64))
            .collect::<Vec<_>>()
    );
    assert_eq!(rows, expected(&edges, &items, None, "seen"));

    // An integer field's range is cut into whole widths that differ by at most one.
    let groupings = [bins(
        "rank",
        3,
        Some((Scalar::Int(-10), Scalar::Float(10.0))),
    )];
    let (_, rows) = table(&fx.engine, &session, request(&groupings));
    let edges = edges_of(&rows);
    assert_eq!(edges, [-10, -4, 3, 10].map(Val::I));
    assert_eq!(rows, expected(&edges, &items, None, "rank"));

    // A fractional bound has an integer field's bins cut, and served, in float.
    let groupings = [bins("rank", 4, Some((Scalar::Float(-0.5), Scalar::Int(1))))];
    let (_, rows) = table(&fx.engine, &session, request(&groupings));
    let edges = edges_of(&rows);
    assert_eq!(edges, [-0.5, -0.125, 0.25, 0.625, 1.0].map(Val::F));
    assert_eq!(rows, expected(&edges, &items, None, "rank"));
}

/// The pages of each grouping of one request, read through every response, `between` called after
/// each response with how many there have been.
fn read_paged(
    engine: &Engine,
    session: &Session,
    req: AggregateRequest<'_>,
    between: &mut dyn FnMut(usize),
) -> Vec<Vec<Vec<Row>>> {
    let mut tables: Vec<Vec<Vec<Row>>> = vec![Vec::new(); req.groupings.len()];
    let mut cursor: Option<String> = None;
    let mut responses = 0;
    loop {
        let this = AggregateRequest {
            cursor: cursor.as_deref(),
            ..req.clone()
        };
        let (collect, trailer) = respond(engine, session, this).unwrap();
        for (g, batch, _) in &collect.pages {
            // A grouping by cells alone has no `group`; only its pages are counted.
            let grouped = batch.column_by_name("group").is_some();
            tables[*g as usize].push(if grouped { rows_of(batch) } else { Vec::new() });
        }
        responses += 1;
        between(responses);
        match trailer.next {
            None => return tables,
            Some(next) => cursor = Some(next),
        }
    }
}

/// **A histogram is one page, whatever `page_rows` says**, beside a grouping by cells that pages
/// through, and it is the table read alone. **Its edges are drawn when its page is**, so a
/// suppression accepted while an earlier table pages leaves no edge bounding the suppressed value.
/// **A cursor opens only for the request it was issued for.**
#[test]
fn a_histogram_is_one_page_beside_groupings_that_page() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let cells = Grouping {
        by: None,
        cells: Some(10),
        area: None,
    };
    let groupings = [bins("rank", 20, None), cells.clone(), bins("seen", 6, None)];
    let alone = |g: usize| table(engine, &session, request(&groupings[g..g + 1])).1;
    let (rank, seen) = (alone(0), alone(2));
    assert!(rank.len() > 7 && seen.len() > 1);
    for page_rows in [1u32, 2, 7] {
        let mut req = request(&groupings);
        req.page_rows = Some(page_rows);
        req.pages = Some(1);
        let tables = read_paged(engine, &session, req, &mut |_| {});
        assert_eq!(tables[0], vec![rank.clone()], "{page_rows} rows a page");
        assert_eq!(tables[2], vec![seen.clone()], "{page_rows} rows a page");
        assert!(tables[1].len() > 1, "the cells page through");
    }

    // The largest rank is suppressed while the cells page; the histogram after them is drawn
    // without it.
    let groupings = [cells, bins("rank", 20, None)];
    let mut req = request(&groupings);
    req.page_rows = Some(2);
    req.pages = Some(1);
    let largest = item_of_id(engine, 8)
        .unwrap()
        .expect("the item holding the largest rank");
    let tables = read_paged(engine, &session, req.clone(), &mut |responses| {
        if responses == 1 {
            engine.accept_change(largest, ChangeOp::Suppress).unwrap();
        }
    });
    let shown: Vec<&Item> = fx.visible(true, &|i| i.source != 8).collect();
    let after = &tables[1][0];
    let edges = edges_of(after);
    assert_ne!(
        edges,
        edges_of(&rank),
        "the suppressed value reaches no edge"
    );
    assert_eq!(after, &expected(&edges, &shown, None, "rank"));

    let (_, trailer) = respond(engine, &session, req).unwrap();
    let token = trailer.next.expect("the cells page on");
    for other in [
        bins("rank", 19, None),
        bins("rank", 20, Some((Scalar::Int(0), Scalar::Int(10)))),
        bins("score", 20, None),
    ] {
        let others = [groupings[0].clone(), other];
        let mut refused = request(&others);
        refused.cursor = Some(&token);
        assert!(
            matches!(
                respond(engine, &session, refused),
                Err(EngineError::CursorRefused)
            ),
            "{:?}",
            others[1]
        );
    }
}

/// **What a grouping by bins cannot be is refused before anything is read**, each with its own
/// refusal.
#[test]
fn a_grouping_by_bins_that_cannot_be_served_is_refused() {
    let fx = fixture();
    let session = fx.session(true);
    let refused = |grouping: Grouping| match respond(
        &fx.engine,
        &session,
        request(std::slice::from_ref(&grouping)),
    ) {
        Err(EngineError::AggregateRefused(why)) => why,
        other => panic!("{grouping:?} answered {:?}", other.map(|(_, t)| t)),
    };
    assert_eq!(refused(bins("score", 0, None)), AggregateRefused::ZeroBins);
    assert!(matches!(
        refused(bins("score", 51, None)),
        AggregateRefused::OverCap {
            cap: "max_aggregate_bins",
            given: 51,
            limit: 50,
            ..
        }
    ));
    for range in [
        (Scalar::Int(5), Scalar::Int(5)),
        (Scalar::Float(5.5), Scalar::Int(5)),
        (
            Scalar::Int(i128::from(i64::MAX)),
            Scalar::Int(i128::from(i64::MAX) - 1),
        ),
    ] {
        assert_eq!(
            refused(bins("score", 4, Some(range))),
            AggregateRefused::EmptyRange
        );
    }
    let mut celled = bins("score", 4, None);
    celled.cells = Some(3);
    assert_eq!(refused(celled), AggregateRefused::BinsWithCells);
    for column in ["kind", "id", "nothing"] {
        assert_eq!(
            refused(bins(column, 4, None)),
            AggregateRefused::NotBinnable(column.to_string())
        );
    }
    assert_eq!(
        refused(bins("flag", 4, None)),
        AggregateRefused::BinsOnBool("flag".to_string())
    );
    assert_eq!(
        refused(bins("seen", 4, Some((Scalar::Float(0.5), Scalar::Int(10))))),
        AggregateRefused::FractionalTime("seen".to_string())
    );
    assert_eq!(
        refused(sampled("score", 4, None, 0)),
        AggregateRefused::ZeroSample
    );
}

// ---- sampled histograms ---------------------------------------------------------------------

fn sampled(column: &str, n: u32, range: Option<(Scalar, Scalar)>, sample: u64) -> Grouping {
    let mut grouping = bins(column, n, range);
    if let Some(By::Bins { sample: s, .. }) = &mut grouping.by {
        *s = Some(sample);
    }
    grouping
}

/// The cut a set of `n` items is sampled below with sample size `s`, as the contract states it.
fn cut_of(s: u64, n: u64) -> Option<u64> {
    (n > s).then(|| ((u128::from(s) << 64) / u128::from(n)) as u64)
}

/// Each item's `mosaica_id`, from the entity its unique `id` names.
fn identities(fx: &Fx) -> HashMap<u64, u64> {
    fx.items
        .iter()
        .map(|item| {
            // An ingested item holds the `id` of its key.
            let entity = match item.source.checked_sub(INGESTED) {
                Some(i) => item_of_key(&fx.engine, &format!("binned-{i}")),
                None => item_of_id(&fx.engine, item.source).unwrap(),
            }
            .expect("every item holds its id");
            (
                item.source,
                test_key()
                    .forward(ItemHigh::new(0, Tenancy::ZERO), entity)
                    .unwrap()
                    .raw(),
            )
        })
        .collect()
}

/// The items of `items` the oracle samples at sample size `s`, and the factor nothing: the cut is
/// taken over the set's size.
fn sample_of<'a>(items: &[&'a Item], s: u64, ids: &HashMap<u64, u64>) -> Vec<&'a Item> {
    match cut_of(s, items.len() as u64) {
        None => items.to_vec(),
        Some(cut) => items
            .iter()
            .copied()
            .filter(|item| ids[&item.source] < cut)
            .collect(),
    }
}

/// `rows`' counts scaled from a sample of `taken` items to a set of `n`, to the nearest whole
/// number with a half rounded up, as the contract states it.
fn scaled(mut rows: Vec<Row>, n: u64, taken: u64, reference: Option<(u64, u64)>) -> Vec<Row> {
    let scale = |c: u64, n: u64, taken: u64| match taken {
        0 => 0,
        t => ((u128::from(c) * u128::from(n) * 2 + u128::from(t)) / (2 * u128::from(t))) as u64,
    };
    for row in &mut rows {
        row.count = scale(row.count, n, taken);
        if let (Some(r), Some((rn, rt))) = (row.reference.as_mut(), reference) {
            *r = scale(*r, rn, rt);
        }
    }
    rows
}

/// Whether the contract counts a set of `n` items exactly with sample size `s`, whatever its rows:
/// where it holds at most `s` items, or its cut is above 2^58 and so wider than any band.
fn exact_by_size(s: u64, n: u64) -> bool {
    cut_of(s, n).is_none_or(|cut| cut > 1 << 58)
}

/// The items the contract counts of `items` with sample size `s`: every one where the size says
/// so, and otherwise those below the cut.
fn counted<'a>(items: &[&'a Item], s: u64, ids: &HashMap<u64, u64>) -> Vec<&'a Item> {
    match exact_by_size(s, items.len() as u64) {
        true => items.to_vec(),
        false => sample_of(items, s, ids),
    }
}

/// **A sampled histogram counts exactly the set's items below one cut and scales them, or counts
/// every item**, on every route a field's values are read by: a drawn column, an indexed one
/// copied in the bands, and one both drawn and indexed. Which of the two is decided by the set's
/// size and the sample size alone, and the head says which. With a range the
/// edges are fixed, so every count is checked against the oracle's; without one the edges are
/// drawn from the visible set, or its sample where the head says so, cover every value in it, and
/// hold still under a filter.
#[test]
fn a_sampled_histogram_counts_the_items_below_one_cut_and_scales_them() {
    let fx = fixture();
    let ids = identities(&fx);
    let area = [100.0, 200.0, 600.0, 750.0];
    let (mut sampled_tables, mut exact_tables, mut entries_read) = (0, 0, 0);
    for broad in [true, false] {
        let session = fx.session(broad);
        let all: Vec<&Item> = fx.visible(broad, &|_| true).collect();
        // 400 is wider than any band and is counted exactly; 8 and 30 are sampled where the set's
        // rows are dense enough among the band's.
        for s in [8, 30, 400] {
            for column in COLUMNS {
                let mut held: Option<Vec<Val>> = None;
                let filters: [(&str, Option<FilterExpr>, Keep); 3] = [
                    ("none", None, &|_| true),
                    ("kind", Some(fx.kind_is("b")), &|i| i.kind == "b"),
                    (
                        "region",
                        Some(bbox(area[0], area[1], area[2], area[3])),
                        &|i| in_box(i, area),
                    ),
                ];
                for (name, filter, keep) in filters {
                    let what = format!("{column}, broad {broad}, sample {s}, filter {name}");
                    let items: Vec<&Item> = fx.visible(broad, keep).collect();
                    let n = items.len() as u64;
                    let v = all.len() as u64;

                    // Default edges, from the whole visible set.
                    let groupings = [sampled(column, 12, None, s)];
                    let mut req = request(&groupings);
                    req.filter = filter.clone();
                    req.reference = Some(Reference::Visible);
                    let (head, rows, entries) = read_table(&fx.engine, &session, req);
                    entries_read += entries;
                    let counts = head.sample.expect("a sample was asked for");
                    let taken = counted(&items, s, &ids);
                    let reference = counted(&all, s, &ids);
                    assert_eq!(
                        counts,
                        mosaica_engine::TableSample {
                            sampled: !exact_by_size(s, n) || !exact_by_size(s, v),
                            items: taken.len() as u64,
                            reference_items: Some(reference.len() as u64),
                        },
                        "{what}"
                    );
                    match exact_by_size(s, n) {
                        false => sampled_tables += 1,
                        true => exact_tables += 1,
                    }
                    let edges = edges_of(&rows);
                    for item in &all {
                        if let Some(v) = item.value(column).filter(|v| match v {
                            Val::F(f) => f.is_finite(),
                            _ => true,
                        }) {
                            assert!(
                                !less(v, edges[0]) && !less(edges[edges.len() - 1], v),
                                "{what}: {v:?} outside {edges:?}"
                            );
                        }
                    }
                    match &held {
                        None => held = Some(edges.clone()),
                        Some(held) => assert_eq!(held, &edges, "{what}: the edges moved"),
                    }
                    let want = scaled(
                        expected(&edges, &taken, Some(&reference), column),
                        n,
                        taken.len() as u64,
                        Some((v, reference.len() as u64)),
                    );
                    assert_eq!(rows, want, "{what}");
                    assert_eq!(head.total, n, "{what}");

                    // A range.
                    let range = Some((Scalar::Int(-50), Scalar::Int(150)));
                    let range = match column {
                        "seen" | "when" => Some((
                            Scalar::Int(i128::from(date(2016, 1, 1))),
                            Scalar::Int(i128::from(date(2026, 1, 1))),
                        )),
                        _ => range,
                    };
                    let groupings = [sampled(column, 10, range, s)];
                    let mut req = request(&groupings);
                    req.filter = filter;
                    let (head, rows) = table(&fx.engine, &session, req);
                    let counts = head.sample.expect("a sample was asked for");
                    let taken = counted(&items, s, &ids);
                    assert_eq!(counts.items, taken.len() as u64, "{what}");
                    let edges = edges_of(&rows);
                    let want = scaled(
                        expected(&edges, &taken, None, column),
                        n,
                        taken.len() as u64,
                        None,
                    );
                    assert_eq!(rows, want, "{what}, in a range");
                }
            }
        }
    }
    assert!(
        sampled_tables > 0 && exact_tables > 0,
        "both ways of counting are reached"
    );
    assert!(entries_read > 0, "a sample is read from the bands");
}

/// **A set no larger than the sample size is counted exactly**, and says so; without a sample size
/// the head says nothing of one.
#[test]
fn a_set_within_the_sample_size_is_counted_exactly() {
    let fx = fixture();
    let session = fx.session(true);
    let exact = table(&fx.engine, &session, request(&[bins("rank", 12, None)]));
    let within = table(
        &fx.engine,
        &session,
        request(&[sampled("rank", 12, None, N)]),
    );
    assert_eq!(exact.1, within.1);
    assert_eq!(exact.0.sample, None);
    assert_eq!(
        within.0.sample,
        Some(mosaica_engine::TableSample {
            sampled: false,
            items: exact.0.total,
            reference_items: None,
        })
    );
}

/// **Whether a set is sampled depends on its size and the sample size alone.** The subset
/// viewer's set at a sample of 30 has a cut above 2^58, wider than any band, and is counted
/// exactly, and says so. A category filter's set at a sample of 8 has a cut the bands hold, and is
/// sampled, though the band holds every row of the view below the cut and the set is few of them,
/// so it is read by scanning: how a set is read never changes what it counts.
#[test]
fn whether_a_set_is_sampled_depends_on_its_size_alone() {
    let fx = fixture();
    let ids = identities(&fx);
    let cases: [(bool, u64, Option<FilterExpr>, Keep); 2] = [
        (false, 30, None, &|_| true),
        (true, 8, Some(fx.kind_is("c")), &|i| i.kind == "c"),
    ];
    for (broad, s, filter, keep) in cases {
        let session = fx.session(broad);
        let items: Vec<&Item> = fx.visible(broad, keep).collect();
        let n = items.len() as u64;
        let taken = counted(&items, s, &ids);
        for column in COLUMNS {
            let what = format!("{column}, sample {s}");
            let range = Some((Scalar::Int(-50), Scalar::Int(150)));
            let groupings = [sampled(column, 10, range, s)];
            let mut req = request(&groupings);
            req.filter = filter.clone();
            let (head, rows) = table(&fx.engine, &session, req);
            assert_eq!(
                head.sample,
                Some(mosaica_engine::TableSample {
                    sampled: !exact_by_size(s, n),
                    items: taken.len() as u64,
                    reference_items: None,
                }),
                "{what}"
            );
            let edges = edges_of(&rows);
            let want = scaled(
                expected(&edges, &taken, None, column),
                n,
                taken.len() as u64,
                None,
            );
            assert_eq!(rows, want, "{what}");
        }
    }
}

/// **Default edges are exact whether or not the counts are sampled**: they are the edges of the
/// table counted whole, they cover every visible value, though the extremes lie outside the
/// sample, and they hold still under a filter or a region.
#[test]
fn default_edges_are_exact_however_the_set_is_counted() {
    let fx = fixture();
    let ids = identities(&fx);
    let session = fx.session(true);
    let s = 8;
    let all: Vec<&Item> = fx.visible(true, &|_| true).collect();
    let visible_sample = sample_of(&all, s, &ids);
    let area = [100.0, 100.0, 140.0, 140.0];
    let in_area = |i: &Item| in_box(i, area);
    let few: Vec<&Item> = fx.visible(true, &in_area).collect();
    assert!(
        !few.is_empty() && few.len() as u64 <= s,
        "the region holds {} items",
        few.len()
    );
    for column in COLUMNS {
        let exact = edges_of(&table(&fx.engine, &session, request(&[bins(column, 12, None)])).1);
        let groupings = [sampled(column, 12, None, s)];
        let (whole_head, whole_rows) = table(&fx.engine, &session, request(&groupings));
        assert!(whole_head.sample.unwrap().sampled, "{column}");
        assert_eq!(edges_of(&whole_rows), exact, "{column}");
        for (name, filter) in [
            ("region", bbox(area[0], area[1], area[2], area[3])),
            ("kind", fx.kind_is("b")),
        ] {
            let mut req = request(&groupings);
            req.filter = Some(filter);
            let (head, rows) = table(&fx.engine, &session, req);
            assert_eq!(edges_of(&rows), exact, "{column}, {name}: the edges moved");
            if name == "region" {
                assert_eq!(
                    head.sample.unwrap(),
                    mosaica_engine::TableSample {
                        sampled: false,
                        items: few.len() as u64,
                        reference_items: None,
                    }
                );
                assert_eq!(rows, expected(&exact, &few, None, column), "{column}");
            }
        }
        let finite = |items: &[&Item]| -> Vec<Val> {
            items
                .iter()
                .filter_map(|i| i.value(column))
                .filter(|v| !matches!(v, Val::F(f) if !f.is_finite()))
                .collect()
        };
        let (seen, everything) = (finite(&visible_sample), finite(&all));
        let outside = everything
            .iter()
            .any(|&v| seen.iter().all(|&w| less(w, v)) || seen.iter().all(|&w| less(v, w)));
        assert!(outside, "{column}: the extremes lie outside the sample");
        for item in &all {
            if let Some(v) = item.value(column).filter(|v| match v {
                Val::F(f) => f.is_finite(),
                _ => true,
            }) {
                assert!(
                    !less(v, exact[0]) && !less(exact[exact.len() - 1], v),
                    "{column}"
                );
            }
        }
    }
}

/// **Items the viewer may not see never enter the sample, never change the set's size and never
/// move a default edge.** The subset viewer's sampled tables are the broad viewer's once every
/// item the subset viewer may not see is suppressed: the same items, under the same identities.
/// Some of those items lie below the cut. The unfiltered tables at a sample of 6 are read from the
/// bands, whose entries include the hidden items, and the filtered ones by scanning or whole.
#[test]
fn invisible_items_never_enter_a_sample() {
    let fx = fixture();
    let ids = identities(&fx);
    let s = 6;
    let hidden: Vec<&Item> = fx.items.iter().filter(|i| !i.subset).collect();
    let visible = fx.visible(false, &|_| true).count() as u64;
    let cut = cut_of(s, visible).unwrap();
    assert!(
        hidden.iter().filter(|i| ids[&i.source] < cut).count() > 3,
        "the fixture hides items below the cut"
    );
    let read = |session: &Session| {
        let mut band_entries = 0;
        let tables = [(s, false), (s, true), (30, true), (400, true)]
            .into_iter()
            .flat_map(|(s, filtered)| COLUMNS.iter().map(move |column| (s, filtered, column)))
            .map(|(s, filtered, column)| {
                let groupings = [sampled(column, 12, None, s)];
                let mut req = request(&groupings);
                req.filter = filtered.then(|| fx.kind_is("a"));
                req.reference = Some(Reference::Visible);
                let (head, rows, entries) = read_table(&fx.engine, session, req);
                band_entries += entries;
                (head, rows)
            })
            .collect::<Vec<_>>();
        (tables, band_entries)
    };
    let (subset, entries) = read(&fx.session(false));
    assert!(
        entries > 0,
        "the subset viewer's sample is read from the bands"
    );
    assert_ne!(
        subset,
        read(&fx.session(true)).0,
        "the hidden items change the broad viewer's tables"
    );
    for item in &hidden {
        let entity = item_of_id(&fx.engine, item.source).unwrap().unwrap();
        fx.engine
            .accept_change(entity, ChangeOp::Suppress)
            .expect("the suppression is accepted");
    }
    assert_eq!(subset, read(&fx.session(true)).0);
    assert_eq!(read(&fx.session(false)).0, subset);
}

/// **Items ingested and flushed are binned**, and a value past the old largest widens the
/// default edges; before the flush they hold no row in the view and are not counted.
#[test]
fn a_flushed_ingest_is_binned() {
    let mut fx = fixture();
    fx.engine.set_background_refresh_for_test(true);
    let session = fx.session(true);
    let mut added = Vec::new();
    let rows: Vec<UnallocatedRow> = (0..30u64)
        .map(|i| {
            let item = Item {
                source: INGESTED + i,
                kind: "a",
                score: (i % 4 != 0).then_some(1_000.0 + i as f64),
                weight: Some(i as f32),
                rank: Some(i as i32),
                seen: Some(date(2030, 6, 1) + i as i64 * DAY),
                when: (i % 3 != 0).then(|| date(2024, 3, 2) + i as i64 * HOUR),
                subset: false,
                position: ((i * 23 % 1000) as f64, (i * 41 % 1000) as f64),
                flushed: false,
            };
            let row = UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x: item.position.0,
                y: item.position.1,
                scalars: vec![
                    WalScalar::Utf8(item.kind.to_string()),
                    item.score.map_or(WalScalar::Null, WalScalar::F64),
                    item.weight.map_or(WalScalar::Null, WalScalar::F32),
                    item.rank.map_or(WalScalar::Null, WalScalar::I32),
                    item.seen.map_or(WalScalar::Null, WalScalar::TimestampUs),
                    item.when.map_or(WalScalar::Null, WalScalar::TimestampUs),
                    WalScalar::Bool(item.source.is_multiple_of(2)),
                    WalScalar::U64(key_id(&format!("binned-{i}"))),
                ],
                terms: fx.engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            };
            added.push(item);
            row
        })
        .collect();
    fx.engine
        .ingest_rows(rows, "binned".to_string(), [7u8; 32])
        .expect("the ingest is accepted");
    fx.items.extend(added);

    let check = |fx: &Fx, when: &str| {
        let items: Vec<&Item> = fx.visible(true, &|_| true).collect();
        for column in COLUMNS {
            let groupings = [bins(column, 12, None)];
            let (head, rows) = table(&fx.engine, &session, request(&groupings));
            let edges = edges_of(&rows);
            assert_eq!(
                rows,
                expected(&edges, &items, None, column),
                "{when}, {column}"
            );
            assert_eq!(head.total, items.len() as u64, "{when}");
        }
    };
    check(&fx, "before the flush");

    let flushes = fx.engine.write_executor_stats().flushes;
    fx.engine.request_flush();
    wait_until("the flush", Duration::from_secs(60), || {
        fx.engine.write_executor_stats().flushes > flushes
    });
    for item in fx.items.iter_mut() {
        item.flushed = true;
    }
    let all = fx.visible(true, &|_| true).count() as u64;
    wait_until(
        "the map to show the flushed items",
        Duration::from_secs(60),
        || {
            let request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize * 2);
            let tiles = fx.engine.viewport(&session, request).unwrap().tiles;
            tiles.iter().map(|t| t.matched).sum::<u64>() == all
        },
    );
    check(&fx, "after the flush");
    let groupings = [bins("score", 12, None)];
    let edges = edges_of(&table(&fx.engine, &session, request(&groupings)).1);
    assert!(matches!(edges[edges.len() - 1], Val::F(hi) if hi >= 1_029.0));

    // The flushed segment's bands copy its indexed columns, so a sample drawn through them is the
    // oracle's, the flushed items among it.
    let ids = identities(&fx);
    let items: Vec<&Item> = fx.visible(true, &|_| true).collect();
    for column in COLUMNS {
        let groupings = [sampled(column, 10, None, 40)];
        let (head, rows, entries) = read_table(&fx.engine, &session, request(&groupings));
        let sample = sample_of(&items, 40, &ids);
        let edges = edges_of(&rows);
        let want = scaled(
            expected(&edges, &sample, None, column),
            items.len() as u64,
            sample.len() as u64,
            None,
        );
        assert_eq!(rows, want, "{column}");
        assert_eq!(head.sample.unwrap().items, sample.len() as u64);
        assert!(entries > 0, "{column}: the sample is read from the bands");
    }
}

/// **An item joining another view at a running service carries its indexed values there, as a
/// built row does.** View `s1` is built holding the even items. The odd ones join it by a row
/// naming their `id` and position alone, and are flushed; then items both views hold are edited,
/// which writes their row in each view, and flushed. A sample of `s1` read from the bands, whose
/// copies of the indexed-only `weight` and `seen` the new rows wrote, is then the oracle's, as it
/// is on `s0`.
#[test]
fn an_item_joining_another_view_is_sampled_with_its_indexed_values() {
    let items: Vec<Item> = (0..N).map(built).collect();
    let even: Vec<Item> = items
        .iter()
        .filter(|i| i.source.is_multiple_of(2))
        .cloned()
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let root = build_views(tmp.path(), &items, &[("s0", &items), ("s1", &even)]);
    let engine = engine_at(tmp.path(), &root, 3600);
    let root = root.clone();
    let mut fx = Fx {
        _tmp: tmp,
        engine,
        items,
    };
    let ids = identities(&fx);
    let session = fx.session(true);
    let ingest = |fx: &Fx, batch: &str, view: Option<&str>, rows: Vec<IngestRow>| {
        let receipt = fx
            .engine
            .ingest(IngestRequest {
                batch_id: batch.to_string(),
                body_hash: {
                    let mut hash = [0u8; 32];
                    hash[..batch.len()].copy_from_slice(batch.as_bytes());
                    hash
                },
                view: view.map(str::to_string),
                rows,
                artifacts: Default::default(),
                strict: true,
                mosaica_id_column: false,
            })
            .expect("the batch is accepted");
        assert!(receipt.refused.is_empty(), "{batch}: {:?}", receipt.refused);
        assert_eq!(receipt.created, 0, "{batch}");
        wait_until("the flush", Duration::from_secs(60), || {
            fx.engine.request_flush();
            fx.engine.generation().buffer.is_empty()
        });
    };
    // Every declared column, `id` alone given.
    let naming = |item: &Item, position: Option<(f64, f64)>, rank: Option<i32>| IngestRow {
        mosaica_id: None,
        labels: None,
        position,
        scalars: (0..8)
            .map(|p| match p {
                3 => rank.map_or(WalScalar::Null, WalScalar::I32),
                7 => WalScalar::U64(item.source),
                _ => WalScalar::Null,
            })
            .collect(),
        scoped: Vec::new(),
        omitted: (0..7).filter(|&p| p != 3 || rank.is_none()).collect(),
    };
    let joining: Vec<IngestRow> = fx
        .items
        .iter()
        .filter(|i| !i.source.is_multiple_of(2))
        .map(|i| naming(i, Some(i.position), None))
        .collect();
    ingest(&fx, "join", Some("s1"), joining);
    let edits: Vec<IngestRow> = fx
        .items
        .iter_mut()
        .filter(|i| i.source % 3 == 1)
        .map(|i| {
            let rank = i.rank.map_or(7, |r| r + 1_000);
            i.rank = Some(rank);
            naming(i, None, Some(rank))
        })
        .collect();
    ingest(&fx, "edit", None, edits);

    let all: Vec<&Item> = fx.items.iter().collect();
    let s = 30;
    let sample = sample_of(&all, s, &ids);
    for view in ["s1", "s0"] {
        for column in COLUMNS {
            let what = format!("{view}, {column}");
            let groupings = [sampled(column, 10, None, s)];
            let mut req = request(&groupings);
            req.view = view;
            let (head, rows, entries) = read_table(&fx.engine, &session, req);
            assert!(entries > 0, "{what}: the sample is read from the bands");
            assert_eq!(head.total, N, "{what}");
            assert_eq!(head.sample.unwrap().items, sample.len() as u64, "{what}");
            let edges = edges_of(&rows);
            let want = scaled(
                expected(&edges, &sample, None, column),
                N,
                sample.len() as u64,
                None,
            );
            assert_eq!(rows, want, "{what}");
        }
    }
    // Every banded row's copy holds its item's value, as a deep verification reads it.
    mosaica_build::verify_deep(&root, &mosaica_build::VerifyOpts::default())
        .expect("the bundle verifies");
}

/// **A sample read partly from the band and partly by scanning is the oracle's.** A flush adds a
/// second segment of 6,000 items, of which the subset viewer sees 15%, against a third of the
/// built segment's. At a sample of 12 of the subset viewer's 1,900 items the cut is held by band
/// 7, about one row in 128: the built segment's dense set is read from the band, and the flushed
/// segment's sparse one, fewer than 25 of its rows a band entry, by scanning its own rows, where
/// an indexed-only field's value is found in the band's copy by row.
#[test]
fn a_sample_read_partly_from_the_band_and_partly_by_scanning_is_the_oracles() {
    let mut fx = fixture();
    let session = fx.session(false);
    let mut added = Vec::new();
    let rows: Vec<UnallocatedRow> = (0..6_000u64)
        .map(|i| {
            let mut item = built(INGESTED + i);
            item.subset = i % 20 < 3;
            item.flushed = false;
            let descriptors = match item.subset {
                true => vec![b"0".to_vec(), b"1".to_vec()],
                false => vec![b"0".to_vec()],
            };
            let row = UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                terms: fx.engine.resolve_terms(&descriptors),
                descriptors,
                x: item.position.0,
                y: item.position.1,
                scalars: vec![
                    WalScalar::Utf8(item.kind.to_string()),
                    item.score.map_or(WalScalar::Null, WalScalar::F64),
                    item.weight.map_or(WalScalar::Null, WalScalar::F32),
                    item.rank.map_or(WalScalar::Null, WalScalar::I32),
                    item.seen.map_or(WalScalar::Null, WalScalar::TimestampUs),
                    item.when.map_or(WalScalar::Null, WalScalar::TimestampUs),
                    WalScalar::Bool(item.source.is_multiple_of(2)),
                    WalScalar::U64(key_id(&format!("binned-{i}"))),
                ],
                scoped: Vec::new(),
            };
            added.push(item);
            row
        })
        .collect();
    fx.engine
        .ingest_rows(rows, "sparse".to_string(), [9u8; 32])
        .expect("the ingest is accepted");
    wait_until("the flush", Duration::from_secs(60), || {
        fx.engine.request_flush();
        fx.engine.generation().buffer.is_empty()
    });
    fx.items.extend(added);
    for item in fx.items.iter_mut() {
        item.flushed = true;
    }
    let ids = identities(&fx);
    let items: Vec<&Item> = fx.visible(false, &|_| true).collect();
    let s = 12;
    let cut = cut_of(s, items.len() as u64).unwrap();
    assert_eq!(mosaica_store::bands::band_below(cut), Some(7));
    let sample = sample_of(&items, s, &ids);
    let scanned = sample.iter().filter(|i| i.source >= INGESTED).count();
    assert!(
        scanned > 0,
        "the flushed segment's set has items below the cut"
    );
    for column in COLUMNS {
        let groupings = [sampled(column, 10, None, s)];
        let (head, rows, entries) = read_table(&fx.engine, &session, request(&groupings));
        assert!(
            entries > 0,
            "{column}: the built segment is read from the band"
        );
        assert_eq!(head.sample.unwrap().items, sample.len() as u64, "{column}");
        let edges = edges_of(&rows);
        let want = scaled(
            expected(&edges, &sample, None, column),
            items.len() as u64,
            sample.len() as u64,
            None,
        );
        assert_eq!(rows, want, "{column}");
    }
}

// ---- a field's figures ----------------------------------------------------------------------

fn summary(column: &str) -> Grouping {
    Grouping {
        by: Some(By::Summary {
            column: column.to_string(),
        }),
        cells: None,
        area: None,
    }
}

/// A field's figures as a summary's row carries them, or as the oracle works them out.
#[derive(Debug, Clone, Copy)]
struct Figures {
    items: u64,
    count: u64,
    none: u64,
    min: Option<Val>,
    max: Option<Val>,
    /// A float, or on a timestamp field the instant.
    mean: Option<Val>,
}

impl PartialEq for Figures {
    /// The oracle's float mean is summed in floats, so it is compared to within its last digits; a
    /// timestamp's is exact.
    fn eq(&self, other: &Self) -> bool {
        let near = match (self.mean, other.mean) {
            (Some(Val::F(a)), Some(Val::F(b))) => {
                (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
            }
            (a, b) => a == b,
        };
        (self.items, self.count, self.none, self.min, self.max)
            == (other.items, other.count, other.none, other.min, other.max)
            && near
    }
}

fn figures_of(batch: &RecordBatch) -> Figures {
    assert_eq!(batch.num_rows(), 1, "a summary is one row");
    let u64_of = |name: &str| {
        batch
            .column_by_name(name)
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(0)
    };
    Figures {
        items: u64_of("items"),
        count: u64_of("count"),
        none: u64_of("none"),
        min: value_at(batch, "min", 0),
        max: value_at(batch, "max", 0),
        mean: value_at(batch, "mean", 0),
    }
}

/// The figures of `column` over `items`, worked out here.
fn oracle(items: &[&Item], column: &str) -> Figures {
    let values: Vec<Val> = items.iter().filter_map(|i| i.value(column)).collect();
    let finite: Vec<Val> = values
        .iter()
        .copied()
        .filter(|v| !matches!(v, Val::F(f) if !f.is_finite()))
        .collect();
    let pick = |smaller: bool| {
        finite
            .iter()
            .copied()
            .reduce(|a, b| if less(a, b) == smaller { a } else { b })
    };
    // Each value over the count, summed with the error of each addition carried, so a sum past
    // the float range, or one a large value cancels, is still near the exact mean.
    let n = finite.len() as f64;
    let (mut sum, mut carried) = (0.0f64, 0.0f64);
    for v in &finite {
        let x = match *v {
            Val::F(f) => f,
            Val::I(i) => i as f64,
            Val::T(t) => t as f64,
        } / n;
        let next = sum + x;
        carried += if sum.abs() >= x.abs() {
            (sum - next) + x
        } else {
            (x - next) + sum
        };
        sum = next;
    }
    Figures {
        items: items.len() as u64,
        count: finite.len() as u64,
        none: (items.len() - values.len()) as u64,
        min: pick(true),
        max: pick(false),
        mean: match finite.first() {
            None => None,
            // The exact mean of the instants, to the nearest microsecond, a tie to even.
            Some(Val::T(_)) => {
                let total: i128 = finite
                    .iter()
                    .map(|v| match v {
                        Val::T(t) => i128::from(*t),
                        _ => unreachable!("one field's values are of one kind"),
                    })
                    .sum();
                let n = finite.len() as i128;
                let (whole, left) = (total.div_euclid(n), total.rem_euclid(n));
                let up = 2 * left > n || (2 * left == n && whole % 2 != 0);
                Some(Val::T((whole + i128::from(up)) as i64))
            }
            Some(_) => Some(Val::F(sum + carried)),
        },
    }
}

/// The summary of `column` in `view`, under `filter`.
fn summary_of(
    engine: &Engine,
    session: &Session,
    view: &str,
    column: &str,
    filter: Option<FilterExpr>,
) -> Figures {
    let groupings = [summary(column)];
    let mut req = request(&groupings);
    req.view = view;
    req.filter = filter;
    let (collect, trailer) = respond(engine, session, req).expect("the summary answers");
    assert_eq!(trailer.next, None, "a summary is one page");
    assert_eq!(collect.pages.len(), 1);
    figures_of(&collect.pages[0].1)
}

/// The default edges of `column` in `view`, checked against `items`, the items the viewer may see
/// there: every finite value lies within them, and the first and last bins hold one, so the edges
/// are the readable ones around the smallest and largest value.
fn check_edges(
    engine: &Engine,
    session: &Session,
    view: &str,
    column: &str,
    items: &[&Item],
    what: &str,
) {
    for sample in [None, Some(5)] {
        let groupings = [Grouping {
            by: Some(By::Bins {
                column: column.to_string(),
                bins: 12,
                range: None,
                sample,
            }),
            cells: None,
            area: None,
        }];
        let mut req = request(&groupings);
        req.view = view;
        let (_, rows) = table(engine, session, req);
        let edges = edges_of(&rows);
        let finite: Vec<Val> = items
            .iter()
            .filter_map(|i| i.value(column))
            .filter(|v| !matches!(v, Val::F(f) if !f.is_finite()))
            .collect();
        if finite.is_empty() {
            assert!(edges.is_empty(), "{what}, {column}: {edges:?}");
            continue;
        }
        for &v in &finite {
            assert!(
                !less(v, edges[0]) && !less(edges[edges.len() - 1], v),
                "{what}, {column}, sample {sample:?}: {v:?} outside {edges:?}"
            );
        }
        let bins = edges.len() - 1;
        let inside = |b: usize, v: Val| {
            !less(v, edges[b])
                && (less(v, edges[b + 1]) || (b + 1 == bins && same(v, edges[b + 1])))
        };
        assert!(
            finite.iter().any(|&v| inside(0, v)) && finite.iter().any(|&v| inside(bins - 1, v)),
            "{what}, {column}, sample {sample:?}: the end bins of {edges:?} hold no value"
        );
    }
}

/// **The summary of every field is the oracle's for each viewer, and a filter or a region leaves it
/// as it is.**
#[test]
fn a_summary_is_the_oracles_over_the_whole_visible_set() {
    let fx = fixture();
    let area = [100.0, 200.0, 600.0, 750.0];
    for broad in [true, false] {
        let session = fx.session(broad);
        let all: Vec<&Item> = fx.visible(broad, &|_| true).collect();
        for column in COLUMNS {
            let what = format!("{column}, broad {broad}");
            let want = oracle(&all, column);
            assert!(
                want.count > 0 && want.none > 0,
                "{what}: the fixture has both"
            );
            for filter in [
                None,
                Some(fx.kind_is("b")),
                Some(bbox(area[0], area[1], area[2], area[3])),
            ] {
                assert_eq!(
                    summary_of(&fx.engine, &session, "s0", column, filter),
                    want,
                    "{what}"
                );
            }
            check_edges(&fx.engine, &session, "s0", column, &all, &what);
        }
        // A summary beside other groupings is one table of its own, after the one before it.
        let groupings = [
            bins("rank", 5, None),
            summary("rank"),
            Grouping {
                by: None,
                cells: None,
                area: None,
            },
        ];
        let mut req = request(&groupings);
        req.filter = Some(fx.kind_is("a"));
        let (collect, _) = respond(&fx.engine, &session, req).unwrap();
        assert_eq!(collect.tables.len(), 3);
        assert_eq!(collect.tables[1].groups, None);
        assert_eq!(
            collect.tables[1].total,
            fx.visible(broad, &|i| i.kind == "a").count() as u64
        );
        assert_eq!(figures_of(&collect.pages[1].1), oracle(&all, "rank"));
    }
}

/// **A grant's figures are those of the key lists it satisfies.** Every item carries key `0`, and
/// every third carries `1` as well, so the base holds two key lists. The subset viewer satisfies
/// one of them and the broad viewer both, and each one's summary is the oracle's over the items of
/// its lists alone, composed from the build's tallies without a row walked.
#[test]
fn a_grant_gets_the_figures_of_the_key_lists_it_satisfies() {
    let fx = fixture();
    for broad in [true, false] {
        let session = fx.session(broad);
        let lists: Vec<&Item> = fx.visible(broad, &|_| true).collect();
        assert_eq!(
            lists.iter().all(|i| i.subset),
            !broad,
            "the subset viewer's items are the second list's"
        );
        for column in COLUMNS {
            assert_eq!(
                summary_of(&fx.engine, &session, "s0", column, None),
                oracle(&lists, column),
                "{column}, broad {broad}"
            );
        }
    }
    assert_eq!(
        fx.engine.figures_stats().field_fills,
        0,
        "no base row was walked"
    );
}

/// **A summary that cannot be served is refused**: a cell level, a category, a bool and a field
/// that is not declared.
#[test]
fn a_summary_that_cannot_be_served_is_refused() {
    let fx = fixture();
    let session = fx.session(true);
    let refused = |grouping: Grouping| {
        let groupings = [grouping];
        match respond(&fx.engine, &session, request(&groupings)) {
            Err(EngineError::AggregateRefused(why)) => why,
            other => panic!("not refused: {:?}", other.map(|(_, t)| t)),
        }
    };
    assert_eq!(
        refused(Grouping {
            cells: Some(4),
            ..summary("rank")
        }),
        AggregateRefused::SummaryWithCells
    );
    for column in ["kind", "flag", "nothing"] {
        assert_eq!(
            refused(summary(column)),
            AggregateRefused::NotSummarisable(column.to_string())
        );
    }
}

/// **Items the viewer may not see move no figure.** The subset viewer's summaries over the whole
/// corpus are its summaries over a bundle holding only the items it may see, though the items it
/// may not see hold every field's smallest and largest value.
#[test]
fn invisible_items_move_no_figure() {
    let fx = fixture();
    let alone = fixture_of((0..N).map(built).filter(|i| i.subset).collect());
    let (all, only) = (fx.session(false), alone.session(false));
    let broad = fx.session(true);
    for column in COLUMNS {
        let seen = summary_of(&fx.engine, &all, "s0", column, None);
        assert_eq!(
            seen,
            summary_of(&alone.engine, &only, "s0", column, None),
            "{column}"
        );
        let everything = summary_of(&fx.engine, &broad, "s0", column, None);
        assert!(
            everything.min != seen.min && everything.max != seen.max,
            "{column}: the hidden items hold the extremes"
        );
    }
}

/// **A value in a view the viewer cannot reach moves no edge and no figure.** Items the subset
/// viewer may see hold every field's extremes, and have rows only in view `s1`, which only the
/// broad viewer reaches.
#[test]
fn a_value_in_a_view_the_viewer_cannot_reach_moves_no_edge() {
    let mut items: Vec<Item> = (0..N).map(built).collect();
    let outside = [3u64, 6, 9, 12];
    for item in items.iter_mut().filter(|i| outside.contains(&i.source)) {
        // The first two hold the smallest values, the last two the largest.
        let sign = if item.source < 7 { -1 } else { 1 };
        item.score = Some(f64::from(sign) * 9_000.0);
        item.weight = Some(sign as f32 * 9e7);
        item.rank = Some(sign * 9_000_000);
        item.seen = Some(date(2050 + i64::from(sign) * 350, 1, 1));
        item.when = Some(date(2050 + i64::from(sign) * 250, 1, 1));
        assert!(item.subset);
    }
    let s0: Vec<Item> = items
        .iter()
        .filter(|i| !outside.contains(&i.source))
        .cloned()
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let root = build_gated_views(
        tmp.path(),
        &items,
        &[
            ("s0", &s0, None),
            ("s1", &items, Some(vec!["0".to_string()])),
        ],
    );
    let engine = engine_at(tmp.path(), &root, 3600);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let broad = engine.authorise(&full_coverage_credential()).unwrap();
    let in_s0: Vec<&Item> = s0.iter().filter(|i| i.subset).collect();
    for column in COLUMNS {
        assert_eq!(
            summary_of(&engine, &subset, "s0", column, None),
            oracle(&in_s0, column),
            "{column}"
        );
        check_edges(&engine, &subset, "s0", column, &in_s0, "s0");
        let reached: Vec<&Item> = items.iter().collect();
        assert_eq!(
            summary_of(&engine, &broad, "s1", column, None),
            oracle(&reached, column),
            "{column}: the broad viewer sees the extremes in s1"
        );
    }
    let groupings = [summary("rank")];
    let mut req = request(&groupings);
    req.view = "s1";
    assert!(matches!(
        respond(&engine, &subset, req),
        Err(EngineError::UnknownView(_))
    ));
}

/// **A float's mean is exact however large the values a deny subtracts.** Two items the subset
/// viewer may see hold 1.7e308, whose sum passes the float range, and a third holds 1e17, which
/// swallows the small values beside it in a float sum. Suppressed, they leave the mean of the
/// rest, and lifted, they bring it back.
#[test]
fn a_huge_float_suppressed_leaves_the_mean_of_the_rest() {
    let mut items: Vec<Item> = (0..N).map(built).collect();
    for item in items.iter_mut() {
        match item.source {
            3 | 6 => item.score = Some(1.7e308),
            9 => item.score = Some(1e17),
            _ => {}
        }
    }
    let fx = fixture_of(items);
    let huge = [3u64, 6, 9];
    let check = |hidden: &[u64], when: &str| {
        for broad in [true, false] {
            let session = fx.session(broad);
            let shown = |i: &Item| !hidden.contains(&i.source);
            let seen: Vec<&Item> = fx.visible(broad, &shown).collect();
            let served = summary_of(&fx.engine, &session, "s0", "score", None);
            let want = oracle(&seen, "score");
            assert_eq!(served, want, "{when}, broad {broad}");
            assert!(
                matches!(served.mean, Some(Val::F(m)) if m.is_finite()),
                "{when}: {served:?}"
            );
        }
    };
    check(&[], "planted");
    for &source in &huge {
        let entity = item_of_id(&fx.engine, source).unwrap().unwrap();
        fx.engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    }
    check(&huge, "suppressed");
    let session = fx.session(false);
    let mean = summary_of(&fx.engine, &session, "s0", "score", None).mean;
    assert!(
        matches!(mean, Some(Val::F(m)) if m.abs() < 1_000.0),
        "the huge values left no trace: {mean:?}"
    );
    for &source in &huge {
        let entity = item_of_id(&fx.engine, source).unwrap().unwrap();
        fx.engine
            .accept_change(entity, ChangeOp::Unsuppress)
            .unwrap();
    }
    check(&[], "lifted");
}

/// What the lifecycle test knows of each item beyond its values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Fate {
    suppressed: bool,
    deleted: bool,
}

/// **The figures and the default edges are the oracle's through every change a running service
/// makes**, for two viewers with different grants: after a build, an ingest before and after its
/// flush, an edit, a suppression of the items holding the visible extremes, from the very next
/// request, its lift, a deletion, a compaction and a restart. The restart reads the figures back
/// rather than walking them.
#[test]
fn the_figures_are_the_oracles_through_every_change() {
    let tmp = tempfile::tempdir().unwrap();
    let mut items: Vec<Item> = (0..N).map(built).collect();
    let root = build_bundle(tmp.path(), &items);
    let mut fates: HashMap<u64, Fate> = HashMap::new();
    let check = |engine: &Engine, items: &[Item], fates: &HashMap<u64, Fate>, when: &str| {
        for broad in [true, false] {
            let session = engine
                .authorise(&if broad {
                    full_coverage_credential()
                } else {
                    subset_credential()
                })
                .unwrap();
            let visible: Vec<&Item> = items
                .iter()
                .filter(|i| {
                    let fate = fates.get(&i.source).copied().unwrap_or_default();
                    i.flushed && (broad || i.subset) && !fate.suppressed && !fate.deleted
                })
                .collect();
            for column in COLUMNS {
                let what = format!("{when}, {column}, broad {broad}");
                assert_eq!(
                    summary_of(engine, &session, "s0", column, None),
                    oracle(&visible, column),
                    "{what}"
                );
                check_edges(engine, &session, "s0", column, &visible, &what);
            }
        }
    };
    let flush = |engine: &Engine| {
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush", Duration::from_secs(60), || {
            engine.write_executor_stats().flushes > flushes && engine.generation().buffer.is_empty()
        });
    };
    let change = |engine: &Engine, sources: &[u64], op: ChangeOp| {
        for &source in sources {
            let entity = item_of_id(engine, source)
                .unwrap()
                .expect("the item exists");
            engine
                .accept_change(entity, op)
                .expect("the change is accepted");
        }
    };
    {
        let engine = engine_at(tmp.path(), &root, 3600);
        check(&engine, &items, &fates, "built");
        assert_eq!(
            engine.figures_stats().field_fills,
            0,
            "the build's tallies are composed, not walked"
        );

        // Thirty new items, every third visible to the subset viewer, two holding new extremes.
        let mut added = Vec::new();
        let rows: Vec<UnallocatedRow> = (0..30u64)
            .map(|i| {
                let source = INGESTED + i;
                let subset = i.is_multiple_of(3);
                let item = Item {
                    source,
                    kind: "a",
                    score: (i % 4 != 0).then_some(match i {
                        3 => 7_000.0,
                        6 => -7_000.0,
                        _ => 10.0 + i as f64,
                    }),
                    weight: Some(i as f32),
                    rank: Some(match i {
                        3 => 5_000_000,
                        _ => i as i32,
                    }),
                    seen: Some(date(2031, 6, 1) + i as i64 * DAY),
                    when: (i % 3 != 0).then(|| date(2024, 3, 2) + i as i64 * HOUR),
                    subset,
                    position: ((i * 23 % 1000) as f64, (i * 41 % 1000) as f64),
                    flushed: false,
                };
                let descriptors: Vec<Vec<u8>> = match subset {
                    true => vec![b"0".to_vec(), b"1".to_vec()],
                    false => vec![b"0".to_vec()],
                };
                let row = UnallocatedRow {
                    view: "s0".to_string(),
                    join: None,
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    x: item.position.0,
                    y: item.position.1,
                    scalars: vec![
                        WalScalar::Utf8(item.kind.to_string()),
                        item.score.map_or(WalScalar::Null, WalScalar::F64),
                        item.weight.map_or(WalScalar::Null, WalScalar::F32),
                        item.rank.map_or(WalScalar::Null, WalScalar::I32),
                        item.seen.map_or(WalScalar::Null, WalScalar::TimestampUs),
                        item.when.map_or(WalScalar::Null, WalScalar::TimestampUs),
                        WalScalar::Bool(item.source.is_multiple_of(2)),
                        WalScalar::U64(source),
                    ],
                    scoped: Vec::new(),
                };
                added.push(item);
                row
            })
            .collect();
        engine
            .ingest_rows(rows, "figures".to_string(), [9u8; 32])
            .expect("the ingest is accepted");
        items.extend(added);
        check(&engine, &items, &fates, "ingested, not flushed");
        flush(&engine);
        for item in items.iter_mut() {
            item.flushed = true;
        }
        check(&engine, &items, &fates, "flushed");

        // An edit moves an item to a new entity with a row above the base.
        let edited = 30u64;
        let rank = 4_000_000;
        engine
            .ingest(IngestRequest {
                batch_id: "edit".to_string(),
                body_hash: [3u8; 32],
                view: None,
                rows: vec![IngestRow {
                    mosaica_id: None,
                    labels: None,
                    position: None,
                    scalars: (0..8)
                        .map(|p| match p {
                            3 => WalScalar::I32(rank),
                            7 => WalScalar::U64(edited),
                            _ => WalScalar::Null,
                        })
                        .collect(),
                    scoped: Vec::new(),
                    omitted: (0..7).filter(|&p| p != 3).collect(),
                }],
                artifacts: Default::default(),
                strict: true,
                mosaica_id_column: false,
            })
            .expect("the edit is accepted");
        items.iter_mut().find(|i| i.source == edited).unwrap().rank = Some(rank);
        flush(&engine);
        check(&engine, &items, &fates, "edited");

        // The items holding each viewer's extremes, base and ingested, suppressed, then lifted.
        let extremes: Vec<u64> = vec![
            1,
            2,
            4,
            5,
            8,
            10,
            11,
            13,
            14,
            16,
            INGESTED + 3,
            INGESTED + 6,
            edited,
        ];
        let shown = summary_of(
            &engine,
            &engine.authorise(&full_coverage_credential()).unwrap(),
            "s0",
            "score",
            None,
        );
        change(&engine, &extremes, ChangeOp::Suppress);
        for &source in &extremes {
            fates.entry(source).or_default().suppressed = true;
        }
        check(&engine, &items, &fates, "suppressed");
        assert_ne!(
            summary_of(
                &engine,
                &engine.authorise(&full_coverage_credential()).unwrap(),
                "s0",
                "score",
                None
            ),
            shown,
            "the suppression moved the extremes"
        );
        // More items than a side's reserve holds, from the low end of `rank`, drawn, and the
        // high end of `seen`, indexed alone: those sides are walked again.
        let spent = engine.figures_stats().reserve_spent;
        let by = |value: &dyn Fn(&Item) -> Option<i128>, low: bool, n: usize| -> Vec<u64> {
            let mut held: Vec<(i128, u64)> = items
                .iter()
                .filter(|i| !fates.get(&i.source).is_some_and(|f| f.suppressed))
                .filter_map(|i| value(i).map(|v| (v, i.source)))
                .collect();
            held.sort();
            if !low {
                held.reverse();
            }
            held.iter().take(n).map(|&(_, source)| source).collect()
        };
        let lowest = by(&|i| i.rank.map(i128::from), true, 28);
        let many: Vec<u64> = lowest[..20]
            .iter()
            .copied()
            .chain(by(&|i| i.seen.map(i128::from), false, 20))
            .collect();
        // Three of the eight the broad viewer's walk will keep, which the subset viewer cannot see,
        // so they leave its figures as they are.
        let more: Vec<u64> = lowest[20..]
            .iter()
            .copied()
            .filter(|&source| !items.iter().any(|i| i.source == source && i.subset))
            .take(3)
            .collect();
        assert_eq!(more.len(), 3);
        change(&engine, &many, ChangeOp::Suppress);
        for &source in &many {
            fates.entry(source).or_default().suppressed = true;
        }
        check(&engine, &items, &fates, "suppressed past the reserve");
        assert!(
            engine.figures_stats().reserve_spent > spent,
            "a side was walked again"
        );
        // A deny that subtracts every row that one did and three of the values its walk kept:
        // the walk is read again rather than taken again, and the answer is still the oracle's.
        let spent = engine.figures_stats().reserve_spent;
        change(&engine, &more, ChangeOp::Suppress);
        for &source in &more {
            fates.entry(source).or_default().suppressed = true;
        }
        check(
            &engine,
            &items,
            &fates,
            "suppressed further, past the reserve",
        );
        assert_eq!(
            engine.figures_stats().reserve_spent,
            spent,
            "the walk was read again"
        );
        change(&engine, &more, ChangeOp::Unsuppress);
        for &source in &more {
            fates.entry(source).or_default().suppressed = false;
        }
        change(&engine, &many, ChangeOp::Unsuppress);
        for &source in &many {
            fates.entry(source).or_default().suppressed = false;
        }
        check(&engine, &items, &fates, "lifted past the reserve");

        change(&engine, &extremes[..5], ChangeOp::Unsuppress);
        for source in &extremes[..5] {
            fates.entry(*source).or_default().suppressed = false;
        }
        check(&engine, &items, &fates, "lifted");

        // Every fourth subset item deleted, and the ingested extremes.
        let deleted: Vec<u64> = (0..N)
            .filter(|s| s % 12 == 0)
            .chain([INGESTED + 3, INGESTED + 9])
            .collect();
        change(&engine, &deleted, ChangeOp::Delete);
        for &source in &deleted {
            fates.entry(source).or_default().deleted = true;
        }
        check(&engine, &items, &fates, "deleted");

        fold(&engine);
        check(&engine, &items, &fates, "compacted");
        assert_eq!(
            engine.figures_stats().field_fills,
            0,
            "the fold's tallies are composed, not walked"
        );
    }
    let engine = engine_at(tmp.path(), &root, 3600);
    check(&engine, &items, &fates, "restarted");
    assert_eq!(engine.figures_stats().field_mismatches, 0);
    assert_eq!(
        engine.figures_stats().field_fills,
        0,
        "nothing was walked after the restart"
    );
}
