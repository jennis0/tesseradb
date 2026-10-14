//! Every read route over a corpus built as three shards against the same corpus built as one,
//! for three viewers. Answers are compared by the fixture's `id` and each artifact's layer and
//! key, since the two builds number items differently.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use arrow::array::{
    ArrayRef, Float64Array, Int64Array, ListBuilder, StringArray, StringBuilder, UInt32Array,
    UInt64Array,
};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use common::*;
use mosaica_build::BuildArgs;
use mosaica_engine::browse::{BrowseForm, BrowseRequest};
use mosaica_engine::filter::{
    Endpoint, FilterExpr, FilterOperand, MemberOfLeaf, RegionLeaf, Scalar,
};
use mosaica_engine::{
    AggregateHead, AggregateRequest, AggregateSink, ArtifactOut, ArtifactsRequest, By,
    CategoryQuery, Engine, Grouping, PageEnd, Pick, RecordsCounts, RecordsOrder, Reference,
    Session, SinkResult, SuggestRequest, TableHead, ViewportArtifactsRequest, ViewportRequest,
};
use mosaica_types::{AttrLocalId, MosaicaId, ShardId};

/// Items the corpus holds, by source id `0..N`.
const N: u64 = 120;
/// The shard size of the several-shard build: shards of 50, 50 and 20 items.
const SIZE: u64 = 50;
/// The items carrying term 2, the narrow viewer's: none carries term 1, so their signature sorts
/// last and the build numbers them into the last shard.
const NARROW: [u64; 10] = [1, 2, 4, 5, 7, 8, 10, 11, 13, 14];

fn narrow() -> Vec<u8> {
    br#"{"terms": ["2"]}"#.to_vec()
}

fn viewers() -> [(&'static str, Vec<u8>); 3] {
    [
        ("full", full_coverage_credential()),
        ("subset", subset_credential()),
        ("narrow", narrow()),
    ]
}

fn terms(e: u64) -> Vec<u32> {
    let mut terms = vec![0];
    if e.is_multiple_of(3) {
        terms.push(1);
    }
    if NARROW.contains(&e) {
        terms.push(2);
    }
    terms
}

fn position(e: u64) -> (f64, f64) {
    (((e * 37) % 1000) as f64, ((e * 53) % 1000) as f64)
}

fn score(e: u64) -> i64 {
    ((e * 7) % 50) as i64
}

// ---------------------------------------------------------------------------------------------
// Where the build puts each item, and the corpus laid out from it
// ---------------------------------------------------------------------------------------------

/// The items of each shard of a build at [`SIZE`], by the terms they carry, ascending.
struct Shards {
    /// Shard 0: term 0 alone.
    z0: Vec<u64>,
    /// Shard 1: term 0 alone.
    z1a: Vec<u64>,
    /// Shard 1: term 1.
    z1s: Vec<u64>,
    /// Shard 2: term 1.
    z2s: Vec<u64>,
    /// Shard 2: the narrow viewer's.
    z2n: Vec<u64>,
    /// Every item's shard.
    of: BTreeMap<u64, u32>,
}

/// Where a build at [`SIZE`] puts each item. The numbering follows the points and the access
/// relation alone, so a build of those answers it for the corpus built with its attributes and
/// layers; [`Pair::sized`] checks that it does.
fn shards() -> &'static Shards {
    static SHARDS: OnceLock<Shards> = OnceLock::new();
    SHARDS.get_or_init(|| {
        let tmp = TempDir::new().unwrap();
        let points = tmp.path().join("points.parquet");
        let pairs = tmp.path().join("pairs.parquet");
        let root = tmp.path().join("bundle");
        write_points(&points, false);
        write_pairs(&pairs);
        let schema = id_schema();
        mosaica_build::build(&build_args(
            &points,
            &pairs,
            &root,
            schema,
            Vec::new(),
            Vec::new(),
            Some(SIZE),
        ))
        .expect("the probe builds");
        let engine = open_engine(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
        let of = shard_map(&engine);
        let mut shards = Shards {
            z0: Vec::new(),
            z1a: Vec::new(),
            z1s: Vec::new(),
            z2s: Vec::new(),
            z2n: Vec::new(),
            of: of.clone(),
        };
        for (&e, &shard) in &of {
            let group = match (shard, e.is_multiple_of(3), NARROW.contains(&e)) {
                (0, false, false) => &mut shards.z0,
                (1, false, false) => &mut shards.z1a,
                (1, true, _) => &mut shards.z1s,
                (2, true, _) => &mut shards.z2s,
                (2, false, true) => &mut shards.z2n,
                other => panic!("item {e} lies where the corpus does not expect: {other:?}"),
            };
            group.push(e);
        }
        let sizes = [
            shards.z0.len(),
            shards.z1a.len(),
            shards.z1s.len(),
            shards.z2s.len(),
            shards.z2n.len(),
        ];
        assert_eq!(sizes, [50, 20, 30, 10, 10], "the build's shards, by term");
        shards
    })
}

/// Each item's shard, read from the `mosaica_id` a bulk read serves the full viewer.
fn shard_map(engine: &Engine) -> BTreeMap<u64, u32> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let read = read_items(engine, &session, &Items::new(RecordsOrder::Stored));
    read.rows_with_ids
        .iter()
        .map(|&(mosaica_id, id)| {
            let (high, _) = test_key()
                .invert(MosaicaId::new(mosaica_id))
                .expect("an item");
            (id, high.shard.raw())
        })
        .collect()
}

/// One artifact as the corpus declares it.
struct Declared {
    layer: &'static str,
    key: &'static str,
    parent: Option<&'static str>,
    /// The `clusters` artifact a label is attached to.
    target: Option<&'static str>,
    members: Vec<u64>,
    /// Each ranked version of a label: its text and its generating set.
    versions: Vec<(&'static str, Vec<u64>)>,
}

fn declared(layer: &'static str, key: &'static str, members: Vec<u64>) -> Declared {
    Declared {
        layer,
        key,
        parent: None,
        target: None,
        members,
        versions: Vec::new(),
    }
}

fn cat(e: u64) -> &'static str {
    let s = shards();
    if s.z2s[9] == e {
        "delta"
    } else if s.z0[49] == e {
        "echo"
    } else {
        ["alpha", "bravo", "charlie"][(e / 2 % 3) as usize]
    }
}

/// The corpus's artifacts, laid out over [`shards`].
///
/// `clusters` is flat and serves an artifact with eight visible members: `big0` lies in shard 0
/// and `big1` in shard 1, so the top two of a tile mix them; `split` holds six members in shard 0,
/// six in shard 1 and three in shard 2, so the subset viewer's nine clear the bar only summed;
/// `lone` holds six, below it; `n2` is the narrow viewer's, in shard 2. `nested` is a tree of the
/// same memberships under one root. `topics` labels `split` with a version generated from all of
/// it and one from its subset members, and `big0` with one. `pinned` declares a label column, and
/// its two artifacts share members in shard 1 alone.
fn artifacts() -> Vec<Declared> {
    let s = shards();
    let joined = |parts: &[&[u64]]| -> Vec<u64> { parts.concat() };
    let big0 = s.z0[0..20].to_vec();
    let big1 = joined(&[&s.z1a[0..10], &s.z1s[0..8]]);
    let split = joined(&[&s.z0[20..26], &s.z1s[8..14], &s.z2s[0..3]]);
    let n2 = s.z2n[0..9].to_vec();
    let root: Vec<u64> = joined(&[&big0, &big1, &split, &n2]);
    let subset_split: Vec<u64> = split.iter().copied().filter(|e| e % 3 == 0).collect();
    let child = |key, members: &Vec<u64>| Declared {
        parent: Some("root"),
        ..declared("nested", key, members.clone())
    };
    let label = |key, target, versions: Vec<(&'static str, Vec<u64>)>| Declared {
        target: Some(target),
        members: versions[0].1.clone(),
        versions,
        ..declared("topics", key, Vec::new())
    };
    vec![
        declared("clusters", "big0", big0.clone()),
        declared("clusters", "big1", big1.clone()),
        declared("clusters", "split", split.clone()),
        declared("clusters", "lone", s.z1s[14..20].to_vec()),
        declared("clusters", "n2", n2.clone()),
        declared("nested", "root", root),
        child("nb0", &big0),
        child("nb1", &big1),
        child("ns", &split),
        child("nn", &n2),
        label(
            "t-split",
            "split",
            vec![
                ("all of split", split.clone()),
                ("split's subset", subset_split),
            ],
        ),
        label("t-big0", "big0", vec![("all of big0", big0)]),
        declared(
            "pinned",
            "p1",
            joined(&[&s.z0[30..36], &s.z1a[14..18], &s.z1s[20..24]]),
        ),
        declared(
            "pinned",
            "p2",
            joined(&[&s.z0[36..40], &s.z1a[16..20], &s.z1s[22..27]]),
        ),
    ]
}

const CONFIG_TOML: &str = r#"
[sources]
clusters         = "clusters.parquet"
clusters_members = "clusters_members.parquet"
nested           = "nested.parquet"
nested_members   = "nested_members.parquet"
topics           = "topics.parquet"
topics_members   = "topics_members.parquet"
pinned           = "pinned.parquet"
pinned_members   = "pinned_members.parquet"

[[vocabulary]]
name       = "cat"
width      = "u8"
value_set  = "closed"
visibility = "derived"
  [vocabulary.values]
  alpha   = 1
  bravo   = 2
  charlie = 3
  delta   = 4
  echo    = 5

[[attribute]]
name   = "id"
type   = "u64"
unique = true
render = true
field  = "entity_id"

[[attribute]]
name       = "cat"
type       = "category"
render     = true
index      = true
vocabulary = "cat"

[[attribute]]
name   = "score"
type   = "i64"
render = true
index  = true

[[attribute]]
name = "note"
type = "keyword"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters"
views = ["s0"]
source = "clusters"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 8 }
hierarchy = { kind = "flat" }
content = { computed = ["centroid", "box", "hull"] }

  [layer.members]
  source = "clusters_members"
  fields = { id = "entity" }

[[layer]]
name = "nested"
views = ["s0"]
source = "nested"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 8 }
hierarchy = { kind = "nested" }

  [layer.members]
  source = "nested_members"
  fields = { id = "entity" }

[[layer]]
name = "topics"
views = ["s0"]
source = "topics"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
depends_on = ["clusters"]

  [layer.members]
  source = "topics_members"
  fields = { id = "entity" }

  [[layer.content.supplied]]
  name = "topic"
  type = "text"
  require_member_visibility = "all"

[[layer]]
name = "pinned"
views = ["s0"]
source = "pinned"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
layout = "column"
content = { computed = ["centroid", "box"] }

  [layer.members]
  source = "pinned_members"
  fields = { id = "entity" }
"#;

fn write(path: &Path, batch: RecordBatch) {
    let mut w =
        ArrowWriter::try_new(std::fs::File::create(path).unwrap(), batch.schema(), None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
    RecordBatch::try_from_iter_with_nullable(
        columns.into_iter().map(|(name, array)| (name, array, true)),
    )
    .unwrap()
}

/// The points, with `cat`, `score` and `note` where `attributes` is set.
fn write_points(path: &Path, attributes: bool) {
    let ids: Vec<u64> = (0..N).collect();
    let mut columns: Vec<(&str, ArrayRef)> = vec![
        ("entity_id", Arc::new(UInt64Array::from(ids.clone()))),
        (
            "x",
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|&e| position(e).0),
            )),
        ),
        (
            "y",
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|&e| position(e).1),
            )),
        ),
    ];
    if attributes {
        columns.push((
            "cat",
            Arc::new(StringArray::from_iter_values(ids.iter().map(|&e| cat(e)))),
        ));
        columns.push((
            "score",
            Arc::new(Int64Array::from_iter_values(ids.iter().map(|&e| score(e)))),
        ));
        columns.push((
            "note",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|&e| format!("n{e}")),
            )),
        ));
    }
    write(path, batch(columns));
}

fn write_pairs(path: &Path) {
    let (mut entities, mut term_ids) = (Vec::new(), Vec::new());
    for e in 0..N {
        for t in terms(e) {
            entities.push(e);
            term_ids.push(t);
        }
    }
    write(
        path,
        batch(vec![
            ("entity_id", Arc::new(UInt64Array::from(entities))),
            ("term_id", Arc::new(UInt32Array::from(term_ids))),
        ]),
    );
}

/// Each layer's artifacts file and members file.
fn write_layers(dir: &Path) {
    let all = artifacts();
    for layer in ["clusters", "nested", "topics", "pinned"] {
        let of: Vec<&Declared> = all.iter().filter(|a| a.layer == layer).collect();
        let keys = Arc::new(StringArray::from_iter_values(of.iter().map(|a| a.key)));
        let mut columns: Vec<(&str, ArrayRef)> = vec![("key", keys)];
        if layer == "nested" {
            columns.push((
                "parent",
                Arc::new(StringArray::from_iter(of.iter().map(|a| a.parent))),
            ));
        }
        if layer == "topics" {
            let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
            for a in &of {
                for (text, _) in &a.versions {
                    contents.values().values().append_value(text);
                    contents.values().append(true);
                }
                contents.append(true);
            }
            columns.push(("contents", Arc::new(contents.finish())));
            columns.push((
                "attached_layer",
                Arc::new(StringArray::from_iter(of.iter().map(|_| Some("clusters")))),
            ));
            columns.push((
                "attached_key",
                Arc::new(StringArray::from_iter(of.iter().map(|a| a.target))),
            ));
        }
        write(&dir.join(format!("{layer}.parquet")), batch(columns));

        let mut rows: Vec<(&str, Option<u32>, u64)> = Vec::new();
        for a in &of {
            rows.extend(a.members.iter().map(|&e| (a.key, None, e)));
            for (rank, (_, set)) in a.versions.iter().enumerate() {
                rows.extend(set.iter().map(|&e| (a.key, Some(rank as u32), e)));
            }
        }
        write(
            &dir.join(format!("{layer}_members.parquet")),
            batch(vec![
                (
                    "key",
                    Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.0))),
                ),
                (
                    "rank",
                    Arc::new(UInt32Array::from_iter(rows.iter().map(|r| r.1))),
                ),
                (
                    "entity",
                    Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.2))),
                ),
            ]),
        );
    }
}

fn build_args(
    points: &Path,
    pairs: &Path,
    out: &Path,
    schema: mosaica_build::config::Schema,
    layers: Vec<mosaica_types::layer::LayerDeclaration>,
    layer_inputs: Vec<mosaica_build::config::LayerSources>,
    shard_size: Option<u64>,
) -> BuildArgs {
    BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        shard_size,
        strict: true,
        identity_key: test_key(),
        layers,
        layer_inputs,
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

/// The corpus built at `shard_size`, open.
struct Built {
    _tmp: TempDir,
    engine: Engine,
    names: Names,
}

impl Built {
    fn new(shard_size: Option<u64>) -> Built {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let (points, pairs, root) = (
            dir.join("points.parquet"),
            dir.join("pairs.parquet"),
            dir.join("bundle"),
        );
        write_points(&points, true);
        write_pairs(&pairs);
        write_layers(dir);
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, CONFIG_TOML).unwrap();
        let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
            .expect("the corpus's declaration parses");
        mosaica_build::build(&build_args(
            &points,
            &pairs,
            &root,
            config.schema,
            config.layers,
            config.layer_sources,
            shard_size,
        ))
        .expect("the corpus builds");
        let engine = open_engine_publishing(&root, &dir.join("cache"), &dir.join("wal"));
        let names = Names::of(&engine);
        Built {
            _tmp: tmp,
            engine,
            names,
        }
    }
}

/// The corpus built as one shard and as three.
struct Pair {
    one: Built,
    several: Built,
}

impl Pair {
    fn new() -> Pair {
        Pair::sized(SIZE)
    }

    fn sized(size: u64) -> Pair {
        let pair = Pair {
            one: Built::new(None),
            several: Built::new(Some(size)),
        };
        if size == SIZE {
            assert_eq!(
                shard_map(&pair.several.engine),
                shards().of,
                "the corpus's items lie where the probe found them"
            );
        }
        pair
    }

    /// `ask` of each build, for each viewer, asserted equal.
    fn same<T: PartialEq + std::fmt::Debug>(
        &self,
        what: &str,
        ask: impl Fn(&Built, &Session) -> T,
    ) {
        for (viewer, credential) in viewers() {
            let one = ask(&self.one, &self.one.engine.authorise(&credential).unwrap());
            let several = ask(
                &self.several,
                &self.several.engine.authorise(&credential).unwrap(),
            );
            assert_eq!(several, one, "{what}, to the {viewer} viewer");
        }
    }
}

/// What one build's identifiers name: each item's `id`, and each artifact's layer and key.
struct Names {
    items: HashMap<u64, u64>,
    artifacts: HashMap<MosaicaId, String>,
    ids: HashMap<String, MosaicaId>,
}

impl Names {
    fn of(engine: &Engine) -> Names {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        let items = read_items(engine, &session, &Items::new(RecordsOrder::Stored))
            .rows_with_ids
            .into_iter()
            .collect();
        let mut artifacts = HashMap::new();
        for (_, credential) in viewers() {
            let session = engine.authorise(&credential).unwrap();
            let out = engine
                .viewport_artifacts(
                    &session,
                    ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX),
                )
                .expect("the artifacts viewport answers");
            for a in out.artifacts() {
                artifacts.insert(a.mosaica_id, format!("{}/{}", a.layer, a.key.unwrap()));
            }
        }
        let ids = artifacts
            .iter()
            .map(|(&id, name)| (name.clone(), id))
            .collect();
        Names {
            items,
            artifacts,
            ids,
        }
    }

    fn artifact(&self, id: MosaicaId) -> String {
        self.artifacts
            .get(&id)
            .unwrap_or_else(|| panic!("{id:?} names an artifact some viewer is served"))
            .clone()
    }

    fn id(&self, name: &str) -> MosaicaId {
        self.ids[name]
    }

    fn item(&self, mosaica_id: u64) -> u64 {
        self.items[&mosaica_id]
    }
}

// ---------------------------------------------------------------------------------------------
// Reading each route, with identifiers replaced by names
// ---------------------------------------------------------------------------------------------

/// Every declared field: rendered, and the record store's `note`.
fn every_field(order: RecordsOrder) -> Items {
    Items {
        fields: ["id", "cat", "score", "note"].map(String::from).to_vec(),
        ..Items::new(order)
    }
}

/// An artifact as served, its identifiers replaced by names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Artifact {
    name: String,
    masked_count: u64,
    /// The centroid, the box and the shape, as text.
    derived: String,
    parents: Vec<String>,
    target: Option<String>,
    content: Vec<String>,
    rung: u32,
    matched: Option<bool>,
    highlighted: Option<bool>,
}

fn artifact(names: &Names, a: &ArtifactOut) -> Artifact {
    let mut parents: Vec<String> = a.parent_ids.iter().map(|&p| names.artifact(p)).collect();
    parents.sort();
    Artifact {
        name: names.artifact(a.mosaica_id),
        masked_count: a.masked_count,
        derived: format!("{:?}", a.derived),
        parents,
        target: a.target.map(|t| names.artifact(t)),
        content: a.content.clone(),
        rung: a.rung,
        matched: a.matched,
        highlighted: a.highlighted,
    }
}

/// Each frame of an artifacts viewport, its artifacts as a set, in the response's order.
fn frames(
    built: &Built,
    session: &Session,
    request: ViewportArtifactsRequest<'_>,
) -> Vec<(Option<u64>, Vec<Artifact>)> {
    let out = built
        .engine
        .viewport_artifacts(session, request)
        .expect("the artifacts viewport answers");
    out.frames
        .iter()
        .map(|frame| {
            let mut served: Vec<Artifact> = frame
                .artifacts
                .iter()
                .map(|a| artifact(&built.names, a))
                .collect();
            served.sort();
            (frame.tile, served)
        })
        .collect()
}

/// Every artifact's card, by name: `None` where it is withheld.
fn cards(built: &Built, session: &Session) -> BTreeMap<String, Option<Artifact>> {
    let mut names: Vec<&String> = built.names.ids.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let card = built
                .engine
                .artifact(session, built.names.id(name), "s0", None)
                .expect("the card answers");
            (name.clone(), card.map(|a| artifact(&built.names, &a)))
        })
        .collect()
}

/// A served point: its `id`, its highlight bit and each layer's tag.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    id: u64,
    highlighted: Option<bool>,
    tags: Vec<(String, Option<String>)>,
}

/// A viewport's tiles, its points by `id` and its underlay, its identifiers replaced by names.
#[derive(Debug, PartialEq)]
struct Viewport {
    /// `(tile, visible, matched, served, highlighted)`.
    tiles: Vec<(u64, u64, u64, u64, u64)>,
    points: Vec<Point>,
    sub_cells: Vec<(u64, u64)>,
}

fn viewport(built: &Built, session: &Session, request: ViewportRequest<'_>) -> Viewport {
    let out = built
        .engine
        .viewport(session, request)
        .expect("the viewport answers");
    let names = &built.names;
    let ids = &out.points.mosaica_ids;
    let mut points: Vec<_> = (0..ids.len())
        .map(|i| {
            let tags = out
                .points
                .membership
                .iter()
                .map(|column| {
                    let tag = column.ids[i].map(|a| names.artifact(MosaicaId::new(a)));
                    (column.layer.clone(), tag)
                })
                .collect();
            let highlighted = out.points.highlighted.as_ref().map(|bits| bits[i]);
            Point {
                id: names.item(ids[i]),
                highlighted,
                tags,
            }
        })
        .collect();
    points.sort();
    Viewport {
        tiles: out
            .tiles
            .iter()
            .map(|t| (t.tile, t.visible, t.matched, t.served, t.highlighted))
            .collect(),
        points,
        sub_cells: out.sub_cells.iter().map(|c| (c.cell, c.count)).collect(),
    }
}

fn range(column: &str, lo: i64, hi: i64) -> FilterExpr {
    let at = |value: i64| {
        Some(Endpoint {
            value: Scalar::Int(value.into()),
            inclusive: true,
        })
    };
    FilterExpr::Leaf {
        column: column.to_string(),
        operand: FilterOperand::Range {
            lo: at(lo),
            hi: at(hi),
        },
    }
}

/// `score` in `[10, 30]`: items of every shard.
fn scored() -> FilterExpr {
    range("score", 10, 30)
}

/// `cat` is `alpha`.
fn alpha() -> FilterExpr {
    FilterExpr::Leaf {
        column: "cat".to_string(),
        operand: FilterOperand::Equals(AttrLocalId::new(1)),
    }
}

fn member_of(built: &Built, name: &str) -> FilterExpr {
    FilterExpr::MemberOf(MemberOfLeaf {
        layer: name.split('/').next().unwrap().to_string(),
        artifact: built.names.id(name),
    })
}

fn region_of(built: &Built, name: &str) -> FilterExpr {
    FilterExpr::Region(RegionLeaf::Artifact(built.names.id(name)))
}

/// A browse row, its identifiers replaced by names.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Browsed {
    name: String,
    masked_count: u64,
    matched_count: Option<u64>,
    rung: u32,
    parents: Vec<String>,
    children: u64,
}

/// Each row a browse serves, by name.
fn browse(
    built: &Built,
    session: &Session,
    layer: &str,
    form: BrowseForm,
    filter: Option<FilterExpr>,
) -> Vec<Browsed> {
    let out = built
        .engine
        .browse(
            session,
            BrowseRequest {
                view: "s0",
                layer,
                level: None,
                form,
                filter,
                limit: 100,
                cursor: None,
                palette_size: None,
            },
        )
        .expect("browse answers");
    let names = &built.names;
    let mut rows: Vec<_> = out
        .artifacts
        .iter()
        .chain(&out.parents)
        .map(|row| {
            let mut parents: Vec<String> =
                row.parent_ids.iter().map(|&p| names.artifact(p)).collect();
            parents.sort();
            Browsed {
                name: names.artifact(row.mosaica_id),
                masked_count: row.masked_count,
                matched_count: row.matched_count,
                rung: row.rung,
                parents,
                children: row.child_count,
            }
        })
        .collect();
    rows.sort();
    rows
}

/// A bulk read of a layer's artifacts, two to a page, its rows as a set, and its counts.
fn bulk_artifacts(
    built: &Built,
    session: &Session,
    layer: &str,
    filter: Option<FilterExpr>,
    keep_unmatched: bool,
) -> (Vec<Vec<String>>, Option<RecordsCounts>) {
    let fields = ["key", "masked_count", "content", "centroid", "box"].map(String::from);
    let mut responses = drain(|cursor, pages| {
        built
            .engine
            .artifacts_stream(
                session,
                ArtifactsRequest {
                    view: "s0",
                    layer,
                    level: None,
                    parent: None,
                    q: None,
                    ids: None,
                    filter: filter.clone(),
                    keep_unmatched,
                    count: cursor.is_none(),
                    fields: &fields,
                    palette_size: None,
                    page_rows: Some(2),
                    pages: Some(1),
                    cursor,
                    limits: limits(),
                    cancel: None,
                },
                pages,
            )
            .expect("the artifacts read answers")
            .next
    });
    let counts = responses[0].head.take().and_then(|head| head.counts);
    let mut rows: Vec<Vec<String>> = responses
        .iter()
        .flat_map(|response| &response.batches)
        .flat_map(|batch| rows_of(batch, &["mosaica_id"], None))
        .collect();
    rows.sort();
    (rows, counts)
}

// ---------------------------------------------------------------------------------------------
// The artifact routes
// ---------------------------------------------------------------------------------------------

/// The top of a tile is ranked by counts summed over every shard: at depth 0 the full viewer's
/// top two are `big0`, wholly in shard 0, and `big1`, wholly in shard 1, ahead of `split`, which
/// each shard alone would rank second; the subset viewer's top one is `split`, ahead of `big1`,
/// which shard 1 alone would rank first. Every tile's frame at depths 0 to 2, and every card,
/// equals one shard's.
#[test]
fn a_tiles_top_artifacts_are_ranked_over_every_shard() {
    let pair = Pair::new();
    let names = |frames: &[(Option<u64>, Vec<Artifact>)]| -> Vec<String> {
        frames
            .iter()
            .flat_map(|(_, a)| a.iter().map(|a| a.name.clone()))
            .collect()
    };
    let top = |built: &Built, credential: &[u8], per_tile| {
        let session = built.engine.authorise(credential).unwrap();
        let request = ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, per_tile)
            .layers(mosaica_engine::LayerSelection::Named(&["clusters"]));
        names(&frames(built, &session, request))
    };
    assert_eq!(
        top(&pair.several, &full_coverage_credential(), 2),
        ["clusters/big0", "clusters/big1"]
    );
    assert_eq!(
        top(&pair.several, &subset_credential(), 1),
        ["clusters/split"]
    );
    assert_eq!(top(&pair.several, &narrow(), 2), ["clusters/n2"]);

    for zoom in 0..=2 {
        for per_tile in [1, 2, usize::MAX] {
            pair.same(
                &format!("the frames at depth {zoom}, {per_tile} a tile"),
                |built, session| {
                    frames(
                        built,
                        session,
                        ViewportArtifactsRequest::new("s0", zoom, WHOLE_MAP, per_tile),
                    )
                },
            );
        }
    }
    pair.same("every card", cards);
}

/// A label generated from a set the shard boundary cuts is served only where the viewer sees the
/// whole of it, summed over every shard: `split`'s first version to the full viewer, its second,
/// generated from the subset's members in shards 1 and 2, to the subset viewer.
#[test]
fn a_label_generated_from_a_set_the_boundary_cuts_is_contained_over_every_shard() {
    let pair = Pair::new();
    let content = |credential: &[u8]| {
        let session = pair.several.engine.authorise(credential).unwrap();
        cards(&pair.several, &session)["topics/t-split"]
            .as_ref()
            .map(|label| label.content.clone())
    };
    assert_eq!(
        content(&full_coverage_credential()),
        Some(vec!["all of split".to_string()])
    );
    assert_eq!(
        content(&subset_credential()),
        Some(vec!["split's subset".to_string()])
    );
    pair.same("every card", cards);
}

/// A `nested` layer's cut is taken once over counts summed across the shards, at every budget.
#[test]
fn a_nested_cut_is_taken_over_every_shard() {
    let pair = Pair::new();
    let cut = |built: &Built, session: &Session, budget| {
        let request = ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX)
            .layers(mosaica_engine::LayerSelection::Named(&["nested"]))
            .budget(Some(budget));
        frames(built, session, request)
    };
    let session = pair
        .several
        .engine
        .authorise(&full_coverage_credential())
        .unwrap();
    let served = |budget| -> Vec<String> {
        let frames = cut(&pair.several, &session, budget);
        frames
            .into_iter()
            .flat_map(|(_, a)| a)
            .map(|a| a.name)
            .collect()
    };
    assert_eq!(served(4), ["nested/root"]);
    assert_eq!(
        served(5),
        [
            "nested/nb0",
            "nested/nb1",
            "nested/nn",
            "nested/ns",
            "nested/root"
        ],
        "the root's children, `ns` among them with members in every shard"
    );
    for budget in [1, 2, 3, 4, 5, 8] {
        pair.same(
            &format!("the cut at a budget of {budget}"),
            |built, session| cut(built, session, budget),
        );
    }
    pair.same("the root's children", |built, session| {
        let root = built.names.id("nested/root");
        browse(
            built,
            session,
            "nested",
            BrowseForm::Children(root),
            Some(scored()),
        )
    });
}

/// `member_of` and a published region naming `split` take its verdict over every shard: the
/// subset viewer's nine members clear the bar only summed, so a gate asked of one shard would
/// leave the leaf empty.
#[test]
fn member_of_and_a_published_region_gate_a_split_artifact_over_every_shard() {
    let pair = Pair::new();
    let matched = |credential: &[u8], filter: fn(&Built, &str) -> FilterExpr| -> u64 {
        let built = &pair.several;
        let session = built.engine.authorise(credential).unwrap();
        let request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize)
            .filter(filter(built, "clusters/split"));
        viewport(built, &session, request)
            .tiles
            .iter()
            .map(|t| t.2)
            .sum()
    };
    for filter in [member_of, region_of] {
        assert_eq!(matched(&full_coverage_credential(), filter), 15);
        assert_eq!(matched(&subset_credential(), filter), 9);
    }

    for filter in [member_of, region_of] {
        pair.same("the viewport filtered by `split`", |built, session| {
            let request = ViewportRequest::new("s0", 2, WHOLE_MAP, N as usize)
                .filter(filter(built, "clusters/split"))
                .highlight(alpha());
            viewport(built, session, request)
        });
        pair.same("the items filtered by `split`", |built, session| {
            let read = every_field(RecordsOrder::Map).filter(filter(built, "clusters/split"));
            let read = read_items(&built.engine, session, &read);
            (read.pages, read.counts)
        });
        pair.same("the artifacts filtered by `split`", |built, session| {
            let request = ViewportArtifactsRequest::new("s0", 1, WHOLE_MAP, usize::MAX)
                .filter(filter(built, "clusters/split"))
                .highlight(alpha());
            frames(built, session, request)
        });
    }
}

/// Bulk artifacts and browse count a filter's matches in each artifact over every shard, `split`'s
/// among them, before ordering and keeping.
#[test]
fn bulk_artifacts_and_browse_count_matches_over_every_shard() {
    let pair = Pair::new();
    for layer in ["clusters", "nested", "topics", "pinned"] {
        for filter in [None, Some(scored()), Some(alpha())] {
            // An attached layer is browsed through its targets.
            if layer != "topics" {
                pair.same(
                    &format!("{layer}'s browse roots under {filter:?}"),
                    |built, session| {
                        browse(built, session, layer, BrowseForm::Roots, filter.clone())
                    },
                );
            }
            for keep_unmatched in [false, true] {
                pair.same(
                    &format!("{layer}'s artifacts under {filter:?}, keeping {keep_unmatched}"),
                    |built, session| {
                        bulk_artifacts(built, session, layer, filter.clone(), keep_unmatched)
                    },
                );
            }
        }
    }
    let split = |built: &Built| {
        let session = built.engine.authorise(&subset_credential()).unwrap();
        browse(
            built,
            &session,
            "clusters",
            BrowseForm::Roots,
            Some(scored()),
        )
        .into_iter()
        .find(|row| row.name == "clusters/split")
        .map(|row| (row.masked_count, row.matched_count))
    };
    let in_range = artifacts()
        .iter()
        .find(|a| a.key == "split")
        .unwrap()
        .members
        .iter()
        .filter(|&&e| e % 3 == 0 && (10..=30).contains(&score(e)))
        .count() as u64;
    assert_eq!(split(&pair.several), Some((9, Some(in_range))));
}

/// Slot centres sampled from the visible items take the cut from the count summed over every
/// shard: the items sampled are the viewer's whose `mosaica_id` is below `⌊S · 2⁶⁴ / N⌋`, `N`
/// the viewer's visible count over the view, which differs from what each shard's own count
/// would cut.
#[test]
fn slot_centres_take_their_sample_cut_from_the_count_over_every_shard() {
    const SAMPLE: u64 = 30;
    let pair = Pair::new();
    let cut = |visible: u64| -> u64 {
        if visible <= SAMPLE {
            u64::MAX
        } else {
            ((u128::from(SAMPLE) << 64) / u128::from(visible)) as u64
        }
    };
    for (viewer, credential) in viewers() {
        let stats = |built: &Built, layer| {
            built.engine.set_slot_sample_for_test(SAMPLE);
            let session = built.engine.authorise(&credential).unwrap();
            built
                .engine
                .cluster_slot_stats(&session, "s0", layer, 8)
                .expect("the slots build")
        };
        let session = pair.several.engine.authorise(&credential).unwrap();
        let visible: Vec<u64> = read_items(
            &pair.several.engine,
            &session,
            &Items::new(RecordsOrder::Map),
        )
        .rows_with_ids
        .into_iter()
        .map(|(mosaica_id, _)| mosaica_id)
        .collect();
        let summed = visible
            .iter()
            .filter(|&&m| m < cut(visible.len() as u64))
            .count() as u64;
        let mut by_shard: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for &m in &visible {
            let (high, _) = test_key().invert(MosaicaId::new(m)).unwrap();
            by_shard.entry(high.shard.raw()).or_default().push(m);
        }
        let per_shard: u64 = by_shard
            .values()
            .map(|ids| ids.iter().filter(|&&m| m < cut(ids.len() as u64)).count() as u64)
            .sum();

        let several = stats(&pair.several, "nested");
        let one = stats(&pair.one, "nested");
        assert_eq!(
            several.sampled_items, summed,
            "the {viewer} viewer's sample"
        );
        if viewer != "narrow" {
            assert_ne!(
                summed, per_shard,
                "the {viewer} viewer's shards would cut otherwise"
            );
        }
        assert_eq!(
            (several.clusters, several.from_figures),
            (one.clusters, one.from_figures),
            "the {viewer} viewer's nested slots"
        );
        let several = stats(&pair.several, "clusters");
        let one = stats(&pair.one, "clusters");
        assert_eq!(
            several.sampled_items, summed,
            "the {viewer} viewer's sample"
        );
        assert_eq!(
            (several.clusters, several.from_figures),
            (one.clusters, one.from_figures),
            "the {viewer} viewer's cluster slots"
        );
    }
}

/// Each point of every shard is tagged with the artifact holding it in each layer, from the
/// level's labels and from the walk of its served artifacts alike.
#[test]
fn points_of_every_shard_are_tagged_from_the_labels_and_from_the_walk() {
    let pair = Pair::new();
    for labels in [true, false] {
        pair.one.engine.set_tags_from_labels_for_test(labels);
        pair.several.engine.set_tags_from_labels_for_test(labels);
        for zoom in [0, 2] {
            pair.same(
                &format!("the tags at depth {zoom}, from labels {labels}"),
                |built, session| {
                    viewport(
                        built,
                        session,
                        ViewportRequest::new("s0", zoom, WHOLE_MAP, N as usize),
                    )
                },
            );
        }
    }
    let session = pair
        .several
        .engine
        .authorise(&full_coverage_credential())
        .unwrap();
    let tagged = viewport(
        &pair.several,
        &session,
        ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize),
    );
    let split: BTreeSet<u32> = tagged
        .points
        .iter()
        .filter(|point| {
            let split = ("clusters".to_string(), Some("clusters/split".to_string()));
            point.tags.contains(&split)
        })
        .map(|point| shards().of[&point.id])
        .collect();
    assert_eq!(
        split,
        BTreeSet::from([0, 1, 2]),
        "`split` tags points in every shard"
    );
}

/// `pinned` declares a label column, and its memberships overlap in shard 1 alone, so shard 0
/// holds a column and shard 1 does not. Its figures, its geometry and its filtered counts read
/// every shard, as one shard holding the same items answers them.
#[test]
fn a_level_whose_shards_disagree_on_its_column_reads_every_shard() {
    let pair = Pair::new();
    pair.same("pinned's frames", |built, session| {
        let request = ViewportArtifactsRequest::new("s0", 1, WHOLE_MAP, usize::MAX)
            .layers(mosaica_engine::LayerSelection::Named(&["pinned"]))
            .filter(scored())
            .highlight(alpha());
        frames(built, session, request)
    });
    let engine = &pair.several.engine;
    let column = |shard| {
        engine
            .held_artifact_form_for_test(ShardId::new(shard).unwrap(), "s0", "pinned", 0)
            .expect("the level's form is held")
            .column()
            .is_some()
    };
    assert_eq!(
        (column(0), column(1)),
        (true, false),
        "the shards' forms disagree"
    );

    pair.same("pinned's cards", |built, session| {
        cards(built, session)
            .into_iter()
            .filter(|(name, _)| name.starts_with("pinned/"))
            .collect::<Vec<_>>()
    });
    for filter in [None, Some(scored()), Some(alpha())] {
        pair.same(
            &format!("pinned's browse under {filter:?}"),
            |built, session| browse(built, session, "pinned", BrowseForm::Roots, filter.clone()),
        );
        pair.same(
            &format!("pinned's artifacts under {filter:?}"),
            |built, session| bulk_artifacts(built, session, "pinned", filter.clone(), false),
        );
    }
    pair.same("the items in p1", |built, session| {
        let read = every_field(RecordsOrder::Stored).filter(member_of(built, "pinned/p1"));
        let read = read_items(&built.engine, session, &read);
        (read.pages, read.counts)
    });
}

// ---------------------------------------------------------------------------------------------
// The item routes
// ---------------------------------------------------------------------------------------------

/// `id` among one item of each shard, and one the subset viewer cannot see.
fn ids_in() -> FilterExpr {
    let s = shards();
    FilterExpr::Leaf {
        column: "id".to_string(),
        operand: FilterOperand::NumIn(
            [s.z0[0], s.z1s[0], s.z2s[0], s.z2n[0]]
                .map(|e| Scalar::Int(e.into()))
                .to_vec(),
        ),
    }
}

/// An items read in either order, resumed page by page across every shard boundary, serves the
/// rows, pages and counts one shard serves, unfiltered and under every kind of filter.
#[test]
fn an_items_read_resumed_across_every_shard_equals_one_shards() {
    let pair = Pair::new();
    for order in [RecordsOrder::Map, RecordsOrder::Stored] {
        let reads = |built: &Built| {
            let every = every_field(order);
            vec![
                ("unfiltered", every.clone()),
                ("scored", every.clone().filter(scored())),
                ("ids", every.clone().filter(ids_in())),
                (
                    "split",
                    every.clone().filter(member_of(built, "clusters/split")),
                ),
                (
                    "scored, keeping the unmatched",
                    Items {
                        keep_unmatched: true,
                        ..every.clone().filter(scored())
                    },
                ),
                (
                    "a row a page",
                    Items {
                        page_rows: 1,
                        ..every.filter(alpha())
                    },
                ),
            ]
        };
        for at in 0..reads(&pair.one).len() {
            let what = reads(&pair.one)[at].0;
            pair.same(
                &format!("the {what} read in {order:?} order"),
                |built, session| {
                    let read = read_items(&built.engine, session, &reads(built)[at].1);
                    (read.pages, read.counts)
                },
            );
        }
    }

    let before = pair.several.engine.driven_stretches();
    let session = pair
        .several
        .engine
        .authorise(&full_coverage_credential())
        .unwrap();
    let read = read_items(
        &pair.several.engine,
        &session,
        &Items::new(RecordsOrder::Stored).filter(ids_in()),
    );
    let mut ids: Vec<u64> = read.rows_with_ids.iter().map(|&(_, id)| id).collect();
    ids.sort_unstable();
    let s = shards();
    let mut want = vec![s.z0[0], s.z1s[0], s.z2s[0], s.z2n[0]];
    want.sort_unstable();
    assert_eq!(ids, want, "one item of each shard");
    assert!(
        pair.several.engine.driven_stretches() > before,
        "the unique `in` drove the read"
    );
}

/// A stored read across twelve shards of ten items, a page of three at a time and each response
/// resumed, serves what one shard serves, filtered or not.
#[test]
fn a_stored_read_across_many_small_shards_equals_one_shards() {
    let pair = Pair::sized(10);
    assert_eq!(
        shard_map(&pair.several.engine).values().max(),
        Some(&11),
        "twelve shards"
    );
    for filter in [None, Some(scored()), Some(ids_in())] {
        pair.same(
            &format!("the stored read under {filter:?}"),
            |built, session| {
                let mut read = Items {
                    page_rows: 3,
                    ..every_field(RecordsOrder::Stored)
                };
                read.filter = filter.clone();
                let read = read_items(&built.engine, session, &read);
                (read.pages, read.counts)
            },
        );
    }
}

/// The viewport's tiles count what each viewer sees, matches and highlights over every shard,
/// its points carry their highlight bits, and its underlay counts each sub-cell over every shard.
#[test]
fn a_filtered_and_highlighted_viewport_and_its_underlay_equal_one_shards() {
    let pair = Pair::new();
    for zoom in [0, 1, 3] {
        for (filter, highlight) in [
            (None, None),
            (Some(scored()), None),
            (None, Some(alpha())),
            (Some(scored()), Some(alpha())),
            (Some(ids_in()), Some(scored())),
        ] {
            pair.same(
                &format!(
                    "the viewport at depth {zoom} under {filter:?}, highlighting {highlight:?}"
                ),
                |built, session| {
                    let mut request = ViewportRequest::new("s0", zoom, WHOLE_MAP, N as usize)
                        .underlay_offset(Some(2));
                    if let Some(filter) = &filter {
                        request = request.filter(filter.clone());
                    }
                    if let Some(highlight) = &highlight {
                        request = request.highlight(highlight.clone());
                    }
                    viewport(built, session, request)
                },
            );
        }
    }
}

/// A `derived` column lists, and suggests, the values a visible item carries in any shard, with
/// their counts summed over every shard: `delta` is carried in shard 2 alone and `echo` in shard 0
/// alone.
#[test]
fn categories_and_suggest_answer_over_every_shard() {
    let pair = Pair::new();
    let listed = |built: &Built, session: &Session| -> Vec<String> {
        built
            .engine
            .categories(
                session,
                "cat",
                CategoryQuery::Page {
                    after: None,
                    limit: 100,
                },
            )
            .expect("the column lists")
            .expect("`cat` is a category")
            .values
            .into_iter()
            .map(|v| v.key)
            .collect()
    };
    let listed_to = |credential: &[u8]| {
        listed(
            &pair.several,
            &pair.several.engine.authorise(credential).unwrap(),
        )
    };
    assert_eq!(
        listed_to(&full_coverage_credential()),
        ["alpha", "bravo", "charlie", "delta", "echo"]
    );
    assert!(listed_to(&subset_credential()).contains(&"delta".to_string()));
    assert!(!listed_to(&subset_credential()).contains(&"echo".to_string()));
    pair.same("the listing", listed);
    pair.same("the codes", |built, session| {
        built
            .engine
            .categories(session, "cat", CategoryQuery::Codes(&[1, 2, 3, 4, 5]))
            .unwrap()
            .unwrap()
            .values
            .into_iter()
            .map(|v| (v.code, v.key))
            .collect::<Vec<_>>()
    });
    for q in ["", "e", "d", "a"] {
        for filter in [None, Some(scored())] {
            for set in [0, 1_000_000] {
                pair.same(
                    &format!("the suggestions for {q:?} under {filter:?} from a set of {set}"),
                    |built, session| {
                        let page = built
                            .engine
                            .suggest(
                                session,
                                SuggestRequest {
                                    column: "cat",
                                    view: Some("s0"),
                                    filter: filter.as_ref(),
                                    q,
                                    limit: 20,
                                    counts: true,
                                    walk_budget: 100_000,
                                    max_suggest_set_entities: set,
                                    cancel: None,
                                },
                            )
                            .expect("the column suggests")
                            .expect("`cat` is a category");
                        let mut values: Vec<(String, Option<u64>)> =
                            page.values.into_iter().map(|v| (v.key, v.count)).collect();
                        values.sort();
                        (values, page.more)
                    },
                );
            }
        }
    }
}

#[derive(Default)]
struct Tables {
    heads: Vec<(u32, u64, Option<u64>, Option<u64>)>,
    pages: Vec<(u32, RecordBatch)>,
}

impl AggregateSink for Tables {
    fn head(&mut self, _: &AggregateHead) -> SinkResult {
        Ok(())
    }

    fn table(&mut self, head: &TableHead) -> SinkResult {
        self.heads
            .push((head.grouping, head.total, head.reference_total, head.groups));
        Ok(())
    }

    fn page(&mut self, table: u32, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.pages.push((table, batch.clone()));
        Ok(())
    }
}

/// The aggregate's groupings sum over every shard before any is chosen or cut: a field's top
/// values and named ones, bins under edges taken over every shard, a field's summary, a layer's
/// top artifacts and named ones, and cells; unfiltered, filtered, and against a reference.
#[test]
fn aggregate_groupings_sum_over_every_shard() {
    let pair = Pair::new();
    let groupings = |built: &Built| -> Vec<Grouping> {
        let by = |by| Grouping {
            by: Some(by),
            cells: None,
            area: None,
        };
        vec![
            by(By::Field {
                column: "cat".into(),
                pick: Pick::Top(2),
            }),
            by(By::Field {
                column: "cat".into(),
                pick: Pick::Named(vec!["alpha".into(), "delta".into(), "echo".into()]),
            }),
            by(By::Bins {
                column: "score".into(),
                bins: 5,
                range: None,
                sample: None,
            }),
            by(By::Summary {
                column: "score".into(),
            }),
            by(By::Layer {
                layer: "clusters".into(),
                level: None,
                pick: Pick::Top(2),
                cut: None,
                palette: None,
            }),
            by(By::Layer {
                layer: "clusters".into(),
                level: None,
                pick: Pick::Named(vec![
                    built.names.id("clusters/split"),
                    built.names.id("clusters/n2"),
                ]),
                cut: None,
                palette: None,
            }),
            Grouping {
                by: Some(By::Field {
                    column: "cat".into(),
                    pick: Pick::Top(3),
                }),
                cells: Some(1),
                area: None,
            },
            Grouping {
                by: None,
                cells: Some(3),
                area: None,
            },
        ]
    };
    type Case = (
        &'static str,
        fn(&Built) -> Option<FilterExpr>,
        Option<Reference>,
    );
    let cases: Vec<Case> = vec![
        ("unfiltered", |_| None, None),
        ("scored", |_| Some(scored()), None),
        (
            "scored against the visible",
            |_| Some(scored()),
            Some(Reference::Visible),
        ),
        ("in split", |b| Some(member_of(b, "clusters/split")), None),
    ];
    for (what, filter, reference) in cases {
        pair.same(&format!("the {what} aggregate"), |built, session| {
            let groupings = groupings(built);
            let mut tables = Tables::default();
            built
                .engine
                .aggregate_stream(
                    session,
                    AggregateRequest {
                        view: "s0",
                        filter: filter(built),
                        reference: reference.clone(),
                        groupings: &groupings,
                        page_rows: None,
                        pages: None,
                        cursor: None,
                        limits: limits(),
                        caps: caps(),
                        cancel: None,
                    },
                    &mut tables,
                )
                .expect("the aggregate answers");
            let pages: Vec<(u32, Vec<Vec<String>>)> = tables
                .pages
                .iter()
                .map(|(table, batch)| {
                    let name = |id| built.names.artifact(id);
                    let mut rows = rows_of(batch, &["slot"], Some(&name));
                    rows.sort();
                    (*table, rows)
                })
                .collect();
            (tables.heads, pages)
        });
    }
}
