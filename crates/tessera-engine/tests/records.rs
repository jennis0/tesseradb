//! `Engine::items_stream`: a viewer reads, page by page, every item they may see in a view that
//! matches a filter, with the fields they name.
//!
//! The oracle is the fixture's own generator: which items exist, where each is placed, what each
//! carries and which terms it holds are functions of its source id, restated here and never read
//! back from the bundle. Map order is `(cell, tessera_id)`, with the cell computed from the
//! generator's position through the build's own quantisation, and stored order is ascending item
//! number, read from the build's external-id sidecar.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, DictionaryArray, Float32Array, Float64Array,
    Int32Array, Int64Array, ListArray, StringArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Int32Type, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::{Attribute, Config, Fields};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, MemberOfLeaf, RegionLeaf, Scalar};
use tessera_engine::shapes::ShapeF64;
use tessera_engine::{
    Engine, EngineError, ItemsHead, ItemsLimits, ItemsPageEnd, ItemsRequest, ItemsSink,
    ItemsTrailer, LayerSelection, PageEndedBy, RecordsOrder, RecordsRefused, RegionVerdict,
    ResponseEndedBy, Session, SinkResult, ViewportRequest,
};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::{IncomingArtifact, UnallocatedRow};
use tessera_spatial::shape::Space;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::{Bounds, Projection};
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource,
};
use tessera_types::{AttrLocalId, EntityId, TesseraId};

const N: u64 = 2000;
const IDSET: u32 = 1;
const GEO: &str = "geo";
const Q1: &str = "quarter:q1";
const Q2: &str = "quarter:q2";
const SECRET: &str = "secret:k";
const QUARTERS: [(&str, Range<u64>); 2] = [("q1", 0..1200), ("q2", 800..2000)];
const SECRET_MEMBERS: Range<u64> = 0..500;

const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "band"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  mid = 2
  high = 3

[[vocabulary]]
name       = "kind"
width      = "u16"
value_set  = "closed"
visibility = "derived"
  [vocabulary.values]
  alpha = 1
  beta = 2

[[attribute]]
name       = "band"
type       = "category"
render     = true
index      = true
vocabulary = "band"

[[attribute]]
name       = "kind"
type       = "category"
vocabulary = "kind"

[[attribute]]
name   = "score"
type   = "i32"
render = true
index  = true

[[attribute]]
name   = "heat"
type   = "f32"
render = true

[[attribute]]
name  = "tag"
type  = "keyword"
index = true

[[attribute]]
name = "note"
type = "keyword"

[[attribute]]
name     = "prose"
type     = "text"
index    = true
analyser = "unicode"

[[attribute]]
name  = "when"
type  = "timestamp_us"
index = true

[[attribute]]
name   = "flag"
type   = "bool"
render = true
"#;

// ---------------------------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------------------------

fn position(s: u64) -> (f64, f64) {
    (((s * 37) % 1000) as f64 + 0.25, ((s * 53) % 1000) as f64 + 0.75)
}

fn geo_position(s: u64) -> (f64, f64) {
    (
        -170.0 + ((s * 13) % 340) as f64 + 0.5,
        -80.0 + ((s * 7) % 160) as f64 + 0.25,
    )
}

fn in_geo(s: u64) -> bool {
    s.is_multiple_of(2)
}

fn band_of(s: u64) -> Option<&'static str> {
    (!s.is_multiple_of(11)).then(|| ["low", "mid", "high"][(s % 3) as usize])
}

fn band_code(key: &str) -> u32 {
    match key {
        "low" => 1,
        "mid" => 2,
        _ => 3,
    }
}

fn kind_of(s: u64) -> Option<&'static str> {
    (!s.is_multiple_of(13)).then(|| ["alpha", "beta"][(s % 2) as usize])
}

fn score_of(s: u64) -> Option<i32> {
    (!s.is_multiple_of(4)).then_some(s as i32)
}

fn heat_of(s: u64) -> Option<f32> {
    (!s.is_multiple_of(5)).then(|| (s % 100) as f32 / 10.0)
}

fn tag_of(s: u64) -> Option<String> {
    (s % 5 != 1).then(|| format!("tag-{}", s % 7))
}

fn note_of(s: u64) -> Option<String> {
    (!s.is_multiple_of(7)).then(|| format!("note-{s:05}"))
}

fn prose_of(s: u64) -> String {
    format!("shared prose group{}", s % 3)
}

fn when_of(s: u64) -> Option<i64> {
    (!s.is_multiple_of(6)).then_some(1_700_000_000_000_000 + s as i64)
}

fn flag_of(s: u64) -> Option<bool> {
    (!s.is_multiple_of(9)).then_some(s.is_multiple_of(2))
}

fn sentiment(slot: usize, s: u64) -> Option<f32> {
    (!(s + slot as u64).is_multiple_of(3)).then(|| ((s * 7 + slot as u64 * 11) % 10) as f32 / 10.0)
}

fn hush(s: u64) -> i32 {
    s as i32 * 2
}

fn column(name: &str, array: ArrayRef) -> (Field, ArrayRef) {
    (Field::new(name, array.data_type().clone(), true), array)
}

fn write_parquet(path: &Path, columns: Vec<(Field, ArrayRef)>) {
    let (fields, arrays): (Vec<Field>, Vec<ArrayRef>) = columns.into_iter().unzip();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn geometry(ids: &[u64], at: impl Fn(u64) -> (f64, f64)) -> Vec<(Field, ArrayRef)> {
    vec![
        (
            Field::new("entity_id", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ids.to_vec())) as ArrayRef,
        ),
        (
            Field::new("x", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&s| at(s).0))),
        ),
        (
            Field::new("y", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&s| at(s).1))),
        ),
    ]
}

fn write_world(path: &Path) {
    let ids: Vec<u64> = (0..N).collect();
    let mut columns = geometry(&ids, position);
    let strings = |f: &dyn Fn(u64) -> Option<String>| -> ArrayRef {
        Arc::new(StringArray::from(ids.iter().map(|&s| f(s)).collect::<Vec<_>>()))
    };
    columns.push(column("band", strings(&|s| band_of(s).map(String::from))));
    columns.push(column("kind", strings(&|s| kind_of(s).map(String::from))));
    columns.push(column(
        "score",
        Arc::new(Int32Array::from(ids.iter().map(|&s| score_of(s)).collect::<Vec<_>>())),
    ));
    columns.push(column(
        "heat",
        Arc::new(Float32Array::from(ids.iter().map(|&s| heat_of(s)).collect::<Vec<_>>())),
    ));
    columns.push(column("tag", strings(&tag_of)));
    columns.push(column("note", strings(&note_of)));
    columns.push(column("prose", strings(&|s| Some(prose_of(s)))));
    columns.push(column(
        "when",
        Arc::new(Int64Array::from(ids.iter().map(|&s| when_of(s)).collect::<Vec<_>>())),
    ));
    columns.push(column(
        "flag",
        Arc::new(BooleanArray::from(ids.iter().map(|&s| flag_of(s)).collect::<Vec<_>>())),
    ));
    write_parquet(path, columns);
}

fn view_args(
    view: &str,
    points: &Path,
    pairs: &Path,
    projection: Projection,
    extent: Bounds,
) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection,
        extent,
        points: points.to_path_buf(),
        point_fields: Fields::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

fn unit() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

fn scoped(
    name: &str,
    ty: ScalarType,
    analyser: Option<&str>,
    group: &str,
    views: Vec<usize>,
) -> ScopedColumnFamily {
    ScopedColumnFamily {
        attribute: Attribute {
            name: name.to_string(),
            title: None,
            field: None,
            ty,
            analyser: analyser.map(|name| {
                tessera_analyse::identity_of(name).expect("the analyser is carried")
            }),
            vocabulary: None,
            value_set: None,
            index: true,
            render: false,
        },
        group: group.to_string(),
        views,
        source: None,
    }
}

fn group(name: &str, keys: &[&str], visibility: Option<Vec<String>>) -> GroupDescriptor {
    let e = extent();
    GroupDescriptor {
        title: None,
        point_default: Some("public".to_string()),
        visibility,
        name: name.to_string(),
        members_of: None,
        views: keys
            .iter()
            .map(|key| GroupViewDescriptor {
                key: key.to_string(),
                visibility: None,
                metadata: Default::default(),
            })
            .collect(),
        quantisation: Quantisation {
            x_min: e.x_min,
            x_max: e.x_max,
            y_min: e.y_min,
            y_max: e.y_max,
        },
        projection: Projection::None,
        metadata: Vec::new(),
        scoped_scalars: Vec::new(),
    }
}

/// The bundle: the plain view `s0` over every item with the schema above, a web-mercator view
/// `geo` over the even ones, a group `quarter` of two overlapping keys carrying `sentiment` and
/// the text family `blurb`, and a group `secret`, gated on term "1", carrying `hush`.
fn build_bundle(dir: &Path) -> PathBuf {
    build_bundle_under(dir, IDSET)
}

/// [`build_bundle`] under `idset`, with the same identity key.
fn build_bundle_under(dir: &Path, idset: u32) -> PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, N);
    let world = dir.join("world.parquet");
    write_world(&world);
    let geo = dir.join("geo.parquet");
    let geo_ids: Vec<u64> = (0..N).filter(|&s| in_geo(s)).collect();
    write_parquet(&geo, geometry(&geo_ids, geo_position));
    let mut views = vec![
        view_args("s0", &world, &pairs, Projection::None, extent()),
        view_args(GEO, &geo, &pairs, Projection::WebMercator, unit()),
    ];
    let mut quarter_views = Vec::new();
    for (slot, (key, members)) in QUARTERS.iter().enumerate() {
        let path = dir.join(format!("quarter-{key}.parquet"));
        let ids: Vec<u64> = members.clone().collect();
        let mut columns = geometry(&ids, position);
        columns.push(column(
            "sentiment",
            Arc::new(Float32Array::from(
                ids.iter().map(|&s| sentiment(slot, s)).collect::<Vec<_>>(),
            )),
        ));
        columns.push(column(
            "blurb",
            Arc::new(StringArray::from(
                ids.iter().map(|&s| format!("blurb {s}")).collect::<Vec<_>>(),
            )),
        ));
        write_parquet(&path, columns);
        quarter_views.push(views.len());
        views.push(view_args(&format!("quarter:{key}"), &path, &pairs, Projection::None, extent()));
    }
    let secret = dir.join("secret.parquet");
    let secret_ids: Vec<u64> = SECRET_MEMBERS.collect();
    let mut columns = geometry(&secret_ids, position);
    columns.push(column(
        "hush",
        Arc::new(Int32Array::from_iter_values(secret_ids.iter().map(|&s| hush(s)))),
    ));
    write_parquet(&secret, columns);
    let secret_view = views.len();
    views.push(view_args(SECRET, &secret, &pairs, Projection::None, extent()));

    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the schema parses");
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            group("quarter", &["q1", "q2"], None),
            group("secret", &["k"], Some(vec!["1".to_string()])),
        ],
        scoped_attributes: vec![
            scoped("sentiment", ScalarType::F32, None, "quarter", quarter_views.clone()),
            scoped("blurb", ScalarType::Text, Some("unicode"), "quarter", quarter_views),
            scoped("hush", ScalarType::I32, None, "secret", vec![secret_view]),
        ],
        attribute_sources: tessera_build::config::AttributeSource::over(world.clone(), &schema),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset,
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
    })
    .expect("the records fixture builds");
    out
}

struct Fx {
    _tmp: tempfile::TempDir,
    engine: Engine,
    /// Source id to entity id, for every built item and every one a case ingests.
    entity: BTreeMap<u64, u64>,
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = build_bundle(tmp.path());
        let engine = engine_at(tmp.path(), &root, 3600);
        engine.set_background_refresh_for_test(false);
        let entity = source_to_new_map(&root, "v00000");
        Fx {
            _tmp: tmp,
            engine,
            entity,
        }
    }

    fn tid(&self, s: u64) -> u64 {
        test_key()
            .forward(0, EntityId::new(self.entity[&s]))
            .unwrap()
            .raw()
    }

    /// The sources in `view` in map order: by cell of the generator's position, then identity.
    fn map_order(&self, sources: impl IntoIterator<Item = u64>, view: &str) -> Vec<u64> {
        let mut keyed: Vec<((u32, u64), u64)> = sources
            .into_iter()
            .map(|s| {
                let cell = match view {
                    GEO => {
                        let (lon, lat) = geo_position(s);
                        let (x, y) = Projection::WebMercator.forward(lon, lat);
                        tessera_spatial::morton_of(x, y, &unit()).raw()
                    }
                    _ => {
                        let (x, y) = position(s);
                        tessera_spatial::morton_of(x, y, &extent()).raw()
                    }
                };
                ((cell, self.tid(s)), s)
            })
            .collect();
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, s)| s).collect()
    }

    /// The sources in stored order: ascending item number.
    fn stored_order(&self, sources: impl IntoIterator<Item = u64>) -> Vec<u64> {
        let mut keyed: Vec<(u64, u64)> =
            sources.into_iter().map(|s| (self.entity[&s], s)).collect();
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, s)| s).collect()
    }

    fn tids(&self, sources: &[u64]) -> Vec<u64> {
        sources.iter().map(|&s| self.tid(s)).collect()
    }

    /// Ingest one item per source into `s0`, carrying the generator's values for that source.
    fn ingest(&mut self, batch: &str, sources: &[u64]) {
        let entities = ingest_sources(&self.engine, batch, sources);
        for (&s, entity) in sources.iter().zip(entities) {
            self.entity.insert(s, entity.raw());
        }
    }
}

/// Ingest one item per source into `s0`, at the generator's position and with its values and
/// terms, answering the entities allocated.
fn ingest_sources(engine: &Engine, batch: &str, sources: &[u64]) -> Vec<EntityId> {
    let rows: Vec<UnallocatedRow> = sources
        .iter()
        .map(|&s| {
            let (x, y) = position(s);
            let descriptors: Vec<Vec<u8>> = terms_of(s)
                .iter()
                .map(|t| t.to_string().into_bytes())
                .collect();
            UnallocatedRow {
                external_id: Some(s.to_le_bytes().to_vec()),
                view: "s0".to_string(),
                join: None,
                x,
                y,
                scalars: scalars_of(s),
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                scoped: Vec::new(),
            }
        })
        .collect();
    engine
        .accept_ingest(rows, batch.to_string(), [0u8; 32])
        .expect("the ingest is accepted")
}

/// One source's values, positionally against the schema's declared columns.
fn scalars_of(s: u64) -> Vec<WalScalar> {
    let text = |v: Option<String>| v.map_or(WalScalar::Null, WalScalar::Utf8);
    vec![
        band_of(s).map_or(WalScalar::U8(0), |k| WalScalar::Utf8(k.to_string())),
        kind_of(s).map_or(WalScalar::U16(0), |k| WalScalar::Utf8(k.to_string())),
        score_of(s).map_or(WalScalar::Null, WalScalar::I32),
        heat_of(s).map_or(WalScalar::Null, WalScalar::F32),
        text(tag_of(s)),
        text(note_of(s)),
        WalScalar::Utf8(prose_of(s)),
        when_of(s).map_or(WalScalar::Null, WalScalar::TimestampUs),
        flag_of(s).map_or(WalScalar::Null, WalScalar::Bool),
    ]
}

fn both_credential() -> Vec<u8> {
    br#"{"terms": ["0", "1"]}"#.to_vec()
}

fn limits() -> ItemsLimits {
    ItemsLimits {
        max_page_rows: 100_000,
        max_page_bytes: 64 << 20,
        response_bytes: 256 << 20,
        response_time: Duration::from_secs(60),
    }
}

fn request<'a>(view: &'a str, fields: &'a [String]) -> ItemsRequest<'a> {
    ItemsRequest {
        view,
        fields,
        system_fields: &[],
        filter: None,
        keep_unmatched: false,
        count: false,
        order: None,
        page_rows: None,
        pages: None,
        cursor: None,
        idset: None,
        limits: limits(),
        cancel: None,
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[derive(Default)]
struct Collect {
    head: Option<ItemsHead>,
    pages: Vec<(RecordBatch, ItemsPageEnd)>,
}

impl ItemsSink for Collect {
    fn head(&mut self, head: &ItemsHead) -> SinkResult {
        assert!(self.head.is_none(), "one head per response");
        self.head = Some(head.clone());
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, end: &ItemsPageEnd) -> SinkResult {
        assert!(self.head.is_some(), "the head precedes every page");
        self.pages.push((batch.clone(), end.clone()));
        Ok(())
    }
}

/// One response, collected. Every response that is not refused carries a head, and no refusal
/// sends one.
fn respond(
    engine: &Engine,
    session: &Session,
    req: ItemsRequest<'_>,
) -> Result<(Collect, ItemsTrailer), EngineError> {
    let mut sink = Collect::default();
    match engine.items_stream(session, req, &mut sink) {
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

/// A whole read: every response from no cursor until one ends with none.
struct Read {
    heads: Vec<ItemsHead>,
    pages: Vec<(RecordBatch, ItemsPageEnd)>,
    trailers: Vec<ItemsTrailer>,
}

impl Read {
    fn ids(&self) -> Vec<u64> {
        self.pages.iter().flat_map(|(batch, _)| ids_of(batch)).collect()
    }
}

fn read_all(engine: &Engine, session: &Session, base: &ItemsRequest<'_>) -> Read {
    let mut read = Read {
        heads: Vec::new(),
        pages: Vec::new(),
        trailers: Vec::new(),
    };
    let mut cursor: Option<String> = None;
    for _ in 0..100_000 {
        let mut req = base.clone();
        req.cursor = cursor.as_deref();
        let (sink, trailer) = respond(engine, session, req).expect("a response");
        read.heads.push(sink.head.unwrap());
        read.pages.extend(sink.pages);
        cursor = trailer.next.clone();
        read.trailers.push(trailer);
        if cursor.is_none() {
            return read;
        }
    }
    panic!("the read never ended");
}

fn ids_of(batch: &RecordBatch) -> Vec<u64> {
    batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .expect("tessera_id is the first column")
        .values()
        .to_vec()
}

fn col<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column {name} is not the expected type"))
}

fn assert_each_once(ids: &[u64]) {
    let distinct: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(distinct.len(), ids.len(), "a row was returned twice");
}

fn range(lo: i64, hi: i64) -> FilterOperand {
    FilterOperand::Range {
        lo: Some(Endpoint {
            value: Scalar::Int(lo.into()),
            inclusive: true,
        }),
        hi: Some(Endpoint {
            value: Scalar::Int(hi.into()),
            inclusive: true,
        }),
    }
}

fn leaf(column: &str, operand: FilterOperand) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    }
}

fn lasso() -> FilterExpr {
    let shape = ShapeF64::Polygon(vec![vec![vec![
        (120.0, 80.0),
        (640.0, 140.0),
        (880.0, 560.0),
        (500.0, 930.0),
        (90.0, 610.0),
        (330.0, 420.0),
    ]]])
    .canonical(Space::View, &extent())
    .expect("a well-formed shape")
    .0;
    FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)))
}

/// Items the viewport counts as visible and matching, over the whole map.
fn viewport_counts(
    engine: &Engine,
    session: &Session,
    view: &str,
    filter: Option<FilterExpr>,
) -> (u64, u64) {
    let mut req = ViewportRequest::new(view, 0, WHOLE_MAP, 10);
    if let Some(filter) = filter {
        req = req.filter(filter);
    }
    let out = engine.viewport(session, req).expect("a viewport");
    (
        out.tiles.iter().map(|t| t.visible).sum(),
        out.tiles.iter().map(|t| t.matched).sum(),
    )
}

// ---------------------------------------------------------------------------------------------
// Order and completeness
// ---------------------------------------------------------------------------------------------

/// **Both orders return the same rows, each once, in their own order**, for every kind of leaf
/// and several page sizes, across responses that each carry a few pages.
#[test]
fn both_orders_return_the_same_rows_once_for_every_leaf_and_page_size() {
    let fx = Fx::new();
    let layer = "clusters/a";
    let members: Range<u64> = 100..400;
    fx.engine
        .register_layer(LayerDeclaration {
            scope: Default::default(),
            name: layer.into(),
            title: None,
            views: vec!["s0".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
            require_member_visibility: Some(ExistenceCriterion::Count(1)),
            hierarchy: Hierarchy {
                kind: HierarchyKind::Flat,
                prune_children: false,
            },
            content: ContentDeclaration {
                computed: vec!["centroid".into()],
                supplied: Vec::new(),
            },
            depends_on: Vec::new(),
            levels: Vec::new(),
            layout: None,
            shape: None,
        })
        .unwrap();
    fx.engine
        .publish_artifacts(
            layer.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                members.clone().map(|s| EntityId::new(fx.entity[&s])).collect::<Vec<_>>(),
            )],
        )
        .unwrap();
    tick(&fx.engine);
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let artifact: TesseraId = fx
        .engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, 10).layers(LayerSelection::All),
        )
        .unwrap()
        .artifacts[0]
        .tessera_id;

    let all: Vec<u64> = (0..N).collect();
    // Each case's name, filter, and the sources it admits where the generator can say.
    type Case<'a> = (&'a str, Option<FilterExpr>, Option<Vec<u64>>);
    let cases: Vec<Case<'_>> = vec![
        ("no filter", None, Some(all.clone())),
        (
            "category",
            Some(leaf("band", FilterOperand::Equals(AttrLocalId::new(band_code("mid"))))),
            Some(all.iter().copied().filter(|&s| band_of(s) == Some("mid")).collect()),
        ),
        (
            "numeric range",
            Some(leaf("score", range(300, 900))),
            Some(
                all.iter()
                    .copied()
                    .filter(|&s| score_of(s).is_some_and(|v| (300..=900).contains(&v)))
                    .collect(),
            ),
        ),
        (
            "rendered-only range",
            Some(FilterExpr::Leaf {
                column: "heat".into(),
                operand: FilterOperand::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Float(2.0),
                        inclusive: true,
                    }),
                    hi: None,
                },
            }),
            Some(all.iter().copied().filter(|&s| heat_of(s).is_some_and(|v| v >= 2.0)).collect()),
        ),
        (
            "text match",
            Some(leaf(
                "prose",
                FilterOperand::Match {
                    query: "group1".into(),
                    minimum: None,
                },
            )),
            Some(all.iter().copied().filter(|&s| s % 3 == 1).collect()),
        ),
        (
            "member_of",
            Some(FilterExpr::MemberOf(MemberOfLeaf {
                layer: layer.into(),
                artifact,
            })),
            Some(members.clone().collect()),
        ),
        ("region past the cell budget", Some(lasso()), None),
    ];
    fx.engine.set_max_region_cells(16);
    for (name, filter, expected) in cases {
        let (_, matched) = viewport_counts(&fx.engine, &session, "s0", filter.clone());
        for page_rows in [7u32, 250, 5000] {
            let fields: Vec<String> = Vec::new();
            let mut base = request("s0", &fields);
            base.filter = filter.clone();
            base.page_rows = Some(page_rows);
            base.pages = Some(3);
            base.order = Some(RecordsOrder::Map);
            let map = read_all(&fx.engine, &session, &base).ids();
            base.order = Some(RecordsOrder::Stored);
            let stored_read = read_all(&fx.engine, &session, &base);
            let stored = stored_read.ids();
            assert_each_once(&map);
            assert_each_once(&stored);
            assert_eq!(
                map.iter().collect::<BTreeSet<_>>(),
                stored.iter().collect::<BTreeSet<_>>(),
                "{name} at {page_rows}: the two orders returned different rows"
            );
            assert_eq!(
                map.len() as u64,
                matched,
                "{name} at {page_rows}: the rows are not the viewport's matched count"
            );
            if let Some(expected) = &expected {
                assert_eq!(
                    map,
                    fx.tids(&fx.map_order(expected.iter().copied(), "s0")),
                    "{name} at {page_rows}: map order"
                );
                assert_eq!(
                    stored,
                    fx.tids(&fx.stored_order(expected.iter().copied())),
                    "{name} at {page_rows}: stored order"
                );
            }
            if name.starts_with("region") {
                assert!(
                    matches!(stored_read.heads[0].region, Some(RegionVerdict::Cover { .. })),
                    "past the budget the region is a cover, and the head says so"
                );
            }
        }
    }
}

/// **A view of several segments is read whole**, in both orders, with every row once and in
/// order, including rows ingested and flushed after the build.
#[test]
fn a_view_of_several_segments_is_read_whole_in_both_orders() {
    let mut fx = Fx::new();
    fx.ingest("b1", &(N..N + 60).collect::<Vec<_>>());
    flush(&fx.engine);
    fx.ingest("b2", &(N + 60..N + 150).collect::<Vec<_>>());
    flush(&fx.engine);
    let segments = fx.engine.generation().bundle.partitions["default"].views["s0"]
        .segments
        .len();
    assert!(segments >= 3, "the view holds {segments} segments");

    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let everything: Vec<u64> = (0..N + 150).collect();
    let fields = names(&["band", "note"]);
    let mut base = request("s0", &fields);
    base.page_rows = Some(333);
    base.pages = Some(2);
    base.order = Some(RecordsOrder::Map);
    assert_eq!(
        read_all(&fx.engine, &session, &base).ids(),
        fx.tids(&fx.map_order(everything.iter().copied(), "s0"))
    );
    base.order = Some(RecordsOrder::Stored);
    assert_eq!(
        read_all(&fx.engine, &session, &base).ids(),
        fx.tids(&fx.stored_order(everything.iter().copied()))
    );

    let mut filtered = request("s0", &fields);
    filtered.filter = Some(leaf("score", range(1000, 2100)));
    let expected: Vec<u64> = everything
        .iter()
        .copied()
        .filter(|&s| score_of(s).is_some_and(|v| (1000..=2100).contains(&v)))
        .collect();
    filtered.order = Some(RecordsOrder::Map);
    assert_eq!(
        read_all(&fx.engine, &session, &filtered).ids(),
        fx.tids(&fx.map_order(expected.iter().copied(), "s0"))
    );
    filtered.order = Some(RecordsOrder::Stored);
    assert_eq!(
        read_all(&fx.engine, &session, &filtered).ids(),
        fx.tids(&fx.stored_order(expected.iter().copied()))
    );
}

/// Continue a read from `cursor` to its end.
fn continue_read(
    engine: &Engine,
    session: &Session,
    base: &ItemsRequest<'_>,
    cursor: String,
) -> Vec<u64> {
    let mut cursor = Some(cursor);
    let mut ids = Vec::new();
    while let Some(held) = cursor.take() {
        let mut req = base.clone();
        req.cursor = Some(&held);
        let (sink, trailer) = respond(engine, session, req).expect("a response");
        ids.extend(sink.pages.iter().flat_map(|(batch, _)| ids_of(batch)));
        cursor = trailer.next;
    }
    ids
}

/// **A flush and a merge landing mid-read lose nothing behind the cursor and repeat nothing**,
/// and a filter stays exact across both, though each renumbers the rows it was evaluated over.
#[test]
fn a_flush_and_a_merge_landing_mid_read_lose_and_repeat_nothing() {
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        for filtered in [false, true] {
            read_across_a_flush_and_a_merge(order, filtered);
        }
    }
}

fn read_across_a_flush_and_a_merge(order: RecordsOrder, filtered: bool) {
    let mut fx = Fx::new();
    fx.engine.set_merge_for_test(false);
    for batch in 0..4u64 {
        let start = N + batch * 40;
        fx.ingest(&format!("pre-{batch}"), &(start..start + 40).collect::<Vec<_>>());
        flush(&fx.engine);
    }
    let before: Vec<u64> = (0..N + 160).collect();
    let later: Vec<u64> = (N + 160..N + 260).collect();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["band"]);
    let matches = |s: &u64| matches!(band_of(*s), Some("low") | Some("high"));
    let mut base = request("s0", &fields);
    base.order = Some(order);
    base.page_rows = Some(97);
    base.pages = Some(2);
    base.filter = filtered.then(|| {
        leaf(
            "band",
            FilterOperand::In(vec![AttrLocalId::new(1), AttrLocalId::new(3)]),
        )
    });
    let (sink, trailer) = respond(&fx.engine, &session, base.clone()).unwrap();
    let mut ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();

    // A merge of the four flushed segments, and a flush of new rows, between two pages.
    fx.engine.set_merge_for_test(true);
    let merges = fx.engine.write_executor_stats().merges;
    fx.engine.request_flush();
    wait_until("the merge to publish", Duration::from_secs(60), || {
        fx.engine.write_executor_stats().merges > merges
    });
    fx.ingest("later", &later);
    flush(&fx.engine);

    let cut = *ids.last().expect("the first response returned rows");
    ids.extend(continue_read(&fx.engine, &session, &base, trailer.next.unwrap()));
    assert_each_once(&ids);

    // Every row present before the read that the filter admits, and of the rows inserted during
    // it exactly those the filter admits that sort after the last row returned before them.
    let mut all = before.clone();
    all.extend(&later);
    let in_order: Vec<u64> = match order {
        RecordsOrder::Map => fx.map_order(all.iter().copied(), "s0"),
        RecordsOrder::Stored => fx.stored_order(all.iter().copied()),
    };
    let rank: BTreeMap<u64, usize> = in_order
        .iter()
        .enumerate()
        .map(|(i, &s)| (fx.tid(s), i))
        .collect();
    let inserted: BTreeSet<u64> = later.iter().copied().collect();
    let expected: Vec<u64> = in_order
        .iter()
        .copied()
        .filter(|s| !filtered || matches(s))
        .filter(|s| !inserted.contains(s) || rank[&fx.tid(*s)] > rank[&cut])
        .map(|s| fx.tid(s))
        .collect();
    if order == RecordsOrder::Map {
        assert!(
            later.iter().any(|&s| rank[&fx.tid(s)] < rank[&cut]),
            "the case is vacuous unless a row is inserted behind the cursor"
        );
    }
    assert_eq!(ids, expected, "{order:?} filtered={filtered}");
}

/// **A suppression and a deletion accepted between two responses of a read apply from the next
/// response.**
#[test]
fn a_suppression_and_a_deletion_accepted_between_responses_are_absent_from_the_next_response() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields: Vec<String> = Vec::new();
    let mut hidden: Vec<u64> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let everything: Vec<u64> = match order {
            RecordsOrder::Map => fx.map_order(0..N, "s0"),
            RecordsOrder::Stored => fx.stored_order(0..N),
        };
        let mut base = request("s0", &fields);
        base.order = Some(order);
        base.page_rows = Some(100);
        base.pages = Some(1);
        let (sink, trailer) = respond(&fx.engine, &session, base.clone()).unwrap();
        let mut ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();
        let mut ahead = everything.iter().rev().filter(|s| !hidden.contains(*s));
        let (suppressed, deleted) = (*ahead.next().unwrap(), *ahead.next().unwrap());
        hidden.extend([suppressed, deleted]);
        fx.engine
            .accept_change(EntityId::new(fx.entity[&suppressed]), ChangeOp::Suppress)
            .unwrap();
        fx.engine
            .accept_change(EntityId::new(fx.entity[&deleted]), ChangeOp::Delete)
            .unwrap();
        ids.extend(continue_read(&fx.engine, &session, &base, trailer.next.unwrap()));
        assert_each_once(&ids);
        let returned: HashSet<u64> = ids.iter().copied().collect();
        assert!(
            !returned.contains(&fx.tid(suppressed)),
            "{order:?}: the suppressed item was served"
        );
        assert!(!returned.contains(&fx.tid(deleted)), "{order:?}: the deleted item was served");
    }
    // Both reads together hid four items, and the viewport agrees on what is left.
    let (visible, _) = viewport_counts(&fx.engine, &session, "s0", None);
    assert_eq!(visible, N - 4);
}

/// **A viewer who sees a subset reads that subset and nothing else**, and the head's counts are
/// the viewport's.
#[test]
fn a_narrower_viewer_reads_only_what_they_see_and_counts_as_the_viewport_does() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&subset_credential()).unwrap();
    let visible: Vec<u64> = (0..N).filter(|&s| subset_sees(s)).collect();
    let fields = names(&["score", "note", "tag"]);
    let filter = leaf("band", FilterOperand::Equals(AttrLocalId::new(band_code("low"))));
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut base = request("s0", &fields);
        base.order = Some(order);
        base.page_rows = Some(64);
        let read = read_all(&fx.engine, &session, &base);
        let expected = match order {
            RecordsOrder::Map => fx.map_order(visible.iter().copied(), "s0"),
            RecordsOrder::Stored => fx.stored_order(visible.iter().copied()),
        };
        assert_eq!(read.ids(), fx.tids(&expected), "{order:?}");

        base.filter = Some(filter.clone());
        base.count = true;
        base.pages = Some(1);
        let (sink, _) = respond(&fx.engine, &session, base).unwrap();
        let counts = sink.head.unwrap().counts.expect("counts asked for");
        let (viewport_visible, viewport_matched) =
            viewport_counts(&fx.engine, &session, "s0", Some(filter.clone()));
        assert_eq!(counts.visible, viewport_visible);
        assert_eq!(counts.matched, viewport_matched);
        assert_eq!(
            counts.matched,
            visible.iter().filter(|&&s| band_of(s) == Some("low")).count() as u64
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------------------------

/// **Every cursor presented outside the read it was issued for is refused alike**: altered, from
/// a session with another credential, for another view, for a view dropped and created again, or
/// for another order. Another idset is the item route's stale-idset refusal.
#[test]
fn every_foreign_cursor_is_refused_alike() {
    let fx = Fx::new();
    let fields: Vec<String> = Vec::new();
    let session = fx.engine.authorise(&both_credential()).unwrap();
    let first_cursor = |view: &str, session: &Session| -> String {
        let mut req = request(view, &fields);
        req.page_rows = Some(10);
        req.pages = Some(1);
        req.order = Some(RecordsOrder::Map);
        respond(&fx.engine, session, req).unwrap().1.next.unwrap()
    };
    let cursor = first_cursor("s0", &session);
    let q2_cursor = first_cursor(Q2, &session);
    let present = |view: &str, session: &Session, cursor: &str, order: Option<RecordsOrder>| {
        let mut req = request(view, &fields);
        req.cursor = Some(cursor);
        req.order = order;
        respond(&fx.engine, session, req).map(|_| ())
    };
    assert!(present("s0", &session, &cursor, None).is_ok(), "the cursor resumes its own read");

    let mut refusals: Vec<EngineError> = Vec::new();
    // Altered: one character of the sealed text changed.
    let mut altered = cursor.clone().into_bytes();
    let at = altered.len() / 2;
    altered[at] = if altered[at] == b'A' { b'B' } else { b'A' };
    let altered = String::from_utf8(altered).unwrap();
    refusals.push(present("s0", &session, &altered, None).unwrap_err());
    // Another credential.
    let other = fx.engine.authorise(&full_coverage_credential()).unwrap();
    refusals.push(present("s0", &other, &cursor, None).unwrap_err());
    // Another view.
    refusals.push(present(Q1, &session, &cursor, None).unwrap_err());
    // Another order.
    refusals.push(present("s0", &session, &cursor, Some(RecordsOrder::Stored)).unwrap_err());
    // A view dropped and created again under the same key.
    fx.engine
        .drop_view("quarter".into(), "q2".into(), false)
        .expect("the view drops");
    fx.engine
        .create_view("quarter".into(), "q2".into(), None, Default::default())
        .expect("the key is created again");
    tick(&fx.engine);
    let later = fx.engine.authorise(&both_credential()).unwrap();
    refusals.push(present(Q2, &later, &q2_cursor, None).unwrap_err());

    for refusal in &refusals {
        assert!(
            matches!(refusal, EngineError::CursorRefused),
            "a foreign cursor was answered {refusal:?}"
        );
    }

    let mut stale = request("s0", &fields);
    stale.idset = Some(IDSET + 1);
    assert!(matches!(
        respond(&fx.engine, &session, stale).map(|_| ()),
        Err(EngineError::StaleIdSet)
    ));
}

/// **A sparse filter under no time budget still finishes**: every response scans at least one
/// stretch, and the cursor it ends with is past it though no row was found.
#[test]
fn a_sparse_filter_under_no_time_budget_still_advances_and_completes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let n = 30_000;
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        n,
    );
    let engine = engine_at(tmp.path(), &root, 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    // A small box around `(37, 53)`, where every item `e` with `e % 1000 == 1` is placed by the
    // fixture's `((e * 37) % 1000, (e * 53) % 1000)`, and no other.
    let shape = ShapeF64::Bbox {
        min_x: 36.0,
        min_y: 52.0,
        max_x: 38.0,
        max_y: 54.0,
    }
    .canonical(Space::View, &extent())
    .unwrap()
    .0;
    let filter = FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)));
    let (_, matched) = viewport_counts(&engine, &session, "s0", Some(filter.clone()));
    assert_eq!(matched, n / 1000, "the box holds one item per thousand");
    let fields: Vec<String> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut base = request("s0", &fields);
        base.filter = Some(filter.clone());
        base.order = Some(order);
        base.page_rows = Some(1);
        base.limits.response_time = Duration::ZERO;
        let read = read_all(&engine, &session, &base);
        assert!(
            read.trailers.len() > 2,
            "{order:?}: the budget ended responses before the read did"
        );
        assert!(
            read.trailers
                .iter()
                .any(|t| t.rows == 0 && t.ended_by == ResponseEndedBy::BudgetTime),
            "{order:?}: a response that found nothing still ended with a cursor"
        );
        let ids = read.ids();
        assert_each_once(&ids);
        assert_eq!(ids.len() as u64, matched, "{order:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------------------------

/// Read `fields` over the whole of `view` in map order, as one batch per page.
fn pages_of(
    fx: &Fx,
    session: &Session,
    view: &str,
    fields: &[String],
    system: &[String],
) -> Vec<RecordBatch> {
    let mut base = request(view, fields);
    base.system_fields = system;
    base.order = Some(RecordsOrder::Map);
    base.page_rows = Some(500);
    read_all(&fx.engine, session, &base)
        .pages
        .into_iter()
        .map(|(batch, _)| batch)
        .collect()
}

/// Source id by `tessera_id`, over the built items.
fn sources_by_tid(fx: &Fx) -> BTreeMap<u64, u64> {
    fx.entity.keys().map(|&s| (fx.tid(s), s)).collect()
}

/// **An absent value is a null in every home**: a rendered number and bool by the presence
/// bitmap, a category by code 0, a value column by its presence, a record field by its absence
/// from the record. Every present value is the generator's, at the declared type.
#[test]
fn absent_values_are_nulls_in_every_home() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["band", "kind", "score", "heat", "tag", "note", "prose", "when", "flag"]);
    let by_tid = sources_by_tid(&fx);
    let mut seen = 0;
    for batch in pages_of(&fx, &session, "s0", &fields, &[]) {
        let schema = batch.schema();
        let expected_names: Vec<&str> = std::iter::once("tessera_id")
            .chain(fields.iter().map(String::as_str))
            .collect();
        let got_names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(got_names, expected_names, "tessera_id, then the fields in the order named");
        assert_eq!(
            schema.field_with_name("when").unwrap().data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        let band = col::<DictionaryArray<Int32Type>>(&batch, "band");
        let band_keys = band.values().as_any().downcast_ref::<StringArray>().unwrap();
        let kind = col::<DictionaryArray<Int32Type>>(&batch, "kind");
        let kind_keys = kind.values().as_any().downcast_ref::<StringArray>().unwrap();
        let score = col::<Int32Array>(&batch, "score");
        let heat = col::<Float32Array>(&batch, "heat");
        let tag = col::<StringArray>(&batch, "tag");
        let note = col::<StringArray>(&batch, "note");
        let prose = col::<StringArray>(&batch, "prose");
        let when = col::<TimestampMicrosecondArray>(&batch, "when");
        let flag = col::<BooleanArray>(&batch, "flag");
        for (i, tid) in ids_of(&batch).into_iter().enumerate() {
            let s = by_tid[&tid];
            let key = |keys: &StringArray, codes: &DictionaryArray<Int32Type>| {
                codes.key(i).map(|k| keys.value(k).to_string())
            };
            assert_eq!(key(band_keys, band), band_of(s).map(String::from), "band of {s}");
            assert_eq!(key(kind_keys, kind), kind_of(s).map(String::from), "kind of {s}");
            let opt = |a: &dyn Array| a.is_valid(i);
            assert_eq!(opt(score).then(|| score.value(i)), score_of(s), "score of {s}");
            assert_eq!(opt(heat).then(|| heat.value(i)), heat_of(s), "heat of {s}");
            assert_eq!(opt(tag).then(|| tag.value(i).to_string()), tag_of(s), "tag of {s}");
            assert_eq!(opt(note).then(|| note.value(i).to_string()), note_of(s), "note of {s}");
            assert_eq!(prose.value(i), prose_of(s));
            assert_eq!(opt(when).then(|| when.value(i)), when_of(s), "when of {s}");
            assert_eq!(opt(flag).then(|| flag.value(i)), flag_of(s), "flag of {s}");
            seen += 1;
        }
    }
    assert_eq!(seen, N);
}

/// **A page's category dictionary holds the keys its rows carry and no other.**
#[test]
fn a_pages_category_dictionary_holds_only_its_own_keys() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["band"]);
    let mut base = request("s0", &fields);
    base.page_rows = Some(2);
    base.pages = Some(40);
    let (sink, _) = respond(&fx.engine, &session, base).unwrap();
    assert_eq!(sink.pages.len(), 40);
    let mut sizes = BTreeSet::new();
    for (batch, _) in &sink.pages {
        let band = col::<DictionaryArray<Int32Type>>(batch, "band");
        let dictionary: BTreeSet<String> = band
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .iter()
            .map(|k| k.unwrap().to_string())
            .collect();
        let carried: BTreeSet<String> = (0..band.len())
            .filter_map(|i| band.key(i))
            .map(|k| {
                band.values()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(k)
                    .to_string()
            })
            .collect();
        assert_eq!(dictionary, carried);
        sizes.insert(dictionary.len());
    }
    assert!(sizes.len() > 1, "pages carried different key sets: {sizes:?}");
}

/// **A position comes back within one step of the grid** of the coordinate it was placed from:
/// the view's own coordinates on a plain view, longitude and latitude on a geographic one.
#[test]
fn positions_come_back_within_one_grid_step() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let by_tid = sources_by_tid(&fx);
    let system = names(&["position", "external_id"]);
    let fields: Vec<String> = Vec::new();
    let step = 1000.0 / 4_294_967_296.0;
    let mut checked = 0;
    for batch in pages_of(&fx, &session, "s0", &fields, &system) {
        let x = col::<Float64Array>(&batch, "tessera:x");
        let y = col::<Float64Array>(&batch, "tessera:y");
        let external = col::<BinaryArray>(&batch, "tessera:external_id");
        for (i, tid) in ids_of(&batch).into_iter().enumerate() {
            let s = by_tid[&tid];
            let (px, py) = position(s);
            assert!((x.value(i) - px).abs() <= step, "x of {s}: {} against {px}", x.value(i));
            assert!((y.value(i) - py).abs() <= step, "y of {s}: {} against {py}", y.value(i));
            assert_eq!(external.value(i), s.to_le_bytes());
            checked += 1;
        }
    }
    assert_eq!(checked, N);

    let unit_step = 1.0 / 4_294_967_296.0;
    let mut checked = 0;
    for batch in pages_of(&fx, &session, GEO, &fields, &system) {
        let lon = col::<Float64Array>(&batch, "tessera:x");
        let lat = col::<Float64Array>(&batch, "tessera:y");
        for (i, tid) in ids_of(&batch).into_iter().enumerate() {
            let s = by_tid[&tid];
            let (want_lon, want_lat) = geo_position(s);
            assert!((lon.value(i) - want_lon).abs() <= 360.0 * unit_step * 1.01, "lon of {s}");
            let (_, got_y) = Projection::WebMercator.forward(lon.value(i), lat.value(i));
            let (_, want_y) = Projection::WebMercator.forward(want_lon, want_lat);
            assert!((got_y - want_y).abs() <= unit_step * 1.01, "lat of {s}");
            checked += 1;
        }
    }
    assert_eq!(checked, N / 2, "the geographic view holds the even items");
}

/// **A group-scoped field resolves as a filter leaf on it does**: under a view of its group it is
/// that view's column; under another view it must be pinned; a pin naming no view is an unknown
/// view; a group the viewer cannot reach makes it an unknown field; a text family has no value.
#[test]
fn a_group_scoped_field_resolves_as_a_filter_leaf_does() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&both_credential()).unwrap();
    let by_tid = sources_by_tid(&fx);
    let sentiment_of = |batches: Vec<RecordBatch>, name: &str| -> BTreeMap<u64, Option<f32>> {
        let mut out = BTreeMap::new();
        for batch in batches {
            let values = col::<Float32Array>(&batch, name);
            for (i, tid) in ids_of(&batch).into_iter().enumerate() {
                out.insert(by_tid[&tid], values.is_valid(i).then(|| values.value(i)));
            }
        }
        out
    };

    let bare = names(&["sentiment"]);
    let in_q1 = sentiment_of(pages_of(&fx, &session, Q1, &bare, &[]), "sentiment");
    assert_eq!(in_q1.len(), QUARTERS[0].1.clone().count());
    for (s, value) in &in_q1 {
        assert_eq!(*value, sentiment(0, *s), "q1's own value for {s}");
    }

    let pinned = names(&["sentiment@q2"]);
    let on_world = sentiment_of(pages_of(&fx, &session, "s0", &pinned, &[]), "sentiment@q2");
    assert_eq!(on_world.len(), N as usize);
    for (s, value) in &on_world {
        let expected = QUARTERS[1].1.contains(s).then(|| sentiment(1, *s)).flatten();
        assert_eq!(*value, expected, "q2's value for {s}, read on the world view");
    }

    let refused = |view: &str, fields: &[String], session: &Session| {
        respond(&fx.engine, session, request(view, fields)).map(|_| ()).unwrap_err()
    };
    assert!(matches!(
        refused("s0", &bare, &session),
        EngineError::RecordsRefused(RecordsRefused::Unpinned { .. })
    ));
    assert!(matches!(
        refused("s0", &names(&["sentiment@q9"]), &session),
        EngineError::UnknownView(_)
    ));
    assert!(matches!(
        refused(Q1, &names(&["blurb"]), &session),
        EngineError::RecordsRefused(RecordsRefused::ScopedText(_))
    ));
    assert!(matches!(
        refused("s0", &names(&["band@q1"]), &session),
        EngineError::RecordsRefused(RecordsRefused::PinOnUnscoped(_))
    ));

    let hush_field = names(&["hush@k"]);
    let mut hushed = 0;
    for batch in pages_of(&fx, &session, "s0", &hush_field, &[]) {
        let values = col::<Int32Array>(&batch, "hush@k");
        for (i, tid) in ids_of(&batch).into_iter().enumerate() {
            let s = by_tid[&tid];
            let expected = SECRET_MEMBERS.contains(&s).then(|| hush(s));
            assert_eq!(values.is_valid(i).then(|| values.value(i)), expected, "hush of {s}");
            hushed += 1;
        }
    }
    assert_eq!(hushed, N, "a viewer reaching the group reads its field on every row");
    let outsider = fx.engine.authorise(&full_coverage_credential()).unwrap();
    assert!(matches!(
        refused("s0", &hush_field, &outsider),
        EngineError::RecordsRefused(RecordsRefused::UnknownField(_))
    ));
    assert!(matches!(
        refused("s0", &names(&["hush"]), &outsider),
        EngineError::RecordsRefused(RecordsRefused::UnknownField(_))
    ));
    assert!(matches!(
        refused(SECRET, &Vec::new(), &outsider),
        EngineError::UnknownView(_)
    ));
}

/// **An item joined into a second view is returned there before its own ingest is flushed, with
/// its record fields null**, as its item card shows them.
#[test]
fn a_joined_item_whose_own_record_is_unflushed_has_null_record_fields() {
    use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};

    let tmp = tempfile::TempDir::new().unwrap();
    let root = build_bundle(tmp.path());
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        tessera_engine::EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            ..config()
        },
    )
    .unwrap();
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(64, Arc::clone(&faults))
        .unwrap();
    let ingest_into = |batch: &str, view: &str, external_id: &str, x: f64, y: f64, scalars| {
        let descriptors = vec![b"0".to_vec(), b"1".to_vec()];
        let row = UnallocatedRow {
            external_id: Some(external_id.as_bytes().to_vec()),
            view: view.to_string(),
            join: None,
            x,
            y,
            scalars,
            terms: engine.resolve_terms(&descriptors),
            descriptors,
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], batch.to_string(), [0u8; 32])
            .expect("the ingest is accepted")[0]
    };
    // "anchor" holds the lower entity id, so the geographic view's plan is dispatched first.
    ingest_into("b-anchor", GEO, "anchor", 0.25, 0.25, scalars_of(3));
    let joiner = ingest_into("b-own", "s0", "joiner", 5.0, 5.0, scalars_of(3));
    // A joining row omits the entity's attributes, which are one value per entity.
    ingest_into("b-join", GEO, "joiner", 0.75, 0.75, vec![WalScalar::Null; 9]);
    faults.arm_pause_after(PauseSite::BeforeManifestPublish, PauseAction::Stall, 1);
    engine.request_flush();
    faults.await_arrivals(PauseSite::BeforeManifestPublish, 2, Duration::from_secs(30));

    let generation = engine.generation();
    let views = &generation.bundle.partitions["default"].views;
    assert!(views[GEO].row_space.row_of(joiner).is_some(), "the join row published");
    assert!(views["s0"].row_space.row_of(joiner).is_none(), "its own row is still buffered");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["note", "tag"]);
    let mark = engine.tessera_id_of(joiner).unwrap().raw();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request(GEO, &fields);
        req.order = Some(order);
        let (sink, _) = respond(&engine, &session, req).unwrap();
        let mut found = false;
        for (batch, _) in &sink.pages {
            let note = col::<StringArray>(batch, "note");
            let tag = col::<StringArray>(batch, "tag");
            for (i, tid) in ids_of(batch).into_iter().enumerate() {
                if tid == mark {
                    found = true;
                    assert!(note.is_null(i), "{order:?}: the unflushed record's field is null");
                    assert!(tag.is_null(i), "{order:?}: and so is its value column's");
                }
            }
        }
        assert!(found, "{order:?}: the joined item is returned in the view it has a row in");
    }
    faults.release();
}

/// **`keep_unmatched` returns every visible row with its matched bit, and `count` heads the
/// response with the visible and matching counts.**
#[test]
fn keep_unmatched_marks_every_visible_row_and_count_heads_the_response() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let by_tid = sources_by_tid(&fx);
    let fields = names(&["score"]);
    let filter = leaf("score", range(100, 700));
    let matches = |s: u64| score_of(s).is_some_and(|v| (100..=700).contains(&v));
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut base = request("s0", &fields);
        base.order = Some(order);
        base.filter = Some(filter.clone());
        base.keep_unmatched = true;
        base.count = true;
        base.page_rows = Some(300);
        base.pages = Some(1);
        let (sink, trailer) = respond(&fx.engine, &session, base.clone()).unwrap();
        let counts = sink.head.as_ref().unwrap().counts.unwrap();
        assert_eq!(counts.visible, N);
        assert_eq!(counts.matched, (0..N).filter(|&s| matches(s)).count() as u64);
        let mut rows = 0;
        let mut pages = sink.pages;
        base.count = false;
        base.pages = None;
        let mut rest = base.clone();
        let next = trailer.next.unwrap();
        rest.cursor = Some(&next);
        pages.extend(respond(&fx.engine, &session, rest).unwrap().0.pages);
        for (batch, _) in &pages {
            let last = batch.schema().fields().last().unwrap().name().clone();
            assert_eq!(last, "tessera:matched", "the matched column is last");
            let matched = col::<BooleanArray>(batch, "tessera:matched");
            for (i, tid) in ids_of(batch).into_iter().enumerate() {
                assert_eq!(matched.value(i), matches(by_tid[&tid]));
                rows += 1;
            }
        }
        assert_eq!(rows, N, "{order:?}: every visible row, matched or not");
    }

    let mut unfiltered = request("s0", &fields);
    unfiltered.keep_unmatched = true;
    unfiltered.count = true;
    let (sink, _) = respond(&fx.engine, &session, unfiltered).unwrap();
    let counts = sink.head.unwrap().counts.unwrap();
    assert_eq!((counts.visible, counts.matched), (N, N));
    for (batch, _) in &sink.pages {
        assert!(col::<BooleanArray>(batch, "tessera:matched").iter().all(|b| b == Some(true)));
    }
}

/// **A row larger than the byte ceiling is sent alone**, and a page never takes a second row past
/// the ceiling.
#[test]
fn a_row_larger_than_the_byte_ceiling_is_sent_alone() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["note", "prose"]);
    let mut tiny = request("s0", &fields);
    tiny.limits.max_page_bytes = 1;
    tiny.pages = Some(5);
    let (sink, _) = respond(&fx.engine, &session, tiny).unwrap();
    assert_eq!(sink.pages.len(), 5);
    for (batch, end) in &sink.pages {
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(end.ended_by, PageEndedBy::Bytes);
    }

    let mut ceiling = request("s0", &fields);
    ceiling.limits.max_page_bytes = 4096;
    ceiling.order = Some(RecordsOrder::Stored);
    let read = read_all(&fx.engine, &session, &ceiling);
    assert_each_once(&read.ids());
    assert_eq!(read.ids().len() as u64, N);
    let (last, whole) = read.pages.split_last().unwrap();
    for (batch, end) in whole {
        assert!(batch.num_rows() > 1, "a page of small rows holds several");
        assert!(end.bytes <= 4096, "{} rows came to {} bytes", batch.num_rows(), end.bytes);
    }
    assert!(last.1.bytes <= 4096);
    assert_eq!(read.pages.last().unwrap().1.ended_by, PageEndedBy::End);
}

/// **The engine chooses stored order where a named field is read only from the record store**,
/// and map order otherwise; every malformed request is refused before its head.
#[test]
fn the_order_follows_the_fields_and_malformed_requests_are_refused() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let order_of = |fields: &[&str]| {
        let fields = names(fields);
        let mut req = request("s0", &fields);
        req.pages = Some(1);
        respond(&fx.engine, &session, req).unwrap().0.head.unwrap().order
    };
    assert_eq!(order_of(&["band", "score", "tag", "kind"]), RecordsOrder::Map);
    assert_eq!(order_of(&["band", "note"]), RecordsOrder::Stored);
    assert_eq!(order_of(&["prose"]), RecordsOrder::Stored);

    let refusal =
        |req: ItemsRequest<'_>| respond(&fx.engine, &session, req).map(|_| ()).unwrap_err();
    let fields = names(&["band", "band"]);
    assert!(matches!(
        refusal(request("s0", &fields)),
        EngineError::RecordsRefused(RecordsRefused::RepeatedField(_))
    ));
    let fields = names(&["nothing"]);
    assert!(matches!(
        refusal(request("s0", &fields)),
        EngineError::RecordsRefused(RecordsRefused::UnknownField(_))
    ));
    let none: Vec<String> = Vec::new();
    let system = names(&["colour"]);
    let mut req = request("s0", &none);
    req.system_fields = &system;
    assert!(matches!(
        refusal(req),
        EngineError::RecordsRefused(RecordsRefused::UnknownSystemField(_))
    ));
    let mut req = request("s0", &none);
    req.page_rows = Some(0);
    assert!(matches!(refusal(req), EngineError::RecordsRefused(RecordsRefused::ZeroPageRows)));
    let mut req = request("s0", &none);
    req.pages = Some(0);
    assert!(matches!(refusal(req), EngineError::RecordsRefused(RecordsRefused::ZeroPages)));
    let mut req = request("s0", &none);
    req.count = true;
    req.cursor = Some("anything");
    assert!(matches!(
        refusal(req),
        EngineError::RecordsRefused(RecordsRefused::CountWithCursor)
    ));
    assert!(matches!(refusal(request("nowhere", &none)), EngineError::UnknownView(_)));

    // A page size above the ceiling is served at the ceiling, and the head says which.
    let mut req = request("s0", &none);
    req.page_rows = Some(10_000);
    req.limits.max_page_rows = 128;
    req.pages = Some(1);
    let (sink, _) = respond(&fx.engine, &session, req).unwrap();
    assert_eq!(sink.head.unwrap().page_rows, 128);
    assert_eq!(sink.pages[0].0.num_rows(), 128);

    // Labels are the viewer's own terms, sorted, on every row.
    let system = names(&["labels"]);
    let both = fx.engine.authorise(&both_credential()).unwrap();
    let mut req = request("s0", &none);
    req.system_fields = &system;
    req.page_rows = Some(30);
    req.pages = Some(1);
    let by_tid = sources_by_tid(&fx);
    let (sink, _) = respond(&fx.engine, &both, req).unwrap();
    let labels = col::<ListArray>(&sink.pages[0].0, "tessera:labels");
    for (i, tid) in ids_of(&sink.pages[0].0).into_iter().enumerate() {
        let row = labels.value(i);
        let row = row.as_any().downcast_ref::<StringArray>().unwrap();
        let got: Vec<&str> = row.iter().map(|l| l.unwrap()).collect();
        let expected: Vec<&str> = match subset_sees(by_tid[&tid]) {
            true => vec!["0", "1"],
            false => vec!["0"],
        };
        assert_eq!(got, expected);
    }
}

/// **A response ends at its page count, at its byte budget, and at cancellation, each with a
/// cursor the read resumes from**, and the resumed read misses and repeats nothing.
#[test]
fn a_response_ends_at_its_pages_its_bytes_and_cancellation_and_the_read_resumes() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["note"]);
    let everything = fx.tids(&fx.map_order(0..N, "s0"));

    let mut req = request("s0", &fields);
    req.order = Some(RecordsOrder::Map);
    req.page_rows = Some(100);
    req.pages = Some(3);
    let (sink, trailer) = respond(&fx.engine, &session, req.clone()).unwrap();
    assert_eq!((trailer.pages, trailer.rows), (3, 300));
    assert_eq!(trailer.ended_by, ResponseEndedBy::Pages);
    assert!(sink.pages.iter().all(|(_, end)| end.ended_by == PageEndedBy::Rows));
    let mut ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();

    // A byte budget of two page ceilings: a third page could take the response past it.
    let mut budgeted = req.clone();
    budgeted.pages = None;
    budgeted.limits.max_page_bytes = 1500;
    budgeted.limits.response_bytes = 3000;
    let next = trailer.next.unwrap();
    budgeted.cursor = Some(&next);
    let (sink, trailer) = respond(&fx.engine, &session, budgeted).unwrap();
    assert_eq!(trailer.ended_by, ResponseEndedBy::BudgetBytes);
    assert_eq!(trailer.pages, 2);
    assert!(sink.pages.iter().map(|(_, end)| end.bytes).sum::<usize>() <= 3000);
    ids.extend(sink.pages.iter().flat_map(|(b, _)| ids_of(b)));

    // Cancelled before it starts: a head, no page, and a trailer that resumes where it was.
    let mut cancelled = req.clone();
    let cancel = tessera_engine::CancelToken::new();
    cancel.cancel();
    cancelled.cancel = Some(cancel);
    let next = trailer.next.unwrap();
    cancelled.cursor = Some(&next);
    let (sink, trailer) = respond(&fx.engine, &session, cancelled).unwrap();
    assert!(sink.pages.is_empty());
    assert_eq!(trailer.ended_by, ResponseEndedBy::Deadline);

    let mut rest = req.clone();
    rest.pages = None;
    ids.extend(continue_read(&fx.engine, &session, &rest, trailer.next.unwrap()));
    assert_eq!(ids, everything);
}

// ---------------------------------------------------------------------------------------------
// Inside one response
// ---------------------------------------------------------------------------------------------

/// A sink that collects, and after its first pages runs one of `after` each, in order.
struct Between<'a> {
    inner: Collect,
    after: std::collections::VecDeque<Box<dyn FnOnce() + 'a>>,
}

impl ItemsSink for Between<'_> {
    fn head(&mut self, head: &ItemsHead) -> SinkResult {
        self.inner.head(head)
    }

    fn page(&mut self, batch: &RecordBatch, end: &ItemsPageEnd) -> SinkResult {
        self.inner.page(batch, end)?;
        if let Some(after) = self.after.pop_front() {
            after();
        }
        Ok(())
    }
}

/// One response whose sink runs each of `after` after one of its first pages.
fn respond_after<'a>(
    engine: &Engine,
    session: &Session,
    req: ItemsRequest<'_>,
    after: Vec<Box<dyn FnOnce() + 'a>>,
) -> (Collect, ItemsTrailer) {
    let mut sink = Between {
        inner: Collect::default(),
        after: after.into(),
    };
    let trailer = engine
        .items_stream(session, req, &mut sink)
        .expect("a response");
    assert!(sink.after.is_empty(), "the response carried a page for every step");
    (sink.inner, trailer)
}

/// One response whose sink runs `between` after its first page.
fn respond_between<'a>(
    engine: &Engine,
    session: &Session,
    req: ItemsRequest<'_>,
    between: impl FnOnce() + 'a,
) -> (Collect, ItemsTrailer) {
    respond_after(engine, session, req, vec![Box::new(between)])
}

/// **A suppression and a deletion accepted between two pages of one response are absent from
/// the next page**: every page composes the visible set from the generation it is built in.
#[test]
fn a_suppression_and_a_deletion_between_two_pages_of_one_response_apply_to_the_next() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["score"]);
    let mut hidden: Vec<u64> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let everything: Vec<u64> = match order {
            RecordsOrder::Map => fx.map_order(0..N, "s0"),
            RecordsOrder::Stored => fx.stored_order(0..N),
        };
        let mut ahead = everything.iter().rev().filter(|s| !hidden.contains(*s));
        let (suppressed, deleted) = (*ahead.next().unwrap(), *ahead.next().unwrap());
        let mut req = request("s0", &fields);
        req.order = Some(order);
        req.page_rows = Some(100);
        let engine = &fx.engine;
        let entity = |s: u64| EntityId::new(fx.entity[&s]);
        let (sink, trailer) = respond_between(engine, &session, req, || {
            engine.accept_change(entity(suppressed), ChangeOp::Suppress).unwrap();
            engine.accept_change(entity(deleted), ChangeOp::Delete).unwrap();
        });
        assert_eq!(trailer.ended_by, ResponseEndedBy::End, "{order:?}: one response");
        hidden.extend([suppressed, deleted]);
        let ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();
        let expected: Vec<u64> = everything
            .iter()
            .filter(|s| !hidden.contains(*s))
            .map(|&s| fx.tid(s))
            .collect();
        assert_eq!(ids, expected, "{order:?}");
    }
}

/// **A merge and a flush between two pages of one filtered response lose nothing and repeat
/// nothing**, though the filter's answer for the stretch was held in the row positions both
/// renumber.
#[test]
fn a_merge_and_a_flush_between_two_pages_of_one_filtered_response_keep_it_exact() {
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut fx = Fx::new();
        fx.engine.set_merge_for_test(false);
        for batch in 0..4u64 {
            let start = N + batch * 40;
            fx.ingest(&format!("pre-{batch}"), &(start..start + 40).collect::<Vec<_>>());
            flush(&fx.engine);
        }
        let before: Vec<u64> = (0..N + 160).collect();
        let later: Vec<u64> = (N + 160..N + 260).collect();
        let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
        let fields = names(&["band"]);
        let mut req = request("s0", &fields);
        req.order = Some(order);
        req.page_rows = Some(97);
        req.filter = Some(leaf(
            "band",
            FilterOperand::In(vec![AttrLocalId::new(1), AttrLocalId::new(3)]),
        ));
        let engine = &fx.engine;
        let inserted = std::cell::RefCell::new(Vec::new());
        let (sink, trailer) = respond_between(engine, &session, req, || {
            engine.set_merge_for_test(true);
            let merges = engine.write_executor_stats().merges;
            engine.request_flush();
            wait_until("the merge to publish", Duration::from_secs(60), || {
                engine.write_executor_stats().merges > merges
            });
            *inserted.borrow_mut() = ingest_sources(engine, "later", &later);
            flush(engine);
        });
        assert_eq!(trailer.ended_by, ResponseEndedBy::End, "{order:?}: one response");
        for (&s, entity) in later.iter().zip(inserted.into_inner()) {
            fx.entity.insert(s, entity.raw());
        }
        let first_page = ids_of(&sink.pages[0].0);
        let cut = *first_page.last().unwrap();
        let ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();

        let mut all = before.clone();
        all.extend(&later);
        let in_order = match order {
            RecordsOrder::Map => fx.map_order(all.iter().copied(), "s0"),
            RecordsOrder::Stored => fx.stored_order(all.iter().copied()),
        };
        let rank: BTreeMap<u64, usize> = in_order
            .iter()
            .enumerate()
            .map(|(i, &s)| (fx.tid(s), i))
            .collect();
        let later_set: BTreeSet<u64> = later.iter().copied().collect();
        let expected: Vec<u64> = in_order
            .iter()
            .copied()
            .filter(|s| matches!(band_of(*s), Some("low") | Some("high")))
            .filter(|s| !later_set.contains(s) || rank[&fx.tid(*s)] > rank[&cut])
            .map(|s| fx.tid(s))
            .collect();
        assert_eq!(ids, expected, "{order:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Stretch boundaries
// ---------------------------------------------------------------------------------------------

/// **A view several stretches long is read whole across every stretch boundary**, in both
/// orders, over several segments, at page sizes either side of a stretch's own size, filtered,
/// unfiltered and with every row marked: every row once, and as many as the viewport counts.
#[test]
fn every_row_is_read_once_across_stretch_boundaries() {
    let n = 30_000u64;
    let (_tmp, engine) = thirty_thousand(3);
    let segments = engine.generation().bundle.partitions["default"].views["s0"]
        .segments
        .len();
    assert!(segments >= 3, "the view holds {segments} segments");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let west = ShapeF64::Bbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 499.0,
        max_y: 1000.0,
    }
    .canonical(Space::View, &extent())
    .unwrap()
    .0;
    let filter = FilterExpr::Region(RegionLeaf::Shape(Arc::new(west)));
    let (visible, matched) = viewport_counts(&engine, &session, "s0", Some(filter.clone()));
    assert_eq!(visible, n + 2100);
    assert!(matched > visible / 3 && matched < visible, "the box holds {matched} rows");
    let fields: Vec<String> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        for page_rows in [1u32, 4097, 5000] {
            for (mode, filter, keep_unmatched, expected) in [
                ("unfiltered", None, false, visible),
                ("filtered", Some(filter.clone()), false, matched),
                ("marked", Some(filter.clone()), true, visible),
            ] {
                let mut req = request("s0", &fields);
                req.order = Some(order);
                req.page_rows = Some(page_rows);
                req.filter = filter;
                req.keep_unmatched = keep_unmatched;
                let read = read_all(&engine, &session, &req);
                let ids = read.ids();
                assert_each_once(&ids);
                assert_eq!(ids.len() as u64, expected, "{order:?} {page_rows} {mode}");
                if keep_unmatched {
                    let marked: u64 = read
                        .pages
                        .iter()
                        .map(|(b, _)| col::<BooleanArray>(b, "tessera:matched").true_count() as u64)
                        .sum();
                    assert_eq!(marked, matched, "{order:?} {page_rows} {mode}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The narrower viewer and the idset
// ---------------------------------------------------------------------------------------------

/// **A narrower viewer's rows, filtered and marked, are exactly theirs row by row, and a label
/// for a term they do not hold is withheld** though every item they see carries one.
#[test]
fn a_narrower_viewers_rows_and_labels_are_theirs_row_by_row() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&subset_credential()).unwrap();
    let by_tid = sources_by_tid(&fx);
    let visible: Vec<u64> = (0..N).filter(|&s| subset_sees(s)).collect();
    let low = |s: u64| band_of(s) == Some("low");
    let fields = names(&["band"]);
    let system = names(&["labels"]);
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let in_order = |sources: Vec<u64>| match order {
            RecordsOrder::Map => fx.map_order(sources, "s0"),
            RecordsOrder::Stored => fx.stored_order(sources),
        };
        let mut req = request("s0", &fields);
        req.system_fields = &system;
        req.order = Some(order);
        req.page_rows = Some(77);
        req.filter = Some(leaf("band", FilterOperand::Equals(AttrLocalId::new(band_code("low")))));
        let filtered = read_all(&fx.engine, &session, &req);
        let expected = in_order(visible.iter().copied().filter(|&s| low(s)).collect());
        assert_eq!(filtered.ids(), fx.tids(&expected), "{order:?}: filtered");

        req.keep_unmatched = true;
        let marked = read_all(&fx.engine, &session, &req);
        assert_eq!(marked.ids(), fx.tids(&in_order(visible.clone())), "{order:?}: marked");
        for (batch, _) in &marked.pages {
            let matched = col::<BooleanArray>(batch, "tessera:matched");
            let labels = col::<ListArray>(batch, "tessera:labels");
            for (i, tid) in ids_of(batch).into_iter().enumerate() {
                let s = by_tid[&tid];
                assert_eq!(matched.value(i), low(s), "{order:?}: the matched bit of {s}");
                let row = labels.value(i);
                let row = row.as_any().downcast_ref::<StringArray>().unwrap();
                let got: Vec<&str> = row.iter().map(|l| l.unwrap()).collect();
                assert_eq!(got, ["1"], "{order:?}: {s} carries \"0\" too, which the viewer lacks");
            }
        }
    }
}

/// **A cursor issued under another idset is the item route's stale-idset refusal**: the same
/// identity key, view, incarnation and credential, and a deployment whose idset has moved.
#[test]
fn a_cursor_issued_under_another_idset_is_stale() {
    let issuing = tempfile::TempDir::new().unwrap();
    let root = build_bundle_under(issuing.path(), IDSET);
    let engine = engine_at(issuing.path(), &root, 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields: Vec<String> = Vec::new();
    let mut req = request("s0", &fields);
    req.page_rows = Some(10);
    req.pages = Some(1);
    let (_, trailer) = respond(&engine, &session, req.clone()).unwrap();
    let cursor = trailer.next.unwrap();

    let moved = tempfile::TempDir::new().unwrap();
    let root = build_bundle_under(moved.path(), IDSET + 1);
    let engine = engine_at(moved.path(), &root, 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    req.cursor = Some(&cursor);
    req.pages = None;
    assert!(matches!(
        respond(&engine, &session, req).map(|_| ()),
        Err(EngineError::StaleIdSet)
    ));
}

/// **The stored walk serves only rows the page's mask admits**, where the candidate it walks is
/// wider: after a flush, a session's projection is served one generation stale until it is
/// rebuilt, and its viewport does not draw the flushed rows, while the candidate, brought forward
/// to the new generation, holds them. Both orders read what the viewport counts.
#[test]
fn the_stored_walk_serves_what_the_mask_admits_where_the_candidate_is_wider() {
    let mut fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let (before, _) = viewport_counts(&fx.engine, &session, "s0", None);
    assert_eq!(before, N);
    fx.ingest("late", &(N..N + 30).collect::<Vec<_>>());
    flush(&fx.engine);
    let (visible, _) = viewport_counts(&fx.engine, &session, "s0", None);
    let fields: Vec<String> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request("s0", &fields);
        req.order = Some(order);
        let ids = read_all(&fx.engine, &session, &req).ids();
        assert_each_once(&ids);
        assert_eq!(ids.len() as u64, visible, "{order:?}: the viewport's count");
    }
}

// ---------------------------------------------------------------------------------------------
// Time and cancellation inside a page
// ---------------------------------------------------------------------------------------------

/// The 30,000-item fixture of `common`, opened with its executor, and `batches` more segments of
/// 700 items each ingested and flushed into its view, placed as the fixture places its own.
fn thirty_thousand(batches: u64) -> (tempfile::TempDir, Engine) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let n = 30_000u64;
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        n,
    );
    let engine = engine_at(tmp.path(), &root, 3600);
    engine.set_background_refresh_for_test(false);
    for batch in 0..batches {
        let rows: Vec<UnallocatedRow> = (0..700u64)
            .map(|i| {
                let s = n + batch * 700 + i;
                UnallocatedRow {
                    external_id: Some(s.to_le_bytes().to_vec()),
                    view: "s0".to_string(),
                    join: None,
                    x: ((s * 37) % 1000) as f64 + 0.5,
                    y: ((s * 53) % 1000) as f64 + 0.5,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&[b"0".to_vec()]),
                    descriptors: vec![b"0".to_vec()],
                    scoped: Vec::new(),
                }
            })
            .collect();
        engine
            .accept_ingest(rows, format!("batch-{batch}"), [0u8; 32])
            .expect("the ingest is accepted");
        flush(&engine);
    }
    (tmp, engine)
}

/// A region over `common`'s positions as a filter.
fn region_of(shape: ShapeF64) -> FilterExpr {
    let shape = shape.canonical(Space::View, &extent()).unwrap().0;
    FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)))
}

/// **A time budget reached while a page holds rows ends that page short, by time, and the
/// response after it**, whose cursor the next response resumes from with no row lost or
/// repeated. A narrower viewer and a filter make the rows a page wants sparser than the rows it
/// scans, so a page reaches the budget before it fills. The view is read as one segment and as
/// four, where the budget can run out while the segments' first chunks are still being gathered.
#[test]
fn a_page_cut_by_time_is_sent_short_and_the_read_resumes_after_it() {
    for batches in [0, 3] {
        page_cut_by_time(batches);
    }
}

fn page_cut_by_time(batches: u64) {
    let (_tmp, engine) = thirty_thousand(batches);
    let session = engine.authorise(&subset_credential()).unwrap();
    let fields: Vec<String> = Vec::new();
    let west = region_of(ShapeF64::Bbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 499.0,
        max_y: 1000.0,
    });
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request("s0", &fields);
        req.order = Some(order);
        req.filter = Some(west.clone());
        req.page_rows = Some(3000);
        let whole = read_all(&engine, &session, &req).ids();

        req.limits.response_time = Duration::ZERO;
        let (sink, trailer) = respond(&engine, &session, req.clone()).unwrap();
        assert_eq!(sink.pages.len(), 1, "{order:?} {batches}: one page before the budget ends it");
        let (batch, end) = &sink.pages[0];
        assert_eq!(end.ended_by, PageEndedBy::Time, "{order:?}");
        assert!(batch.num_rows() > 0 && batch.num_rows() < 3000, "{order:?}: a short page");
        assert_eq!(trailer.ended_by, ResponseEndedBy::BudgetTime, "{order:?}");

        let mut ids = ids_of(batch);
        ids.extend(continue_read(&engine, &session, &req, trailer.next.unwrap()));
        assert_eq!(ids, whole, "{order:?}: the read resumed with nothing lost or repeated");
    }
}

/// **Responses cancelled at a third of a full scan's time still complete a read under a sparse
/// filter**: each sends the rows its page held when it was cancelled, and the next resumes past
/// everything it scanned. The filter is a thin band across the map, so its rows are spread
/// through the scan in both orders, and a page is larger than the band, so it holds rows at
/// every cancellation after its first. The view is read as one segment and as four.
#[test]
fn chained_responses_cancelled_mid_scan_complete_a_sparse_read() {
    for batches in [0, 3] {
        cancelled_mid_scan(batches);
    }
}

fn cancelled_mid_scan(batches: u64) {
    let (_tmp, engine) = thirty_thousand(batches);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields: Vec<String> = Vec::new();
    let band = region_of(ShapeF64::Bbox {
        min_x: 0.0,
        min_y: 500.0,
        max_x: 1000.0,
        max_y: 502.0,
    });
    let (_, matched) = viewport_counts(&engine, &session, "s0", Some(band.clone()));
    assert!(matched > 20 && matched < 1000, "the band holds {matched} items");
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request("s0", &fields);
        req.order = Some(order);
        req.filter = Some(band.clone());
        // Larger than the band holds, so a page is still gathering rows whenever it is cut.
        req.page_rows = Some(1000);
        let started = std::time::Instant::now();
        let whole = read_all(&engine, &session, &req).ids();
        let cut = started.elapsed() / 3;
        assert_eq!(whole.len() as u64, matched, "{order:?}");

        let mut ids: Vec<u64> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut responses = 0;
        loop {
            responses += 1;
            assert!(responses <= 200, "{order:?} {batches}: the read made no progress");
            let token = tessera_engine::CancelToken::new();
            let deadline = {
                let token = token.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(cut);
                    token.cancel();
                })
            };
            let mut next = req.clone();
            next.cursor = cursor.as_deref();
            next.cancel = Some(token);
            let (sink, trailer) = respond(&engine, &session, next).unwrap();
            deadline.join().unwrap();
            for (batch, end) in &sink.pages {
                assert_ne!(end.ended_by, PageEndedBy::Bytes, "{order:?}");
                ids.extend(ids_of(batch));
            }
            cursor = trailer.next;
            if cursor.is_none() {
                break;
            }
        }
        assert!(responses > 1, "{order:?}: the cut ended at least one response");
        assert_eq!(ids, whole, "{order:?}: every row once, in order");
    }
}

/// **A projection refreshed between two pages of one response serves the rows it makes
/// visible**, matched as the filter matches them. Rows ingested and flushed after the first page
/// are outside the session's projection, served a generation stale, on the second; the refresh
/// lands after it with no new generation, and the pages after it must answer for them, both in
/// the rows they return and in the matched bit.
#[test]
fn rows_a_refreshed_projection_makes_visible_are_served_and_matched() {
    let mut fx = Fx::new();
    fx.engine.set_background_refresh_for_test(true);
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let mut held = N;
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        for keep_unmatched in [false, true] {
            assert_eq!(viewport_counts(&fx.engine, &session, "s0", None).0, held);
            let later: Vec<u64> = (held..held + 300).collect();
            let fields = names(&["prose"]);
            let mut req = request("s0", &fields);
            req.order = Some(order);
            req.page_rows = Some(100);
            req.keep_unmatched = keep_unmatched;
            req.filter = Some(leaf(
                "prose",
                FilterOperand::Match {
                    query: "group1".into(),
                    minimum: None,
                },
            ));
            let engine = &fx.engine;
            let inserted = std::cell::RefCell::new(Vec::new());
            let (sink, trailer) = respond_after(
                engine,
                &session,
                req,
                vec![
                    Box::new(|| {
                        engine.set_refresh_paused_for_test(true);
                        *inserted.borrow_mut() = ingest_sources(engine, "late", &later);
                        flush(engine);
                        assert_eq!(
                            viewport_counts(engine, &session, "s0", None).0,
                            held,
                            "the projection is served stale"
                        );
                    }),
                    Box::new(|| {
                        engine.set_refresh_paused_for_test(false);
                        wait_until("the refresh to land", Duration::from_secs(60), || {
                            viewport_counts(engine, &session, "s0", None).0 == held + 300
                        });
                    }),
                ],
            );
            assert_eq!(trailer.ended_by, ResponseEndedBy::End);
            for (&s, entity) in later.iter().zip(inserted.into_inner()) {
                fx.entity.insert(s, entity.raw());
            }
            let cut = *ids_of(&sink.pages[1].0).last().unwrap();
            let all: Vec<u64> = (0..held + 300).collect();
            let in_order = match order {
                RecordsOrder::Map => fx.map_order(all.iter().copied(), "s0"),
                RecordsOrder::Stored => fx.stored_order(all.iter().copied()),
            };
            let rank: BTreeMap<u64, usize> = in_order
                .iter()
                .enumerate()
                .map(|(i, &s)| (fx.tid(s), i))
                .collect();
            let matches = |s: u64| s % 3 == 1;
            let expected: Vec<u64> = in_order
                .iter()
                .copied()
                .filter(|&s| keep_unmatched || matches(s))
                .filter(|&s| s < held || rank[&fx.tid(s)] > rank[&cut])
                .collect();
            let by_tid: BTreeMap<u64, u64> = all.iter().map(|&s| (fx.tid(s), s)).collect();
            let ids: Vec<u64> = sink.pages.iter().flat_map(|(b, _)| ids_of(b)).collect();
            assert_eq!(
                ids,
                fx.tids(&expected),
                "{order:?} keep_unmatched={keep_unmatched}: rows"
            );
            if keep_unmatched {
                for (batch, _) in &sink.pages {
                    let matched = col::<BooleanArray>(batch, "tessera:matched");
                    for (i, tid) in ids_of(batch).into_iter().enumerate() {
                        let s = by_tid[&tid];
                        assert_eq!(matched.value(i), matches(s), "{order:?}: matched bit of {s}");
                    }
                }
            }
            held += 300;
        }
    }
}

/// The bytes the arrays of `batch` hold in their buffers: values, offsets and validity, and their
/// children's.
fn buffer_bytes(batch: &RecordBatch) -> usize {
    fn of(data: &arrow::array::ArrayData) -> usize {
        data.buffers().iter().map(|b| b.len()).sum::<usize>()
            + data.nulls().map_or(0, |n| n.buffer().len())
            + data.child_data().iter().map(of).sum::<usize>()
    }
    batch.columns().iter().map(|c| of(&c.to_data())).sum()
}

/// The bytes `batch` comes to as an Arrow IPC stream, schema and all.
fn encoded_bytes(batch: &RecordBatch) -> usize {
    let mut writer =
        arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &batch.schema()).unwrap();
    writer.write(batch).unwrap();
    writer.finish().unwrap();
    writer.into_inner().unwrap().len()
}

/// **A page's bytes are the bytes its columns' buffers hold**, strings included, and the page is
/// held to the ceiling by them: the batch's own buffers, and its encoded size less framing, come
/// to what the page end reports.
#[test]
fn a_pages_bytes_are_its_buffers_and_the_ceiling_holds_on_them() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&both_credential()).unwrap();
    let fields = names(&["note", "prose", "tag", "band", "score", "when"]);
    let system = names(&["labels", "external_id", "position"]);
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request("s0", &fields);
        req.system_fields = &system;
        req.order = Some(order);
        req.limits.max_page_bytes = 8192;
        let read = read_all(&fx.engine, &session, &req);
        assert_each_once(&read.ids());
        assert_eq!(read.ids().len() as u64, N, "{order:?}");
        for (batch, end) in &read.pages {
            assert_eq!(end.bytes, buffer_bytes(batch), "{order:?}: the page's buffers");
            assert!(end.bytes <= 8192 || batch.num_rows() == 1, "{order:?}: the ceiling");
            let encoded = encoded_bytes(batch);
            let framing = 8 * 3 * batch.num_columns() + 4096;
            assert!(
                end.bytes <= encoded && encoded <= end.bytes + framing,
                "{order:?}: {} bytes counted, {encoded} encoded",
                end.bytes
            );
        }
    }
}

/// **A row the byte ceiling cuts from a page adds no key to that page's dictionary**: every
/// page's dictionary holds exactly the keys its rows carry.
#[test]
fn a_row_cut_by_the_ceiling_adds_no_key_to_the_dictionary() {
    let fx = Fx::new();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["band"]);
    let mut req = request("s0", &fields);
    req.limits.max_page_bytes = 40;
    req.pages = Some(200);
    let (sink, _) = respond(&fx.engine, &session, req).unwrap();
    assert!(
        sink.pages.iter().all(|(_, end)| end.ended_by == PageEndedBy::Bytes),
        "every page is cut by the ceiling"
    );
    for (batch, _) in &sink.pages {
        let band = col::<DictionaryArray<Int32Type>>(batch, "band");
        let keys = band.values().as_any().downcast_ref::<StringArray>().unwrap();
        let dictionary: BTreeSet<&str> = keys.iter().map(|k| k.unwrap()).collect();
        let carried: BTreeSet<&str> = (0..band.len())
            .filter_map(|i| band.key(i))
            .map(|k| keys.value(k))
            .collect();
        assert_eq!(dictionary, carried);
    }
}

/// **A malformed filter is refused before the head whatever rows remain**, for a viewer who sees
/// everything and for one who sees nothing, in both orders.
#[test]
fn a_malformed_filter_is_refused_whatever_rows_remain() {
    let fx = Fx::new();
    let fields: Vec<String> = Vec::new();
    for credential in [full_coverage_credential(), zero_credential()] {
        let session = fx.engine.authorise(&credential).unwrap();
        for order in [RecordsOrder::Map, RecordsOrder::Stored] {
            let mut req = request("s0", &fields);
            req.order = Some(order);
            req.filter = Some(leaf("nothing", FilterOperand::Equals(AttrLocalId::new(1))));
            assert!(matches!(
                respond(&fx.engine, &session, req).map(|_| ()),
                Err(EngineError::FilterMalformed(_))
            ));
        }
    }
}

/// **A cell holding more rows than a stretch may span is read whole**, every row once, in both
/// orders: a stretch's end can fall inside a cell.
#[test]
fn a_cell_larger_than_a_stretch_is_read_whole() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        1000,
    );
    let engine = engine_at(tmp.path(), &root, 3600);
    engine.set_background_refresh_for_test(false);
    let rows: Vec<UnallocatedRow> = (0..10_000u64)
        .map(|i| UnallocatedRow {
            external_id: Some(format!("crowd-{i}").into_bytes()),
            view: "s0".to_string(),
            join: None,
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            descriptors: vec![b"0".to_vec()],
            scoped: Vec::new(),
        })
        .collect();
    engine
        .accept_ingest(rows, "crowd".to_string(), [0u8; 32])
        .expect("the ingest is accepted");
    flush(&engine);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let (visible, _) = viewport_counts(&engine, &session, "s0", None);
    assert_eq!(visible, 11_000);
    let fields: Vec<String> = Vec::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let mut req = request("s0", &fields);
        req.order = Some(order);
        req.page_rows = Some(1000);
        // A stretch's ceiling at its floor of 4,096 rows, under the 10,000 in the one cell.
        req.limits.max_page_bytes = 16 * 4096;
        let ids = read_all(&engine, &session, &req).ids();
        assert_each_once(&ids);
        assert_eq!(ids.len() as u64, visible, "{order:?}");
    }
}
