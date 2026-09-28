//! `Engine::aggregate_stream`: how a viewer's set is distributed across a field's values, a
//! layer's artifacts and the map's cells.
//!
//! The oracle is the fixture's own source data: each item's values, position and terms are
//! computed here from its source id, and every expected count is taken by walking those items,
//! never read back from the engine. The comparisons against other routes are the viewport's
//! matched count and `/v1/items`' count, which the size of a set must equal.

mod common;
#[path = "homes/fixture.rs"]
mod homes;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Array, DictionaryArray, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Int32Type, Int8Type, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::filter::{FilterExpr, FilterOperand, RegionLeaf};
use tessera_engine::{
    AggregateCaps, AggregateHead, AggregateRequest, AggregateSink, AggregateTrailer, By,
    CancelToken, CategoryQuery, Engine, EngineError, Grouping, ItemsRequest, PageEnd, Pick,
    RecordsHead, RecordsLimits, RecordsSink, Reference, ResponseEndedBy, Session, SinkResult,
    TableHead, ViewportRequest,
};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::UnallocatedRow;
use tessera_spatial::shape::{ShapeF64, Space};
use tessera_types::AttrLocalId;

const N: u64 = 3_000;

/// `kind` is drawn and indexed, `shade` is indexed alone under a `derived` vocabulary, and `mark`
/// is drawn alone.
const SCHEMA: &str = r#"
[[vocabulary]]
name       = "kind"
width      = "u16"
value_set  = "open"
visibility = "public"

[[vocabulary]]
name       = "shade"
width      = "u8"
value_set  = "open"
visibility = "derived"

[[vocabulary]]
name       = "mark"
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
name       = "shade"
type       = "category"
index      = true
vocabulary = "shade"

[[attribute]]
name       = "mark"
type       = "category"
render     = true
vocabulary = "mark"

[[attribute]]
name   = "id"
type   = "u64"
field  = "entity_id"
unique = true
"#;

/// One item as the oracle knows it.
#[derive(Debug, Clone)]
struct Item {
    source: u64,
    kind: Option<String>,
    shade: Option<String>,
    mark: Option<String>,
    /// Visible to the subset viewer as well as the broad one.
    subset: bool,
    position: (f64, f64),
    /// Holds a row in the view: built, or ingested and flushed.
    flushed: bool,
}

impl Item {
    fn value(&self, column: &str) -> Option<&str> {
        match column {
            "kind" => self.kind.as_deref(),
            "shade" => self.shade.as_deref(),
            "mark" => self.mark.as_deref(),
            _ => panic!("no column {column}"),
        }
    }
}

fn built(s: u64) -> Item {
    let kind = ["a", "b", "c", "d", "e", "f"][(s % 7) as usize % 6];
    let kind = (!s.is_multiple_of(11)).then(|| kind.to_string());
    // `hidden` is carried only by items the subset viewer cannot see.
    let shade = if !s.is_multiple_of(3) && s % 5 == 1 {
        Some("hidden".to_string())
    } else if s.is_multiple_of(13) {
        None
    } else {
        Some(["x", "y", "z"][(s % 4) as usize % 3].to_string())
    };
    let mark = ["p", "q", "r"][((s / 2) % 4) as usize % 3];
    let mark = ((s / 2) % 4 != 3).then(|| mark.to_string());
    Item {
        source: s,
        kind,
        shade,
        mark,
        subset: subset_sees(s),
        position: (((s * 37) % 1000) as f64, ((s * 53) % 1000) as f64),
        flushed: true,
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("kind", DataType::Utf8, true),
        Field::new("shade", DataType::Utf8, true),
        Field::new("mark", DataType::Utf8, true),
    ]));
    let items: Vec<Item> = (0..N).map(built).collect();
    let text = |f: &dyn Fn(&Item) -> Option<String>| -> Arc<StringArray> {
        Arc::new(StringArray::from(items.iter().map(f).collect::<Vec<_>>()))
    };
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
            text(&|i| i.kind.clone()),
            text(&|i| i.shade.clone()),
            text(&|i| i.mark.clone()),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_bundle(dir: &Path) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points);
    write_pairs_n(&pairs, N);
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
    root: PathBuf,
    engine: Engine,
    items: Vec<Item>,
    /// The items ingested since the build, by entity, as positions in `items`.
    ingested: HashMap<u64, usize>,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_bundle(tmp.path());
    let engine = engine_at(tmp.path(), &root, 3600);
    engine.set_background_refresh_for_test(false);
    Fx {
        _tmp: tmp,
        root,
        engine,
        items: (0..N).map(built).collect(),
        ingested: HashMap::new(),
    }
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

    /// The items `broad` or the subset viewer may see with a row in the view, `keep` admits.
    fn visible<'a>(
        &'a self,
        broad: bool,
        keep: &'a dyn Fn(&Item) -> bool,
    ) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |item| item.flushed && (broad || item.subset) && keep(item))
    }

    /// A category's code for `key`, from `/v1/categories` as a broad viewer reads it.
    fn code(&self, column: &str, key: &str) -> u32 {
        let session = self.session(true);
        let page = self
            .engine
            .categories(
                &session,
                column,
                CategoryQuery::Page {
                    after: None,
                    limit: 1000,
                },
            )
            .unwrap()
            .expect("a category");
        page.values
            .iter()
            .find(|v| v.key == key)
            .unwrap_or_else(|| panic!("{column} has no value {key}"))
            .code
    }

    /// `column in keys`.
    fn is_in(&self, column: &str, keys: &[&str]) -> FilterExpr {
        FilterExpr::Leaf {
            column: column.to_string(),
            operand: FilterOperand::In(
                keys.iter()
                    .map(|key| AttrLocalId::new(self.code(column, key)))
                    .collect(),
            ),
        }
    }
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
        assert!(self.head.is_none(), "one head per response");
        self.head = Some(head.clone());
        Ok(())
    }

    fn table(&mut self, head: &TableHead) -> SinkResult {
        assert!(self.head.is_some(), "the head precedes every table");
        self.tables.push(*head);
        Ok(())
    }

    fn page(&mut self, grouping: u32, batch: &RecordBatch, end: &PageEnd) -> SinkResult {
        assert_eq!(
            self.tables.last().map(|t| t.grouping),
            Some(grouping),
            "a page follows its table's head"
        );
        self.pages.push((grouping, batch.clone(), end.clone()));
        Ok(())
    }
}

fn limits() -> RecordsLimits {
    RecordsLimits {
        max_page_rows: 100_000,
        max_page_bytes: 64 << 20,
        response_bytes: 256 << 20,
        response_time: Duration::from_secs(60),
    }
}

fn caps() -> AggregateCaps {
    AggregateCaps {
        groupings: 8,
        top: 100,
        named: 100,
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
        limits: limits(),
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
    match engine.aggregate_stream(session, req, &mut sink) {
        Ok(trailer) => {
            assert!(sink.head.is_some(), "every response carries a head");
            Ok((sink, trailer))
        }
        Err(refusal) => {
            assert!(sink.head.is_none(), "a refusal sent a head: {refusal}");
            Err(refusal)
        }
    }
}

/// One row of a table, as the response carries it.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    group: Option<String>,
    key: Option<String>,
    title: Option<String>,
    cell: Option<u64>,
    count: u64,
    reference: Option<u64>,
    lift: Option<f64>,
}

fn rows_of(batch: &RecordBatch) -> Vec<Row> {
    let column = |name: &str| batch.column_by_name(name);
    let u64s = |name: &str| {
        column(name).map(|c| c.as_any().downcast_ref::<UInt64Array>().unwrap().clone())
    };
    // A dictionary-encoded text column's value at row `i`, `None` where it is null.
    let text = |c: &dyn Array, i: usize| -> Option<String> {
        if c.is_null(i) {
            return None;
        }
        match c.data_type() {
            DataType::Dictionary(key, _) if **key == DataType::Int8 => {
                let d = c
                    .as_any()
                    .downcast_ref::<DictionaryArray<Int8Type>>()
                    .unwrap();
                let values = d.values().as_any().downcast_ref::<StringArray>().unwrap();
                Some(values.value(d.keys().value(i) as usize).to_string())
            }
            DataType::Dictionary(key, _) if **key == DataType::Int32 => {
                let d = c
                    .as_any()
                    .downcast_ref::<DictionaryArray<Int32Type>>()
                    .unwrap();
                let values = d.values().as_any().downcast_ref::<StringArray>().unwrap();
                Some(values.value(d.keys().value(i) as usize).to_string())
            }
            DataType::Utf8 => Some(
                c.as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(i)
                    .to_string(),
            ),
            DataType::UInt64 => Some(
                c.as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap()
                    .value(i)
                    .to_string(),
            ),
            other => panic!("a text column of type {other:?}"),
        }
    };
    let (cell, count, reference) = (
        u64s("cell"),
        u64s("count").unwrap(),
        u64s("reference_count"),
    );
    let lift = column("lift").map(|c| c.as_any().downcast_ref::<Float64Array>().unwrap().clone());
    (0..batch.num_rows())
        .map(|i| Row {
            group: column("group").map(|g| text(g.as_ref(), i).expect("every row has a group")),
            key: column("key").and_then(|k| text(k.as_ref(), i)),
            title: column("title").and_then(|t| text(t.as_ref(), i)),
            cell: cell.as_ref().map(|c| c.value(i)),
            count: count.value(i),
            reference: reference.as_ref().map(|r| r.value(i)),
            lift: lift
                .as_ref()
                .and_then(|l| (!l.is_null(i)).then(|| l.value(i))),
        })
        .collect()
}

/// Every table of a read, followed through its cursors: each grouping's head as first sent, and
/// its rows in order.
fn read_all(
    engine: &Engine,
    session: &Session,
    req: AggregateRequest<'_>,
) -> BTreeMap<u32, (TableHead, Vec<Row>)> {
    let mut tables: BTreeMap<u32, (TableHead, Vec<Row>)> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let this = AggregateRequest {
            cursor: cursor.as_deref(),
            ..req.clone()
        };
        let (collect, trailer) = respond(engine, session, this).expect("the read answers");
        for head in &collect.tables {
            tables.entry(head.grouping).or_insert((*head, Vec::new()));
        }
        for (grouping, batch, _) in &collect.pages {
            tables
                .get_mut(grouping)
                .expect("a page of a table whose head was sent")
                .1
                .extend(rows_of(batch));
        }
        match trailer.next {
            None => return tables,
            Some(next) => cursor = Some(next),
        }
    }
}

/// One table read whole.
fn table(engine: &Engine, session: &Session, req: AggregateRequest<'_>) -> (TableHead, Vec<Row>) {
    read_all(engine, session, req)
        .remove(&0)
        .expect("the table")
}

fn field(column: &str, pick: Pick<String>) -> Grouping {
    Grouping {
        by: Some(By::Field {
            column: column.to_string(),
            pick,
        }),
        cells: None,
    }
}

fn named(keys: &[&str]) -> Pick<String> {
    Pick::Named(keys.iter().map(|k| k.to_string()).collect())
}

fn size() -> Grouping {
    Grouping {
        by: None,
        cells: None,
    }
}

/// The table a value grouping must give, from the oracle's items: `listed` in order, where a row
/// appears for each with an item or, under `always`, with none; then the rest and none.
fn expected_values(
    items: &[&Item],
    reference: Option<&[&Item]>,
    column: &str,
    listed: &[&str],
    always: &dyn Fn(&str) -> bool,
) -> Vec<(String, Option<String>, u64, Option<u64>)> {
    let tally = |items: &[&Item]| {
        let mut by: BTreeMap<Option<String>, u64> = BTreeMap::new();
        for item in items {
            *by.entry(item.value(column).map(str::to_string))
                .or_default() += 1;
        }
        by
    };
    let set = tally(items);
    let reference = reference.map(tally);
    let of = |by: &BTreeMap<Option<String>, u64>, key: Option<&str>| {
        by.get(&key.map(str::to_string)).copied().unwrap_or(0)
    };
    let mut out = Vec::new();
    let mut listed_counts = (0u64, 0u64);
    for key in listed {
        let count = of(&set, Some(key));
        let reference_count = reference.as_ref().map(|r| of(r, Some(key)));
        listed_counts.0 += count;
        listed_counts.1 += reference_count.unwrap_or(0);
        if count > 0 || reference_count.unwrap_or(0) > 0 || always(key) {
            out.push((
                "listed".to_string(),
                Some(key.to_string()),
                count,
                reference_count,
            ));
        }
    }
    let carried = |by: &BTreeMap<Option<String>, u64>| {
        by.iter()
            .filter(|(k, _)| k.is_some())
            .map(|(_, n)| n)
            .sum::<u64>()
    };
    let rest = carried(&set) - listed_counts.0;
    let reference_rest = reference.as_ref().map(|r| carried(r) - listed_counts.1);
    if rest > 0 || reference_rest.unwrap_or(0) > 0 {
        out.push(("rest".to_string(), None, rest, reference_rest));
    }
    let none = of(&set, None);
    let reference_none = reference.as_ref().map(|r| of(r, None));
    if none > 0 || reference_none.unwrap_or(0) > 0 {
        out.push(("none".to_string(), None, none, reference_none));
    }
    out
}

/// The `n` keys with the most items, ties by key.
fn top_keys(items: &[&Item], column: &str, n: usize) -> Vec<String> {
    let mut by: BTreeMap<String, u64> = BTreeMap::new();
    for item in items {
        if let Some(value) = item.value(column) {
            *by.entry(value.to_string()).or_default() += 1;
        }
    }
    let mut ranked: Vec<(String, u64)> = by.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(n).map(|(k, _)| k).collect()
}

fn simplified(rows: &[Row]) -> Vec<(String, Option<String>, u64, Option<u64>)> {
    rows.iter()
        .map(|r| {
            (
                r.group.clone().expect("a grouped table"),
                r.key.clone(),
                r.count,
                r.reference,
            )
        })
        .collect()
}

// ---- other routes' counts -------------------------------------------------------------------

fn viewport_matched(engine: &Engine, session: &Session, filter: Option<FilterExpr>) -> u64 {
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize * 2);
    if let Some(filter) = filter {
        request = request.filter(filter);
    }
    engine
        .viewport(session, request)
        .expect("the viewport answers")
        .tiles
        .iter()
        .map(|t| t.matched)
        .sum()
}

#[derive(Default)]
struct ItemsHead(Option<RecordsHead>);

impl RecordsSink for ItemsHead {
    fn head(&mut self, head: &RecordsHead) -> SinkResult {
        self.0 = Some(head.clone());
        Ok(())
    }

    fn page(&mut self, _: &RecordBatch, _: &PageEnd) -> SinkResult {
        Ok(())
    }
}

fn items_count(engine: &Engine, session: &Session, filter: Option<FilterExpr>) -> u64 {
    let mut sink = ItemsHead::default();
    engine
        .items_stream(
            session,
            ItemsRequest {
                view: "s0",
                fields: &[],
                system_fields: &[],
                filter,
                keep_unmatched: false,
                count: true,
                order: None,
                page_rows: Some(1),
                pages: Some(1),
                cursor: None,
                limits: limits(),
                cancel: None,
            },
            &mut sink,
        )
        .expect("items answers");
    sink.0
        .and_then(|head| head.counts)
        .expect("counts were asked for")
        .matched
}

fn size_of(engine: &Engine, session: &Session, filter: Option<FilterExpr>) -> u64 {
    let groupings = [size()];
    let mut req = request(&groupings);
    req.filter = filter;
    let (head, rows) = table(engine, session, req);
    assert_eq!(rows.len(), 1, "a table with no level has one row");
    assert_eq!(rows[0].count, head.total, "the row is the set's size");
    head.total
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

// ---- the tests ------------------------------------------------------------------------------

/// **A table with no level is the set's size**, and equals what the viewport matches over the
/// whole map and what `/v1/items` counts, for either viewer, with no filter, a category filter
/// and a region answered exactly and by its cover.
#[test]
fn the_size_of_a_set_is_what_the_viewport_and_items_count() {
    let fx = fixture();
    let engine = &fx.engine;
    let region = bbox(120.0, 80.0, 610.0, 745.0);
    let filters: Vec<(&str, Option<FilterExpr>)> = vec![
        ("no filter", None),
        ("a drawn category", Some(fx.is_in("kind", &["a", "c"]))),
        ("an indexed category", Some(fx.is_in("shade", &["x"]))),
        ("a drawn-only category", Some(fx.is_in("mark", &["q"]))),
        ("a region", Some(region.clone())),
    ];
    for broad in [true, false] {
        let session = fx.session(broad);
        for (what, filter) in &filters {
            let size = size_of(engine, &session, filter.clone());
            assert_eq!(
                size,
                viewport_matched(engine, &session, filter.clone()),
                "{what}, broad {broad}: the viewport"
            );
            assert_eq!(
                size,
                items_count(engine, &session, filter.clone()),
                "{what}, broad {broad}: items"
            );
        }
        let oracle = fx.visible(broad, &|_| true).count() as u64;
        assert_eq!(size_of(engine, &session, None), oracle);
    }
    engine.set_max_region_cells(16);
    let session = fx.session(true);
    let groupings = [size()];
    let mut req = request(&groupings);
    req.filter = Some(region.clone());
    let (collect, _) = respond(engine, &session, req).unwrap();
    assert!(
        matches!(
            collect.head.and_then(|h| h.region),
            Some(tessera_engine::RegionVerdict::Cover { .. })
        ),
        "the region is answered by its cover"
    );
    let size = size_of(engine, &session, Some(region.clone()));
    assert_eq!(
        size,
        viewport_matched(engine, &session, Some(region.clone()))
    );
    assert_eq!(size, items_count(engine, &session, Some(region)));
}

/// **A value grouping's rows are the oracle's**, and so sum to the set's size: the top values and
/// a named list, then the rest and none, over a drawn and indexed field, an indexed one and a
/// drawn-only one, under filters routed in entity space and in row space, so each field is
/// counted by its records, by a pass over its drawn column and through the rows' entities.
#[test]
fn value_rows_are_the_oracles_on_every_route() {
    let fx = fixture();
    let engine = &fx.engine;
    type Keep = Box<dyn Fn(&Item) -> bool>;
    let filters: Vec<(&str, Option<FilterExpr>, Keep)> = vec![
        ("no filter", None, Box::new(|_| true)),
        (
            "an entity-routed filter",
            Some(fx.is_in("shade", &["x", "z"])),
            Box::new(|i: &Item| matches!(i.shade.as_deref(), Some("x" | "z"))),
        ),
        (
            "a row-routed filter",
            Some(fx.is_in("mark", &["p", "r"])),
            Box::new(|i: &Item| matches!(i.mark.as_deref(), Some("p" | "r"))),
        ),
    ];
    for broad in [true, false] {
        let session = fx.session(broad);
        for (what, filter, keep) in &filters {
            let items: Vec<&Item> = fx.visible(broad, keep.as_ref()).collect();
            for column in ["kind", "shade", "mark"] {
                let top = top_keys(&items, column, 2);
                let top_refs: Vec<&str> = top.iter().map(String::as_str).collect();
                let groupings = [
                    field(column, Pick::Top(2)),
                    field(column, named(&["b", "y", "q", "nothing"])),
                    size(),
                ];
                let mut req = request(&groupings);
                req.filter = filter.clone();
                let tables = read_all(engine, &session, req);
                let total = tables[&2].1[0].count;
                assert_eq!(total, items.len() as u64, "{what}, {column}: the size");
                let (head, rows) = &tables[&0];
                assert_eq!(head.total, total);
                assert_eq!(
                    simplified(rows),
                    expected_values(&items, None, column, &top_refs, &|_| false),
                    "{what}, broad {broad}, {column}: the top two"
                );
                assert_eq!(
                    rows.iter().map(|r| r.count).sum::<u64>(),
                    total,
                    "{what}, {column}: the rows sum to the size"
                );
                let distinct = items
                    .iter()
                    .filter_map(|i| i.value(column))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len() as u64;
                assert_eq!(head.groups, Some(distinct), "{what}, {column}: the groups");
                // A named value of a public vocabulary appears with no item; `nothing` names no
                // value at all.
                let minted = |key: &str| match column {
                    "kind" => ["a", "b", "c", "d", "e", "f"].contains(&key),
                    "mark" => ["p", "q", "r"].contains(&key),
                    _ => false,
                };
                let named_keys: Vec<&str> = ["b", "y", "q", "nothing"]
                    .into_iter()
                    .filter(|k| column == "shade" || minted(k))
                    .collect();
                let listable = |key: &str| {
                    minted(key)
                        || (column == "shade"
                            && fx
                                .visible(broad, &|_| true)
                                .any(|i| i.shade.as_deref() == Some(key)))
                };
                assert_eq!(
                    simplified(&tables[&1].1),
                    expected_values(&items, None, column, &named_keys, &listable),
                    "{what}, broad {broad}, {column}: the named list"
                );
            }
        }
    }
}

/// **A comparison set gives each row its count there and the lift**, over the same groups.
#[test]
fn a_reference_counts_the_same_groups_and_gives_the_lift() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let region = bbox(0.0, 0.0, 500.0, 500.0);
    let inside = |i: &Item| i.position.0 <= 500.0 && i.position.1 <= 500.0;
    for column in ["kind", "shade"] {
        let groupings = [field(column, Pick::Top(3))];
        let mut req = request(&groupings);
        req.filter = Some(region.clone());
        req.reference = Some(Reference::Visible);
        let (head, rows) = table(engine, &session, req);
        let items: Vec<&Item> = fx.visible(true, &inside).collect();
        let everything: Vec<&Item> = fx.visible(true, &|_| true).collect();
        assert!(head.total > 0 && (head.total as usize) < everything.len());
        assert_eq!(
            head.total,
            items.len() as u64,
            "the region's items all lie inside it"
        );
        assert_eq!(head.reference_total, Some(everything.len() as u64));
        let top = top_keys(&items, column, 3);
        let top: Vec<&str> = top.iter().map(String::as_str).collect();
        assert_eq!(
            simplified(&rows),
            expected_values(&items, Some(&everything), column, &top, &|_| false)
        );
        for row in &rows {
            let reference = row.reference.unwrap();
            let lift = (row.count as f64 / head.total as f64)
                / (reference as f64 / everything.len() as f64);
            assert!((row.lift.unwrap() - lift).abs() < 1e-9, "{row:?}");
        }
    }
}

/// **A `derived` value no visible item carries is withheld whole**: no row under top, nothing of it
/// in the rest, and a named request for it answers exactly as one for a key that names nothing.
#[test]
fn a_derived_value_with_no_visible_carrier_is_indistinguishable_from_no_value() {
    let fx = fixture();
    let engine = &fx.engine;
    let broad = fx.session(true);
    let subset = fx.session(false);
    let groupings = [field("shade", Pick::Top(10))];
    let (_, rows) = table(engine, &broad, request(&groupings));
    assert!(
        rows.iter().any(|r| r.key.as_deref() == Some("hidden")),
        "the broad viewer is told of it"
    );
    let (_, rows) = table(engine, &subset, request(&groupings));
    assert!(rows.iter().all(|r| r.key.as_deref() != Some("hidden")));
    let items: Vec<&Item> = fx.visible(false, &|_| true).collect();
    let keys: Vec<String> = top_keys(&items, "shade", 10);
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    assert_eq!(
        simplified(&rows),
        expected_values(&items, None, "shade", &keys, &|_| false),
        "the rest holds nothing of the withheld value"
    );

    let answer = |keys: &[&str]| {
        let groupings = [field("shade", named(keys))];
        let (collect, _) = respond(engine, &subset, request(&groupings)).unwrap();
        (
            collect.tables,
            collect
                .pages
                .into_iter()
                .map(|(g, batch, end)| (g, rows_of(&batch), end.ended_by))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(answer(&["x", "hidden"]), answer(&["x", "unminted"]));
    assert_eq!(answer(&["hidden"]), answer(&["unminted"]));
}

/// **A suppression applies from the next request**, and **between two pages of one table from the
/// next page**.
#[test]
fn a_suppression_applies_from_the_next_page() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [field("kind", named(&["a", "b", "c", "d", "e", "f"]))];
    let (_, before) = table(engine, &session, request(&groupings));

    // The first page is the first value alone.
    let mut req = request(&groupings);
    req.page_rows = Some(1);
    req.pages = Some(1);
    let (collect, trailer) = respond(engine, &session, req.clone()).unwrap();
    assert_eq!(rows_of(&collect.pages[0].1), before[..1].to_vec());

    // Suppress one item of every kind but the first.
    for s in [1u64, 2, 3, 4, 5] {
        let entity = item_of_id(engine, s).unwrap().expect("a built item");
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    }
    let lost = |key: &str| {
        [1u64, 2, 3, 4, 5]
            .iter()
            .filter(|&&s| built(s).kind.as_deref() == Some(key))
            .count() as u64
    };
    req.cursor = trailer.next.as_deref();
    req.pages = None;
    let (collect, trailer) = respond(engine, &session, req).unwrap();
    assert!(
        trailer.recomposed,
        "the page after the suppression counted a new state"
    );
    let rest: Vec<Row> = collect
        .pages
        .iter()
        .flat_map(|(_, b, _)| rows_of(b))
        .collect();
    let expected: Vec<Row> = before[1..]
        .iter()
        .map(|row| Row {
            count: row.count - lost(row.key.as_deref().unwrap_or("")),
            ..row.clone()
        })
        .collect();
    assert_eq!(rest, expected);
    assert!(collect.tables[0].resumed);

    let (_, after) = table(engine, &session, request(&groupings));
    assert_eq!(
        after.iter().map(|r| r.count).sum::<u64>(),
        before.iter().map(|r| r.count).sum::<u64>() - 5
    );
}

/// **A table's pages joined are the table read whole**, at every page size; **the listed values
/// are held** across pages while the corpus changes under them; and **a cursor opens only for the
/// request, view and viewer it was issued to**.
#[test]
fn pages_join_to_the_whole_table_and_a_cursor_is_bound_to_its_request() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [
        field("kind", Pick::Top(3)),
        size(),
        field("shade", named(&["z", "x"])),
    ];
    let whole = read_all(engine, &session, request(&groupings));
    for page_rows in [1u32, 2, 3, 5] {
        for pages in [Some(1u32), Some(2), None] {
            let mut req = request(&groupings);
            req.page_rows = Some(page_rows);
            req.pages = pages;
            let joined = read_all(engine, &session, req);
            assert_eq!(
                joined.values().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
                whole.values().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
                "{page_rows} rows a page, {pages:?} pages a response"
            );
        }
    }

    // The top three, fixed at the first page, are held when the ranking changes under them.
    let groupings = [field("kind", Pick::Top(3))];
    let mut req = request(&groupings);
    req.page_rows = Some(1);
    req.pages = Some(1);
    let (first, trailer) = respond(engine, &session, req.clone()).unwrap();
    let first = rows_of(&first.pages[0].1);
    let leader = first[0].key.clone().unwrap();
    let mut suppressed = 0;
    for item in fx
        .items
        .iter()
        .filter(|i| i.kind.as_deref() == Some(&leader))
        .take(450)
    {
        let entity = item_of_id(engine, item.source).unwrap().unwrap();
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
        suppressed += 1;
    }
    assert_eq!(suppressed, 450);
    let (_, now) = table(engine, &session, request(&groupings));
    assert_ne!(now[0].key, Some(leader.clone()), "the ranking has changed");
    req.cursor = trailer.next.as_deref();
    req.pages = None;
    let (rest, _) = respond(engine, &session, req.clone()).unwrap();
    let rest: Vec<Row> = rest.pages.iter().flat_map(|(_, b, _)| rows_of(b)).collect();
    let whole_before = whole[&0].1.clone();
    assert_eq!(
        rest.iter()
            .filter(|r| r.group.as_deref() == Some("listed"))
            .map(|r| r.key.clone())
            .collect::<Vec<_>>(),
        whole_before[1..3]
            .iter()
            .map(|r| r.key.clone())
            .collect::<Vec<_>>(),
        "the resumed table lists the values chosen at its first page"
    );

    // Another request, another view's binding, another viewer: refused.
    let token = trailer.next.clone().unwrap();
    let other = [field("kind", Pick::Top(4))];
    let mut refused = request(&other);
    refused.cursor = Some(&token);
    assert!(matches!(
        respond(engine, &session, refused),
        Err(EngineError::CursorRefused)
    ));
    let mut filtered = request(&groupings);
    filtered.filter = Some(fx.is_in("kind", &["a"]));
    filtered.cursor = Some(&token);
    assert!(matches!(
        respond(engine, &session, filtered),
        Err(EngineError::CursorRefused)
    ));
    let subset = fx.session(false);
    let mut elsewhere = request(&groupings);
    elsewhere.cursor = Some(&token);
    assert!(matches!(
        respond(engine, &subset, elsewhere),
        Err(EngineError::CursorRefused)
    ));
}

/// **A response cancelled before its first page ends with no rows** and the cursor it was given.
#[test]
fn a_response_cancelled_before_its_first_page_carries_no_rows() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [field("kind", Pick::Top(3))];
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut req = request(&groupings);
    req.cancel = Some(cancel);
    let (collect, trailer) = respond(engine, &session, req).unwrap();
    assert_eq!(trailer.ended_by, ResponseEndedBy::Deadline);
    assert_eq!((trailer.pages, trailer.rows), (0, 0));
    assert!(collect.pages.is_empty() && collect.tables.is_empty());
    assert_eq!(collect.head.unwrap().identity_key, None);
    assert!(trailer.next.is_some(), "the read can start again");
}

/// **What the request's shape decides is refused before anything is read**, naming the limit.
#[test]
fn a_request_past_a_limit_or_naming_what_cannot_be_counted_is_refused() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let refused = |groupings: &[Grouping]| respond(engine, &session, request(groupings)).err();
    assert!(matches!(
        refused(&[]),
        Some(EngineError::AggregateRefused(
            tessera_engine::AggregateRefused::NoGroupings
        ))
    ));
    let many = vec![size(); 9];
    assert!(matches!(
        refused(&many),
        Some(EngineError::AggregateRefused(
            tessera_engine::AggregateRefused::OverCap { .. }
        ))
    ));
    for bad in [
        field("kind", Pick::Top(0)),
        field("kind", Pick::Top(101)),
        field("kind", named(&[])),
        field("id", Pick::Top(3)),
        field("nothing", Pick::Top(3)),
        Grouping {
            by: None,
            cells: Some(33),
        },
    ] {
        assert!(
            matches!(
                refused(std::slice::from_ref(&bad)),
                Some(EngineError::AggregateRefused(_))
            ),
            "{bad:?}"
        );
    }
}

/// **Items ingested and not yet flushed hold no row in the view and are not counted**; once
/// flushed, and after a fold, they are.
#[test]
fn a_live_corpus_is_counted_as_the_map_shows_it() {
    let mut fx = fixture();
    fx.engine.set_background_refresh_for_test(true);
    let session = fx.session(true);
    ingest_items(&mut fx, 40, "live");

    let check = |fx: &Fx, when: &str| {
        let engine = &fx.engine;
        let items: Vec<&Item> = fx.visible(true, &|_| true).collect();
        for column in ["kind", "shade", "mark"] {
            let groupings = [field(column, Pick::Top(8)), size()];
            let tables = read_all(engine, &session, request(&groupings));
            let keys = top_keys(&items, column, 8);
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            assert_eq!(
                simplified(&tables[&0].1),
                expected_values(&items, None, column, &keys, &|_| false),
                "{when}, {column}"
            );
            assert_eq!(tables[&1].1[0].count, items.len() as u64, "{when}");
        }
        assert_eq!(
            size_of(engine, &session, None),
            viewport_matched(engine, &session, None),
            "{when}"
        );
    };
    check(&fx, "before the flush");

    flush_and_show(&mut fx, &session);
    let all = fx.visible(true, &|_| true).count() as u64;
    check(&fx, "after the flush");

    let before = fx.engine.write_executor_stats();
    fx.engine.request_fold();
    wait_for("the fold", || {
        fx.engine.write_executor_stats().folds > before.folds
    });
    wait_for("the map after the fold", || {
        viewport_matched(&fx.engine, &session, None) == all
    });
    check(&fx, "after the fold");
}

/// Ingest `n` items the broad viewer alone may see, unflushed, recording each in the oracle: half
/// carry a kind the build never saw, a third a shade it never saw.
fn ingest_items(fx: &mut Fx, n: u64, batch: &str) {
    let text = |v: &Option<String>| match v {
        Some(v) => WalScalar::Utf8(v.clone()),
        None => WalScalar::Null,
    };
    let mut added = Vec::new();
    let rows: Vec<UnallocatedRow> = (0..n)
        .map(|i| {
            let item = Item {
                source: 1_000_000 + i,
                kind: Some(["a", "g"][(i % 2) as usize].to_string()),
                shade: Some(["x", "w"][(i % 3 % 2) as usize].to_string()),
                mark: (i % 5 != 0).then(|| "p".to_string()),
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
                    text(&item.kind),
                    text(&item.shade),
                    text(&item.mark),
                    WalScalar::U64(key_id(&format!("{batch}-{i}"))),
                ],
                terms: fx.engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            };
            added.push(item);
            row
        })
        .collect();
    let entities = fx
        .engine
        .ingest_rows(rows, batch.to_string(), [9u8; 32])
        .expect("the ingest is accepted");
    for (entity, item) in entities.into_iter().zip(added) {
        fx.ingested.insert(entity.raw(), fx.items.len());
        fx.items.push(item);
    }
}

/// Flush what is buffered and wait until the map shows every item.
fn flush_and_show(fx: &mut Fx, session: &Session) {
    let flushes = fx.engine.write_executor_stats().flushes;
    fx.engine.request_flush();
    wait_for("the flush", || {
        fx.engine.write_executor_stats().flushes > flushes
    });
    for item in fx.items.iter_mut() {
        item.flushed = true;
    }
    let all = fx.visible(true, &|_| true).count() as u64;
    wait_for("the map to show the flushed items", || {
        viewport_matched(&fx.engine, session, None) == all
    });
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// **In a view of a group, only the items holding a row there are counted**: each quarter holds
/// its own items, the whole view all of them, and the set's entities restricted to the view give
/// the same counts as the set's rows, whether the set is routed in entity space or row space.
#[test]
fn a_view_counts_only_the_items_with_a_row_in_it() {
    use homes::{band_of, build_homes, open, score_of, QUARTERS};
    use tessera_engine::filter::{Endpoint, Scalar};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let views: Vec<(&str, Vec<u64>)> = QUARTERS
        .iter()
        .map(|(key, members)| (key, members.clone().collect::<Vec<u64>>()))
        .map(|(key, members)| {
            (
                if *key == "q1" {
                    "quarter:q1"
                } else {
                    "quarter:q2"
                },
                members,
            )
        })
        .chain([("s0", (0..homes::N).collect())])
        .collect();
    let every_score = FilterExpr::Leaf {
        column: "score".to_string(),
        operand: FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
            hi: None,
        },
    };
    let every_band = FilterExpr::Leaf {
        column: "band".to_string(),
        operand: FilterOperand::In((1..=3).map(AttrLocalId::new).collect()),
    };
    for (view, members) in views {
        assert!(members.iter().all(|&s| score_of(s) >= 0));
        let mut expected: BTreeMap<&str, u64> = BTreeMap::new();
        for &s in &members {
            *expected.entry(band_of(s)).or_default() += 1;
        }
        let groupings = [field("band", named(&["low", "mid", "high"])), size()];
        for (what, filter) in [
            ("no filter", None),
            ("a row-routed filter", Some(every_score.clone())),
            ("an entity-routed filter", Some(every_band.clone())),
        ] {
            let mut req = request(&groupings);
            req.view = view;
            req.filter = filter;
            let tables = read_all(&engine, &session, req);
            assert_eq!(
                tables[&1].1[0].count,
                members.len() as u64,
                "{view}, {what}"
            );
            let counted: BTreeMap<&str, u64> = tables[&0]
                .1
                .iter()
                .map(|r| {
                    let key = r.key.as_deref().unwrap();
                    (
                        ["low", "mid", "high"]
                            .into_iter()
                            .find(|k| *k == key)
                            .unwrap(),
                        r.count,
                    )
                })
                .collect();
            assert_eq!(counted, expected, "{view}, {what}");
        }
    }
}

fn with_cells(mut grouping: Grouping, depth: u8) -> Grouping {
    grouping.cells = Some(depth);
    grouping
}

/// Every row this viewer may see in the view, as the stored geometry holds it: its 64-bit
/// position and the item it belongs to.
fn stored_rows<'a>(fx: &'a Fx, session: &Session) -> Vec<(u64, &'a Item)> {
    let (generation, mask) = fx.engine.composed_mask(session, "s0").unwrap();
    let view_data = &generation.bundle.partitions["default"].views["s0"];
    let segments = tessera_engine::viewport::segments_with_row_bases("s0", view_data).unwrap();
    let tables = view_data
        .row_space
        .row_entities()
        .expect("a row-to-entity table");
    let item_of: HashMap<u64, &Item> = source_to_new_map(&fx.root, "v00000")
        .into_iter()
        .filter(|&(source, _)| source < N)
        .map(|(source, entity)| (entity, &fx.items[source as usize]))
        .chain(
            fx.ingested
                .iter()
                .map(|(&entity, &at)| (entity, &fx.items[at])),
        )
        .collect();
    let mut out = Vec::new();
    for &(segment, row_base) in &segments {
        let morton = segment.morton.u32();
        let residual = segment.columns.residual();
        for local in 0..segment.row_count as usize {
            let row = row_base + local as u32;
            if mask.count_range(row..row + 1) == 0 {
                continue;
            }
            let position = (u64::from(morton[local]) << 32) | u64::from(residual[local]);
            let entity = u64::from(tables.entity_of(row));
            out.push((position, item_of[&entity]));
        }
    }
    out
}

fn cell_at(position: u64, depth: u8) -> u64 {
    if depth == 0 {
        0
    } else {
        position >> (64 - 2 * u32::from(depth))
    }
}

/// **Every cell table is the one counted row by row** from the stored positions, at depths from
/// 0 to 32, with no group and grouped by a drawn field and an indexed one, with and without a
/// filter; **in every cell the groups add to the cell's count**; and the counts by range, which
/// the coarsest depths take, agree with the pass.
#[test]
fn every_cell_table_is_the_one_counted_row_by_row() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(false);
    let rows = stored_rows(&fx, &session);
    let kinds = ["a", "c"];
    for (what, filter, keep) in [
        (
            "no filter",
            None,
            &(|_: &Item| true) as &dyn Fn(&Item) -> bool,
        ),
        ("a filter", Some(fx.is_in("kind", &kinds)), &|i: &Item| {
            kinds.contains(&i.kind.as_deref().unwrap_or(""))
        }),
    ] {
        let kept: Vec<(u64, &Item)> = rows.iter().filter(|(_, i)| keep(i)).cloned().collect();
        let items: Vec<&Item> = kept.iter().map(|(_, i)| *i).collect();
        for depth in [0u8, 1, 2, 3, 6, 16, 20, 32] {
            let groupings = [
                with_cells(size(), depth),
                with_cells(field("kind", Pick::Top(2)), depth),
                with_cells(field("shade", Pick::Top(2)), depth),
            ];
            let mut req = request(&groupings);
            req.filter = filter.clone();
            let mut sink = Collect::default();
            let trailer = engine
                .aggregate_stream(&session, req.clone(), &mut sink)
                .unwrap();
            let cells = (1u64 << (2 * u32::from(depth.min(16)))).min(N);
            let ranges = depth <= 16 && cells * 64 <= kept.len() as u64;
            let method = if ranges { "ranges" } else { "pass" };
            assert!(
                trailer.timings.methods.contains(&(0, method)),
                "{what}, depth {depth}: {:?}",
                trailer.timings.methods
            );
            let tables = read_all(engine, &session, req);
            let mut density: BTreeMap<u64, u64> = BTreeMap::new();
            for (position, _) in &kept {
                *density.entry(cell_at(*position, depth)).or_default() += 1;
            }
            let served: Vec<(u64, u64)> = tables[&0]
                .1
                .iter()
                .map(|r| (r.cell.unwrap(), r.count))
                .collect();
            assert_eq!(
                served,
                density.clone().into_iter().collect::<Vec<_>>(),
                "{what}, depth {depth}: density"
            );
            for (grouping, column) in [(1u32, "kind"), (2, "shade")] {
                let listed = top_keys(&items, column, 2);
                let group_of = |item: &Item| match item.value(column) {
                    Some(v) if listed.iter().any(|k| k == v) => (
                        listed.iter().position(|k| k == v).unwrap(),
                        Some(v.to_string()),
                    ),
                    Some(_) => (2, None),
                    None => (3, None),
                };
                let mut expected: BTreeMap<(usize, u64), (Option<String>, u64)> = BTreeMap::new();
                for (position, item) in &kept {
                    let (g, key) = group_of(item);
                    expected
                        .entry((g, cell_at(*position, depth)))
                        .or_insert((key, 0))
                        .1 += 1;
                }
                let expected: Vec<(String, Option<String>, u64, u64)> = expected
                    .into_iter()
                    .map(|((g, cell), (key, n))| {
                        let group = ["listed", "listed", "rest", "none"][g].to_string();
                        (group, key, cell, n)
                    })
                    .collect();
                let served: Vec<(String, Option<String>, u64, u64)> = tables[&grouping]
                    .1
                    .iter()
                    .map(|r| {
                        (
                            r.group.clone().unwrap(),
                            r.key.clone(),
                            r.cell.unwrap(),
                            r.count,
                        )
                    })
                    .collect();
                assert_eq!(served, expected, "{what}, depth {depth}, {column}");
                let mut added: BTreeMap<u64, u64> = BTreeMap::new();
                for (_, _, cell, n) in &served {
                    *added.entry(*cell).or_default() += n;
                }
                assert_eq!(
                    added, density,
                    "{what}, depth {depth}, {column}: groups add up"
                );
            }
        }
    }
}

/// **A cell table's pages joined are the table read whole**, cut anywhere inside a group, with a
/// reference beside the set.
#[test]
fn a_cell_table_pages_from_any_cell() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [
        with_cells(field("kind", Pick::Top(2)), 6),
        with_cells(size(), 20),
        with_cells(field("shade", named(&["y", "x"])), 32),
    ];
    let mut req = request(&groupings);
    req.filter = Some(bbox(0.0, 0.0, 600.0, 600.0));
    req.reference = Some(Reference::Visible);
    let whole = read_all(engine, &session, req.clone());
    for (_, rows) in whole.values() {
        assert!(rows.len() > 20, "each table spans many pages");
        assert!(rows
            .iter()
            .any(|r| r.count == 0 && r.reference.unwrap() > 0));
    }
    for page_rows in [17u32, 64, 500] {
        for pages in [Some(1u32), Some(3)] {
            let mut paged = req.clone();
            paged.page_rows = Some(page_rows);
            paged.pages = pages;
            let joined = read_all(engine, &session, paged);
            assert_eq!(
                joined.values().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
                whole.values().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
                "{page_rows} rows a page, {pages:?} pages a response"
            );
        }
    }
}

// ---- layers ---------------------------------------------------------------------------------

/// The planted artifacts: a key, its members' source ids, and the label a viewer must hold.
/// `t0` and `t1` overlap; `secret` carries a label no viewer holds, so its items outside `t2` are
/// in no served artifact; `mine` carries the subset viewer's label and not the broad one's.
fn planted() -> Vec<(&'static str, Vec<u64>, Option<&'static str>)> {
    vec![
        ("t0", (0..1200).collect(), None),
        ("t1", (800..2000).collect(), None),
        ("t2", (1900..2400).collect(), None),
        ("secret", (2300..2700).collect(), Some("7")),
        ("mine", (0..600).filter(|s| s % 3 == 0).collect(), Some("1")),
    ]
}

/// A flat enumerated layer over `s0` whose artifacts carry their labels in `visibility`.
fn declaration(
    name: &str,
    layout: tessera_types::layer::ServingLayout,
) -> tessera_types::layer::LayerDeclaration {
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
        MembershipSource,
    };
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::carried("visibility"),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: Some(layout),
        shape: None,
    }
}

/// An artifact of `key` over the built items `members`, carrying `label`.
fn artifact(
    fx: &Fx,
    key: &str,
    members: &[u64],
    label: Option<&str>,
) -> tessera_lifecycle::IncomingArtifact {
    let map = source_to_new_map(&fx.root, "v00000");
    let mut artifact = tessera_lifecycle::IncomingArtifact::from_entities(
        Some(key.into()),
        members
            .iter()
            .map(|s| tessera_types::EntityId::new(map[s]))
            .collect::<Vec<_>>(),
    );
    artifact.access = Some(label.map_or_else(Vec::new, |l| vec![l.as_bytes().to_vec()]));
    artifact
}

/// Register `name` over the planted artifacts in `layout`, and answer each artifact's id.
fn plant(
    fx: &Fx,
    name: &str,
    layout: tessera_types::layer::ServingLayout,
) -> BTreeMap<&'static str, tessera_types::TesseraId> {
    let engine = &fx.engine;
    engine.register_layer(declaration(name, layout)).unwrap();
    let artifacts = planted()
        .iter()
        .map(|(key, members, label)| artifact(fx, key, members, *label))
        .collect();
    let ids = engine.publish_artifacts(name.into(), 0, artifacts).unwrap();
    tick(engine);
    planted()
        .into_iter()
        .map(|(key, _, _)| key)
        .zip(ids)
        .collect()
}

fn layer(name: &str, pick: Pick<tessera_types::TesseraId>) -> Grouping {
    Grouping {
        by: Some(By::Layer {
            layer: name.to_string(),
            level: None,
            pick,
        }),
        cells: None,
    }
}

/// **An artifact grouping's rows are the oracle's**: the served artifacts by count or as named, the
/// rest in a served artifact and in no listed one, and none in no served artifact, so an item held
/// only by a withheld artifact is in none; overlapping artifacts add to more than the set's size.
/// Artifact-major and row-major levels answer alike, and a withheld artifact named by its id
/// answers as an id naming nothing.
#[test]
fn artifact_rows_are_the_oracles_and_a_withheld_artifact_shows_nowhere() {
    use tessera_types::layer::ServingLayout;
    let fx = fixture();
    let engine = &fx.engine;
    let ids = plant(&fx, "topics/major", ServingLayout::ArtifactMajor);
    let row_ids = plant(&fx, "topics/rows", ServingLayout::RowMajorList);
    assert_eq!(
        engine.recorded_layout("topics/rows", 0),
        Some(ServingLayout::RowMajorList)
    );
    let members: BTreeMap<&str, Vec<u64>> =
        planted().into_iter().map(|(key, m, _)| (key, m)).collect();
    for broad in [true, false] {
        let session = fx.session(broad);
        let held = |label: Option<&str>| match label {
            None => true,
            Some("1") => !broad,
            Some(_) => false,
        };
        for (what, filter, keep) in [
            (
                "no filter",
                None,
                &(|_: &Item| true) as &dyn Fn(&Item) -> bool,
            ),
            (
                "a filter",
                Some(fx.is_in("kind", &["a", "b"])),
                &|i: &Item| matches!(i.kind.as_deref(), Some("a" | "b")),
            ),
        ] {
            let set: Vec<u64> = fx.visible(broad, keep).map(|i| i.source).collect();
            let visible: Vec<u64> = fx.visible(broad, &|_| true).map(|i| i.source).collect();
            let served: Vec<&str> = planted()
                .into_iter()
                .filter(|(_, m, label)| held(*label) && m.iter().any(|s| visible.contains(s)))
                .map(|(key, _, _)| key)
                .collect();
            let count = |key: &str| members[key].iter().filter(|s| set.contains(s)).count() as u64;
            let mut ranked: Vec<&str> = served.iter().copied().filter(|k| count(k) > 0).collect();
            ranked.sort_by(|a, b| count(b).cmp(&count(a)).then(ids[a].cmp(&ids[b])));
            let expected = |listed: &[&str],
                            ids: &BTreeMap<&str, tessera_types::TesseraId>,
                            always: bool| {
                let mut out: Vec<(String, Option<String>, u64)> = listed
                    .iter()
                    .filter(|k| always || count(k) > 0)
                    .map(|k| {
                        (
                            "listed".to_string(),
                            Some(ids[k].raw().to_string()),
                            count(k),
                        )
                    })
                    .collect();
                let in_any = |s: &u64, keys: &[&str]| keys.iter().any(|k| members[k].contains(s));
                let rest = set
                    .iter()
                    .filter(|s| in_any(s, &served) && !in_any(s, listed))
                    .count() as u64;
                let none = set.iter().filter(|s| !in_any(s, &served)).count() as u64;
                if rest > 0 {
                    out.push(("rest".to_string(), None, rest));
                }
                if none > 0 {
                    out.push(("none".to_string(), None, none));
                }
                out
            };
            for (name, ids) in [("topics/major", &ids), ("topics/rows", &row_ids)] {
                let simple = |rows: &[Row]| -> Vec<(String, Option<String>, u64)> {
                    rows.iter()
                        .map(|r| (r.group.clone().unwrap(), r.key.clone(), r.count))
                        .collect()
                };
                let groupings = [
                    layer(name, Pick::Top(2)),
                    layer(name, Pick::Named(vec![ids["t2"], ids["secret"], ids["t0"]])),
                ];
                let mut req = request(&groupings);
                req.filter = filter.clone();
                let tables = read_all(engine, &session, req);
                let top: Vec<&str> = ranked.iter().copied().take(2).collect();
                assert_eq!(
                    simple(&tables[&0].1),
                    expected(&top, ids, false),
                    "{name}, broad {broad}, {what}: top two"
                );
                assert_eq!(
                    tables[&0].0.groups,
                    Some(ranked.len() as u64),
                    "{name}, broad {broad}, {what}: groups"
                );
                assert_eq!(
                    simple(&tables[&1].1),
                    expected(&["t2", "t0"], ids, true),
                    "{name}, broad {broad}, {what}: named, the withheld one dropped"
                );
                let secret_id = ids["secret"];
                let nothing = tessera_types::TesseraId::new(secret_id.raw() ^ 0x5555);
                let answer = |named: Vec<tessera_types::TesseraId>| {
                    let groupings = [layer(name, Pick::Named(named))];
                    let (collect, _) = respond(engine, &session, request(&groupings)).unwrap();
                    collect
                        .pages
                        .iter()
                        .flat_map(|(_, b, _)| rows_of(b))
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    answer(vec![ids["t1"], secret_id]),
                    answer(vec![ids["t1"], nothing]),
                    "{name}: a withheld id answers as one naming nothing"
                );
            }
        }
    }
    // The two layouts give the same cells.
    let session = fx.session(false);
    for depth in [3u8, 18] {
        let read = |name: &str| {
            let groupings = [Grouping {
                by: Some(By::Layer {
                    layer: name.to_string(),
                    level: None,
                    pick: Pick::Top(3),
                }),
                cells: Some(depth),
            }];
            table(engine, &session, request(&groupings))
                .1
                .into_iter()
                .map(|r| (r.group, r.cell, r.count))
                .collect::<Vec<_>>()
        };
        let major = read("topics/major");
        assert!(!major.is_empty());
        assert_eq!(major, read("topics/rows"), "depth {depth}");
    }
    assert_eq!(
        engine.layout_fallbacks(),
        0,
        "the row-major level kept its column"
    );
    assert!(engine.columns_composed() >= 1);
}

/// **A `member_of` filter's set is what the viewport and items count**, and a layer this viewer
/// does not reach is refused as an unknown layer.
#[test]
fn a_member_of_set_is_what_the_viewport_and_items_count() {
    use tessera_types::layer::ServingLayout;
    let fx = fixture();
    let engine = &fx.engine;
    let ids = plant(&fx, "topics/major", ServingLayout::ArtifactMajor);
    for broad in [true, false] {
        let session = fx.session(broad);
        for key in ["t0", "t1", "secret", "mine"] {
            let filter = Some(FilterExpr::MemberOf(tessera_engine::filter::MemberOfLeaf {
                layer: "topics/major".to_string(),
                artifact: ids[key],
            }));
            let size = size_of(engine, &session, filter.clone());
            assert_eq!(
                size,
                viewport_matched(engine, &session, filter.clone()),
                "{key}"
            );
            assert_eq!(size, items_count(engine, &session, filter), "{key}");
        }
        let groupings = [layer("topics/nowhere", Pick::Top(2))];
        assert!(matches!(
            respond(engine, &session, request(&groupings)),
            Err(EngineError::RecordsRefused(
                tessera_engine::RecordsRefused::UnknownLayer(_)
            ))
        ));
    }
}

// ---- cells, artifacts and bindings, further ---------------------------------------------------

/// **Cells over a view of two segments**, the build's and a flush's, are the oracle's: a cell's
/// rows from both segments are added, at depths either side of 16, with no group and by a field.
#[test]
fn cells_over_the_segments_a_flush_adds_are_the_oracles() {
    let mut fx = fixture();
    fx.engine.set_background_refresh_for_test(true);
    let session = fx.session(true);
    ingest_items(&mut fx, 600, "cells");
    flush_and_show(&mut fx, &session);
    let (generation, _) = fx.engine.composed_mask(&session, "s0").unwrap();
    let segments = tessera_engine::viewport::segments_with_row_bases(
        "s0",
        &generation.bundle.partitions["default"].views["s0"],
    )
    .unwrap()
    .len();
    assert!(segments >= 2, "the flush wrote a segment of its own");
    let rows = stored_rows(&fx, &session);
    assert_eq!(rows.len(), fx.visible(true, &|_| true).count());
    let items: Vec<&Item> = rows.iter().map(|(_, i)| *i).collect();
    for depth in [3u8, 16, 20, 32] {
        let groupings = [
            with_cells(size(), depth),
            with_cells(field("kind", Pick::Top(3)), depth),
        ];
        let tables = read_all(&fx.engine, &session, request(&groupings));
        let mut density: BTreeMap<u64, u64> = BTreeMap::new();
        for (position, _) in &rows {
            *density.entry(cell_at(*position, depth)).or_default() += 1;
        }
        let served: Vec<(u64, u64)> = tables[&0]
            .1
            .iter()
            .map(|r| (r.cell.unwrap(), r.count))
            .collect();
        assert_eq!(
            served,
            density.into_iter().collect::<Vec<_>>(),
            "depth {depth}"
        );
        let listed = top_keys(&items, "kind", 3);
        let mut expected: BTreeMap<(usize, u64), (Option<String>, u64)> = BTreeMap::new();
        for (position, item) in &rows {
            let (g, key) = match item.kind.as_deref() {
                Some(v) if listed.iter().any(|k| k == v) => (
                    listed.iter().position(|k| k == v).unwrap(),
                    Some(v.to_string()),
                ),
                Some(_) => (3, None),
                None => (4, None),
            };
            expected
                .entry((g, cell_at(*position, depth)))
                .or_insert((key, 0))
                .1 += 1;
        }
        let expected: Vec<(Option<String>, u64, u64)> = expected
            .into_iter()
            .map(|((_, cell), (key, n))| (key, cell, n))
            .collect();
        let served: Vec<(Option<String>, u64, u64)> = tables[&1]
            .1
            .iter()
            .map(|r| (r.key.clone(), r.cell.unwrap(), r.count))
            .collect();
        assert_eq!(served, expected, "depth {depth}, by kind");
    }
}

/// Suppresses one item after the first page it is handed.
struct SuppressAfterFirstPage<'a> {
    inner: Collect,
    engine: &'a Engine,
    entity: Option<tessera_types::EntityId>,
}

impl AggregateSink for SuppressAfterFirstPage<'_> {
    fn head(&mut self, head: &AggregateHead) -> SinkResult {
        self.inner.head(head)
    }

    fn table(&mut self, head: &TableHead) -> SinkResult {
        self.inner.table(head)
    }

    fn page(&mut self, grouping: u32, batch: &RecordBatch, end: &PageEnd) -> SinkResult {
        self.inner.page(grouping, batch, end)?;
        if let Some(entity) = self.entity.take() {
            self.engine
                .accept_change(entity, ChangeOp::Suppress)
                .unwrap();
        }
        Ok(())
    }
}

/// **A suppression landing between two pages of one response of a cell table applies from the
/// next page**: rows the first page counted past its end are not served after it.
#[test]
fn a_suppression_between_pages_of_a_cell_table_drops_the_rows_held_past_the_page() {
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let groupings = [with_cells(size(), 16)];
    let (_, before) = table(engine, &session, request(&groupings));
    assert!(before.len() > 200);
    let target = before[120].cell.unwrap();
    let (_, item) = stored_rows(&fx, &session)
        .into_iter()
        .find(|(position, _)| cell_at(*position, 16) == target)
        .expect("an item in the cell");
    let entity = item_of_id(engine, item.source).unwrap().unwrap();
    let mut req = request(&groupings);
    req.page_rows = Some(100);
    let mut sink = SuppressAfterFirstPage {
        inner: Collect::default(),
        engine,
        entity: Some(entity),
    };
    let trailer = engine.aggregate_stream(&session, req, &mut sink).unwrap();
    assert!(trailer.next.is_none() && sink.inner.pages.len() > 2);
    assert!(trailer.recomposed);
    let served: Vec<Row> = sink
        .inner
        .pages
        .iter()
        .flat_map(|(_, b, _)| rows_of(b))
        .collect();
    let (_, after) = table(engine, &session, request(&groupings));
    assert_ne!(after, before, "the suppression changed the table");
    assert_eq!(served[..100], before[..100]);
    assert_eq!(served, after);
}

/// **An artifact by cell table is the oracle's**: each item counted in every listed artifact
/// holding it, else the rest where a served artifact holds it, else none, on both layouts.
#[test]
fn artifact_cells_are_the_oracles() {
    use tessera_types::layer::ServingLayout;
    let fx = fixture();
    let engine = &fx.engine;
    let ids = plant(&fx, "topics/major", ServingLayout::ArtifactMajor);
    let row_ids = plant(&fx, "topics/rows", ServingLayout::RowMajorList);
    for broad in [true, false] {
        let session = fx.session(broad);
        let rows = stored_rows(&fx, &session);
        let held = |label: Option<&str>| match label {
            None => true,
            Some("1") => !broad,
            Some(_) => false,
        };
        let served: Vec<(&str, Vec<u64>)> = planted()
            .into_iter()
            .filter(|(_, m, label)| held(*label) && rows.iter().any(|(_, i)| m.contains(&i.source)))
            .map(|(key, m, _)| (key, m))
            .collect();
        for (name, ids) in [("topics/major", &ids), ("topics/rows", &row_ids)] {
            let count = |members: &[u64]| {
                rows.iter()
                    .filter(|(_, i)| members.contains(&i.source))
                    .count() as u64
            };
            let mut ranked: Vec<&(&str, Vec<u64>)> = served.iter().collect();
            ranked.sort_by(|a, b| count(&b.1).cmp(&count(&a.1)).then(ids[a.0].cmp(&ids[b.0])));
            let listed: Vec<&(&str, Vec<u64>)> = ranked.into_iter().take(2).collect();
            for depth in [5u8, 20] {
                let mut expected: BTreeMap<(usize, u64), (Option<String>, u64)> = BTreeMap::new();
                for (position, item) in &rows {
                    let cell = cell_at(*position, depth);
                    let mut add = |g: usize, key: Option<String>| {
                        expected.entry((g, cell)).or_insert((key, 0)).1 += 1;
                    };
                    let mut in_listed = false;
                    for (g, (key, members)) in listed.iter().enumerate() {
                        if members.contains(&item.source) {
                            in_listed = true;
                            add(g, Some(ids[key].raw().to_string()));
                        }
                    }
                    if !in_listed {
                        if served.iter().any(|(_, m)| m.contains(&item.source)) {
                            add(listed.len(), None);
                        } else {
                            add(listed.len() + 1, None);
                        }
                    }
                }
                let expected: Vec<(Option<String>, u64, u64)> = expected
                    .into_iter()
                    .map(|((_, cell), (key, n))| (key, cell, n))
                    .collect();
                let groupings = [Grouping {
                    by: Some(By::Layer {
                        layer: name.to_string(),
                        level: None,
                        pick: Pick::Top(2),
                    }),
                    cells: Some(depth),
                }];
                let served_rows: Vec<(Option<String>, u64, u64)> =
                    table(engine, &session, request(&groupings))
                        .1
                        .into_iter()
                        .map(|r| (r.key, r.cell.unwrap(), r.count))
                        .collect();
                assert_eq!(
                    served_rows, expected,
                    "{name}, broad {broad}, depth {depth}"
                );
            }
        }
    }
}

/// Collects an artifacts read's `tessera_id` and `matched_count` columns.
#[derive(Default)]
struct Matched(BTreeMap<u64, u64>);

impl RecordsSink for Matched {
    fn head(&mut self, _: &RecordsHead) -> SinkResult {
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        let column = |name: &str| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .clone()
        };
        let (ids, matched) = (column("tessera_id"), column("matched_count"));
        for i in 0..batch.num_rows() {
            self.0.insert(ids.value(i), matched.value(i));
        }
        Ok(())
    }
}

/// **An attached layer counts as `/v1/artifacts` counts it**: an attached artifact's items are its
/// target's, whatever members it holds of its own.
#[test]
fn an_attached_layer_counts_as_the_artifacts_route_does() {
    use tessera_lifecycle::membership::IncomingAttachment;
    use tessera_types::layer::ServingLayout;
    let fx = fixture();
    let engine = &fx.engine;
    plant(&fx, "topics/major", ServingLayout::ArtifactMajor);
    let mut labels = declaration("labels/major", ServingLayout::ArtifactMajor);
    labels.depends_on = vec!["topics/major".into()];
    engine.register_layer(labels).unwrap();
    let planted = planted();
    let attached = (0..3)
        .map(|i| {
            // Its own members are the next artifact's.
            let mut label = artifact(&fx, &format!("label-{i}"), &planted[(i + 1) % 3].1, None);
            label.attached_to = Some(IncomingAttachment {
                layer: "topics/major".into(),
                level: 0,
                key: planted[i].0.to_string(),
            });
            label
        })
        .collect();
    let label_ids = engine
        .publish_artifacts("labels/major".into(), 0, attached)
        .unwrap();
    tick(engine);
    for broad in [true, false] {
        let session = fx.session(broad);
        for filter in [FilterExpr::AllOf(Vec::new()), fx.is_in("kind", &["a", "b"])] {
            let mut route = Matched::default();
            engine
                .artifacts_stream(
                    &session,
                    tessera_engine::ArtifactsRequest {
                        view: "s0",
                        layer: "labels/major",
                        level: None,
                        parent: None,
                        q: None,
                        filter: Some(filter.clone()),
                        keep_unmatched: true,
                        count: false,
                        fields: &[],
                        page_rows: None,
                        pages: None,
                        cursor: None,
                        limits: limits(),
                        cancel: None,
                    },
                    &mut route,
                )
                .unwrap();
            assert_eq!(route.0.len(), 3, "every label is served");
            let groupings = [layer("labels/major", Pick::Named(label_ids.clone()))];
            let mut req = request(&groupings);
            req.filter = Some(filter.clone());
            let (_, rows) = table(engine, &session, req);
            let counted: BTreeMap<u64, u64> = rows
                .iter()
                .filter(|r| r.group.as_deref() == Some("listed"))
                .map(|r| (r.key.as_ref().unwrap().parse().unwrap(), r.count))
                .collect();
            assert_eq!(counted, route.0, "broad {broad}, {filter:?}");
            assert!(route.0.values().any(|&n| n > 0));
        }
    }
}

/// **A stacked layer is counted one named level at a time**, and one named without a level is
/// refused.
#[test]
fn a_level_is_required_on_a_layer_with_several() {
    use tessera_types::layer::{HierarchyKind, LevelDeclaration, ServingLayout};
    let fx = fixture();
    let engine = &fx.engine;
    let mut tiers = declaration("clusters/tiers", ServingLayout::ArtifactMajor);
    tiers.hierarchy.kind = HierarchyKind::Stacked;
    tiers.levels = (0..2)
        .map(|level| LevelDeclaration {
            level,
            title: None,
            zoom: None,
        })
        .collect();
    engine.register_layer(tiers).unwrap();
    for level in [0u32, 1] {
        let artifacts = (0..3u64)
            .map(|t| {
                let members: Vec<u64> = (t * 300..t * 300 + 300 + u64::from(level) * 50).collect();
                artifact(&fx, &format!("l{level}-{t}"), &members, None)
            })
            .collect();
        engine
            .publish_artifacts("clusters/tiers".into(), level, artifacts)
            .unwrap();
    }
    tick(engine);
    let session = fx.session(true);
    let at = |level| Grouping {
        by: Some(By::Layer {
            layer: "clusters/tiers".into(),
            level,
            pick: Pick::Top(5),
        }),
        cells: None,
    };
    assert!(matches!(
        respond(engine, &session, request(&[at(None)])),
        Err(EngineError::AggregateRefused(
            tessera_engine::AggregateRefused::LevelRequired(_)
        ))
    ));
    for (level, per) in [(0u32, 300u64), (1, 350)] {
        let (_, rows) = table(engine, &session, request(&[at(Some(level))]));
        let listed: Vec<u64> = rows
            .iter()
            .filter(|r| r.group.as_deref() == Some("listed"))
            .map(|r| r.count)
            .collect();
        assert_eq!(listed, vec![per; 3], "level {level}");
    }
}

/// **A cursor opens only for its view**, and a layer dropped since is refused as unknown: its name
/// cannot be registered again, and the cursor is bound to the layer's own entity besides.
#[test]
fn a_cursor_is_refused_under_another_view_or_over_a_dropped_layer() {
    use homes::{build_homes, open};
    use tessera_types::layer::ServingLayout;
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let groupings = [with_cells(size(), 16)];
    let mut req = request(&groupings);
    req.view = "quarter:q1";
    req.page_rows = Some(1);
    req.pages = Some(1);
    let (_, trailer) = respond(&engine, &session, req.clone()).unwrap();
    let token = trailer.next.expect("more than one row");
    let mut elsewhere = req.clone();
    elsewhere.view = "quarter:q2";
    elsewhere.cursor = Some(&token);
    assert!(matches!(
        respond(&engine, &session, elsewhere),
        Err(EngineError::CursorRefused)
    ));

    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    plant(&fx, "topics/major", ServingLayout::ArtifactMajor);
    let groupings = [layer("topics/major", Pick::Top(3))];
    let mut req = request(&groupings);
    req.page_rows = Some(1);
    req.pages = Some(1);
    let (_, trailer) = respond(engine, &session, req.clone()).unwrap();
    let token = trailer.next.expect("more than one row");
    engine.drop_layer("topics/major".into()).unwrap();
    req.cursor = Some(&token);
    assert!(matches!(
        respond(engine, &session, req),
        Err(EngineError::RecordsRefused(
            tessera_engine::RecordsRefused::UnknownLayer(_)
        ))
    ));
}

/// **A region in the reference sets the response's verdict**, the coarsest of the set's and the
/// reference's.
#[test]
fn a_region_in_the_reference_gives_the_coarsest_verdict() {
    use tessera_engine::RegionVerdict;
    let fx = fixture();
    let engine = &fx.engine;
    let session = fx.session(true);
    let region = bbox(120.0, 80.0, 610.0, 745.0);
    let exact = bbox(0.0, 0.0, 1000.0, 1000.0);
    let head = |filter: Option<FilterExpr>, reference: Option<Reference>| {
        let groupings = [size()];
        let mut req = request(&groupings);
        req.filter = filter;
        req.reference = reference;
        respond(engine, &session, req)
            .unwrap()
            .0
            .head
            .unwrap()
            .region
    };
    assert_eq!(head(None, None), None);
    assert_eq!(
        head(Some(exact.clone()), Some(Reference::Filter(region.clone()))),
        Some(RegionVerdict::Exact)
    );
    engine.set_max_region_cells(16);
    assert!(matches!(
        head(Some(exact.clone()), Some(Reference::Filter(region.clone()))),
        Some(RegionVerdict::Cover { .. })
    ));
    assert!(matches!(
        head(None, Some(Reference::Filter(region))),
        Some(RegionVerdict::Cover { .. })
    ));
}
