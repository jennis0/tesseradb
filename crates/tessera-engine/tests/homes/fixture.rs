//! The fixture both every-home tests run over: one item, [`X`], with a value in every home.
//!
//! Twenty-four items in three views, `s0` and a group `quarter` of two keys, each key holding a
//! float and a text family scoped to it. Every item carries a band, a score, a tag, a note, prose
//! and a unique `ident`, each of its own; a layer holds one artifact over every item, its content
//! generated from [`X`] and [`Y`].

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, Int32Array, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use crate::common::*;
use tessera_build::config::{Attribute, Config, Fields};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_engine::filter::{FilterExpr, FilterOperand};
use tessera_engine::{Engine, EngineConfig, Session, ViewportRequest};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Projection;
use tessera_types::EntityId;

pub const N: u64 = 24;
/// The item edited.
pub const X: u64 = 10;
/// The other item the content is generated from.
pub const Y: u64 = 4;
/// The group's two keys and the items each holds; [`X`] is in both.
pub const QUARTERS: [(&str, std::ops::Range<u64>); 2] = [("q1", 0..16), ("q2", 8..24)];
pub const VIEWS: [&str; 3] = ["quarter:q1", "quarter:q2", "s0"];
pub const WHOLE: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
pub const LAYER: &str = "topics/a";
pub const CONTENT: &str = "a label";

pub const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "band"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  mid = 2
  high = 3

[[attribute]]
name       = "band"
type       = "category"
render     = true
index      = true
vocabulary = "band"

[[attribute]]
name   = "score"
type   = "i32"
render = true
index  = true

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
name   = "ident"
type   = "u64"
index  = true
unique = true
"#;

/// The positions of `score`, `tag` and the rest in a row's scalars, in declared order.
pub const SCORE_AT: usize = 1;
pub const DECLARED: usize = 6;

pub fn position(s: u64) -> (f64, f64) {
    (((s * 37) % 1000) as f64, ((s * 53) % 1000) as f64)
}
pub fn band_code(s: u64) -> u8 {
    (s % 3) as u8 + 1
}
pub fn band_of(s: u64) -> &'static str {
    ["low", "mid", "high"][(s % 3) as usize]
}
pub fn score_of(s: u64) -> i32 {
    100 + s as i32
}
pub fn tag_of(s: u64) -> String {
    format!("tag-{s}")
}
pub fn note_of(s: u64) -> String {
    format!("note-{s}")
}
pub fn prose_of(s: u64) -> String {
    format!("shared p{s}q")
}
pub fn ident_of(s: u64) -> u64 {
    7_000 + s
}
pub fn heat(slot: usize, s: u64) -> f32 {
    (s * 10 + slot as u64) as f32 / 4.0
}
pub fn memo(slot: usize, s: u64) -> String {
    format!("memo m{slot}n{s}x")
}

pub fn write_parquet(path: &Path, columns: Vec<(&str, ArrayRef)>) {
    let schema = Arc::new(ArrowSchema::new(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), false))
            .collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_new(
        schema.clone(),
        columns.into_iter().map(|(_, array)| array).collect(),
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

pub fn geometry(ids: &[u64]) -> Vec<(&'static str, ArrayRef)> {
    vec![
        ("entity_id", Arc::new(UInt64Array::from(ids.to_vec()))),
        (
            "x",
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|s| position(*s).0),
            )),
        ),
        (
            "y",
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|s| position(*s).1),
            )),
        ),
    ]
}

pub fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection: Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Fields::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

pub fn scoped(
    name: &str,
    ty: ScalarType,
    analyser: Option<&str>,
    views: Vec<usize>,
) -> ScopedColumnFamily {
    ScopedColumnFamily {
        attribute: Attribute {
            name: name.to_string(),
            title: None,
            field: None,
            ty,
            analyser: analyser
                .map(|name| tessera_analyse::identity_of(name).expect("the analyser is carried")),
            vocabulary: None,
            value_set: None,
            index: true,
            render: false,
            unique: false,
        },
        group: "quarter".to_string(),
        views,
        source: None,
    }
}

pub fn build_homes(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, N);
    let ids: Vec<u64> = (0..N).collect();
    let world = dir.join("world.parquet");
    let mut columns = geometry(&ids);
    columns.extend([
        (
            "band",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|s| band_of(*s)),
            )) as ArrayRef,
        ),
        (
            "score",
            Arc::new(Int32Array::from_iter_values(
                ids.iter().map(|s| score_of(*s)),
            )),
        ),
        (
            "tag",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|s| tag_of(*s)),
            )),
        ),
        (
            "note",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|s| note_of(*s)),
            )),
        ),
        (
            "prose",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|s| prose_of(*s)),
            )),
        ),
        (
            "ident",
            Arc::new(UInt64Array::from_iter_values(
                ids.iter().map(|s| ident_of(*s)),
            )),
        ),
    ]);
    write_parquet(&world, columns);
    let mut views = vec![view_args("s0", &world, &pairs)];
    let mut quarter_views = Vec::new();
    for (slot, (key, members)) in QUARTERS.iter().enumerate() {
        let path = dir.join(format!("quarter-{key}.parquet"));
        let ids: Vec<u64> = members.clone().collect();
        let mut columns = geometry(&ids);
        columns.push((
            "heat",
            Arc::new(Float32Array::from_iter_values(
                ids.iter().map(|s| heat(slot, *s)),
            )),
        ));
        columns.push((
            "memo",
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|s| memo(slot, *s)),
            )),
        ));
        write_parquet(&path, columns);
        quarter_views.push(views.len());
        views.push(view_args(&format!("quarter:{key}"), &path, &pairs));
    }
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &Default::default())
        .expect("the schema parses")
        .schema;
    let e = extent();
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
            visibility: None,
            name: "quarter".to_string(),
            members_of: None,
            views: QUARTERS
                .iter()
                .map(|(key, _)| GroupViewDescriptor {
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
        }],
        scoped_attributes: vec![
            scoped("heat", ScalarType::F32, None, quarter_views.clone()),
            scoped("memo", ScalarType::Text, Some("unicode"), quarter_views),
        ],
        attribute_sources: tessera_build::config::AttributeSource::over(world, &schema),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
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
    .expect("the fixture builds");
    out
}

pub fn open(tmp: &Path, root: &Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
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

pub fn label_layer() -> tessera_types::layer::LayerDeclaration {
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
        MembershipSource, SuppliedContent, SuppliedRequirement,
    };
    LayerDeclaration {
        scope: Default::default(),
        name: LAYER.into(),
        title: None,
        views: vec!["s0".into()],
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

/// One artifact over every item, its content generated from [`X`] and [`Y`].
pub fn publish(engine: &Engine, root: &Path) {
    use tessera_lifecycle::membership::IncomingContent;
    use tessera_lifecycle::IncomingArtifact;
    engine
        .register_layer(label_layer())
        .expect("the layer registers");
    let map = source_to_new_map(root, "v00000");
    let entity = |s: u64| EntityId::new(map[&s]);
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                (0..N).map(entity).collect::<Vec<_>>(),
                vec![IncomingContent::new(
                    vec![CONTENT.to_string()],
                    vec![entity(X), entity(Y)],
                )],
            )],
        )
        .expect("the artifact publishes");
    tick(engine);
}

pub fn served(
    engine: &Engine,
    session: &Session,
    view: &str,
    filter: Option<FilterExpr>,
) -> BTreeSet<u64> {
    let mut req = ViewportRequest::new(view, 0, WHOLE, 10_000);
    req.filter = filter;
    engine
        .viewport(session, req)
        .unwrap()
        .points
        .tessera_ids
        .into_iter()
        .collect()
}

pub fn leaf(column: &str, operand: FilterOperand) -> Option<FilterExpr> {
    Some(FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    })
}
