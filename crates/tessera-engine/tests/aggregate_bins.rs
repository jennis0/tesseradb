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
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::filter::{FilterExpr, FilterOperand, RegionLeaf, Scalar};
use tessera_engine::{
    AggregateCaps, AggregateHead, AggregateRefused, AggregateRequest, AggregateSink,
    AggregateTrailer, By, Engine, EngineError, Grouping, PageEnd, RecordsLimits, Reference,
    Session, SinkResult, TableHead, ViewportRequest,
};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::UnallocatedRow;
use tessera_spatial::shape::{ShapeF64, Space};

const N: u64 = 3_000;

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
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, items);
    write_pairs(&pairs, items);
    std::fs::write(dir.join("config.toml"), SCHEMA).unwrap();
    let schema = Config::parse(&dir.join("config.toml"), &HashMap::new())
        .expect("the schema parses")
        .schema;
    let out = dir.join("bundle");
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
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
                tessera_engine::CategoryQuery::Page {
                    after: None,
                    limit: 100,
                },
            )
            .unwrap()
            .expect("a category");
        let code = page.values.iter().find(|v| v.key == key).unwrap().code;
        FilterExpr::Leaf {
            column: "kind".to_string(),
            operand: FilterOperand::In(vec![tessera_types::AttrLocalId::new(code)]),
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
    let edge = |name: &str, i: usize| -> Option<Val> {
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
            other => panic!("an edge of type {other:?}"),
        })
    };
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
    let mut head = None;
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
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
        match trailer.next {
            None => return (head.expect("a table"), rows),
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

fn less(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::F(a), Val::F(b)) => a < b,
        (Val::I(a), Val::I(b)) => a < b,
        (Val::T(a), Val::T(b)) => a < b,
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
                    && (less(v, edges[b + 1]) || (b + 1 == bins && v == edges[b + 1]))
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
}

/// **A table's pages joined are the table read whole**, and **its edges are held across pages**
/// while the item holding the largest value is suppressed between them; a fresh request then
/// draws narrower edges. **A cursor opens only for the request it was issued for.**
#[test]
fn pages_join_and_hold_their_edges() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [bins("rank", 20, None), bins("seen", 6, None)];
    let whole: Vec<(TableHead, Vec<Row>)> = (0..2)
        .map(|g| table(engine, &session, request(&groupings[g..g + 1])))
        .collect();
    for page_rows in [1u32, 2, 7] {
        let mut req = request(&groupings);
        req.page_rows = Some(page_rows);
        req.pages = Some(1);
        let mut joined: Vec<Vec<Row>> = vec![Vec::new(), Vec::new()];
        let mut cursor: Option<String> = None;
        loop {
            let this = AggregateRequest {
                cursor: cursor.as_deref(),
                ..req.clone()
            };
            let (collect, trailer) = respond(engine, &session, this).unwrap();
            for (g, batch, _) in &collect.pages {
                joined[*g as usize].extend(rows_of(batch));
            }
            match trailer.next {
                None => break,
                Some(next) => cursor = Some(next),
            }
        }
        assert_eq!(joined[0], whole[0].1, "{page_rows} rows a page");
        assert_eq!(joined[1], whole[1].1, "{page_rows} rows a page");
    }

    let groupings = [bins("rank", 20, None)];
    let mut req = request(&groupings);
    req.page_rows = Some(2);
    req.pages = Some(1);
    let (first, trailer) = respond(engine, &session, req.clone()).unwrap();
    let first = rows_of(&first.pages[0].1);
    let before = edges_of(&whole[0].1);
    let largest = item_of_id(engine, 8)
        .unwrap()
        .expect("the item holding the largest rank");
    engine.accept_change(largest, ChangeOp::Suppress).unwrap();
    let fresh = edges_of(&table(engine, &session, request(&groupings)).1);
    assert_ne!(
        fresh, before,
        "the largest value no longer reaches the edges"
    );

    let token = trailer.next.clone().unwrap();
    req.cursor = Some(&token);
    req.pages = None;
    let (rest, trailer) = respond(engine, &session, req).unwrap();
    assert!(trailer.recomposed && rest.tables[0].resumed);
    let resumed: Vec<Row> = first
        .into_iter()
        .chain(rest.pages.iter().flat_map(|(_, b, _)| rows_of(b)))
        .collect();
    assert_eq!(
        edges_of(&resumed),
        before,
        "the resumed table keeps its edges"
    );
    let shown: Vec<&Item> = fx.visible(true, &|i| i.source != 8).collect();
    assert_eq!(
        resumed[2..],
        expected(&before, &shown, None, "rank")[2..],
        "the pages after the suppression count without it"
    );

    for other in [
        bins("rank", 19, None),
        bins("rank", 20, Some((Scalar::Int(0), Scalar::Int(10)))),
        bins("score", 20, None),
    ] {
        let others = [other];
        let mut refused = request(&others);
        refused.cursor = Some(&token);
        assert!(
            matches!(
                respond(engine, &session, refused),
                Err(EngineError::CursorRefused)
            ),
            "{:?}",
            others[0]
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
    for column in ["seen", "rank"] {
        assert_eq!(
            refused(bins(column, 4, Some((Scalar::Float(0.5), Scalar::Int(10))))),
            AggregateRefused::FractionalBound(column.to_string())
        );
    }
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
                source: 1_000_000 + i,
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
}
