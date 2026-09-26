//! **One item edited, and every home it has to be carried to.** An edit moves the item to a new
//! entity and keeps its `tessera_id`, so every place the old entity's data lives must be written
//! again for the new one, or read through to it: its rows and positions in every view, every
//! column family, the record blob, its external id, its labels, a unique value, its layer
//! memberships, the items a content was generated from, a suppression standing against it, and
//! the group-scoped values and prose of every key it holds.
//!
//! [`Home`] is the list, shared with `deletion_reaches_every_home.rs`, and [`check`] matches it
//! exhaustively, so a home added there does not compile here until an edit is shown to carry it.
//!
//! The edited item is suppressed, edited through one view, shown again, then edited through a
//! group's view with a row carrying no coordinates, and the log is replayed before that edit is
//! flushed. Every home is checked after each step, after a fold and after a restart, through what
//! the engine serves.

mod common;
mod homes;

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, Int32Array, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use homes::Home;
use tessera_build::config::{Attribute, Config, Fields};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
use tessera_engine::{
    ColumnBuf, Engine, EngineConfig, IngestRequest, ItemOut, ScalarOut, Session, ViewportRequest,
};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::IngestRow;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Projection;
use tessera_types::{AttrLocalId, EntityId, TesseraId};

const N: u64 = 24;
/// The item edited.
const X: u64 = 10;
/// The other item the content is generated from.
const Y: u64 = 4;
/// The group's two keys and the items each holds; [`X`] is in both.
const QUARTERS: [(&str, std::ops::Range<u64>); 2] = [("q1", 0..16), ("q2", 8..24)];
const VIEWS: [&str; 3] = ["quarter:q1", "quarter:q2", "s0"];
const WHOLE: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "topics/a";
const CONTENT: &str = "a label";

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
const SCORE_AT: usize = 1;
const DECLARED: usize = 6;

fn position(s: u64) -> (f64, f64) {
    (((s * 37) % 1000) as f64, ((s * 53) % 1000) as f64)
}
fn band_code(s: u64) -> u8 {
    (s % 3) as u8 + 1
}
fn band_of(s: u64) -> &'static str {
    ["low", "mid", "high"][(s % 3) as usize]
}
fn score_of(s: u64) -> i32 {
    100 + s as i32
}
fn tag_of(s: u64) -> String {
    format!("tag-{s}")
}
fn note_of(s: u64) -> String {
    format!("note-{s}")
}
fn prose_of(s: u64) -> String {
    format!("shared p{s}q")
}
fn ident_of(s: u64) -> u64 {
    7_000 + s
}
fn heat(slot: usize, s: u64) -> f32 {
    (s * 10 + slot as u64) as f32 / 4.0
}
fn memo(slot: usize, s: u64) -> String {
    format!("memo m{slot}n{s}x")
}

fn write_parquet(path: &Path, columns: Vec<(&str, ArrayRef)>) {
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

fn geometry(ids: &[u64]) -> Vec<(&'static str, ArrayRef)> {
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

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
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

fn scoped(
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

fn build_fixture(dir: &Path) -> std::path::PathBuf {
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

fn open(tmp: &Path, root: &Path) -> Engine {
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

fn label_layer() -> tessera_types::layer::LayerDeclaration {
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
fn publish(engine: &Engine, root: &Path) {
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

/// What the edited item should hold, and what it held at the build.
struct Expected {
    tid: TesseraId,
    /// The entity the build gave it.
    first: EntityId,
    suppressed: bool,
    score: i32,
    heat: [f32; 2],
    /// Its card at the build, before any edit.
    built: ItemOut,
}

fn served(
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

fn leaf(column: &str, operand: FilterOperand) -> Option<FilterExpr> {
    Some(FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    })
}

fn field(card: &ItemOut, name: &str) -> Option<ScalarOut> {
    card.fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.value.clone())
}

fn scoped_of(card: &ItemOut, family: &str) -> Vec<(String, ScalarOut)> {
    card.scoped
        .iter()
        .find(|s| s.name == family)
        .map(|s| s.values.clone())
        .unwrap_or_default()
}

/// Every home of the edited item, through what the engine serves.
fn check(engine: &Engine, expected: &Expected, after: &str) {
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let tid = expected.tid;
    let card = engine.item(&full, tid).unwrap();
    for home in Home::ALL {
        match home {
            // Readable whether or not the item may be seen.
            Home::EditedItems => {
                let entity = engine.resolve_tessera_ids(&[tid]).unwrap()[0].unwrap_or_else(|| {
                    panic!("Home::EditedItems: the tessera_id names nothing, after {after}")
                });
                assert_eq!(
                    engine.tessera_id_of(entity).unwrap(),
                    tid,
                    "Home::EditedItems: the entity it names answers another tessera_id, after {after}"
                );
            }
            Home::ExternalIdSidecar => {
                let entity = engine
                    .resolve_external_id(&source_id_key(X))
                    .unwrap()
                    .unwrap_or_else(|| {
                        panic!(
                            "Home::ExternalIdSidecar: the external id names nothing, after {after}"
                        )
                    });
                assert_eq!(
                    engine.tessera_id_of(entity).unwrap(),
                    tid,
                    "Home::ExternalIdSidecar: the external id names another item, after {after}"
                );
            }
            Home::Suppression => {
                if expected.suppressed {
                    assert!(
                        card.is_none(),
                        "Home::Suppression: a suppressed item has a card, after {after}"
                    );
                    for view in VIEWS {
                        assert!(
                            !served(engine, &full, view, None).contains(&tid.raw()),
                            "Home::Suppression: {view} serves a suppressed item, after {after}"
                        );
                    }
                } else {
                    assert!(
                        card.is_some(),
                        "Home::Suppression: a shown item has no card, after {after}"
                    );
                }
            }
            // No other home is observable while the suppression stands.
            _ if expected.suppressed => {}
            Home::Row => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    card.views, expected.built.views,
                    "Home::Row: the item's views and positions, after {after}"
                );
                for view in VIEWS {
                    assert!(
                        served(engine, &full, view, None).contains(&tid.raw()),
                        "Home::Row: {view} does not serve the item, after {after}"
                    );
                }
            }
            Home::RenderColumn | Home::RenderPresence => {
                let out = engine
                    .viewport(&full, ViewportRequest::new("s0", 0, WHOLE, 10_000))
                    .unwrap();
                let at = out
                    .points
                    .tessera_ids
                    .iter()
                    .position(|t| *t == tid.raw())
                    .expect("s0 serves the item");
                let column = |name: &str| {
                    let i = out.scalar_names.iter().position(|n| n == name).unwrap();
                    &out.points.scalars[i]
                };
                let score = column("score");
                let band = column("band");
                match home {
                    Home::RenderColumn => {
                        let ColumnBuf::U8(bands) = &band.values else {
                            panic!("band renders as u8")
                        };
                        let ColumnBuf::I32(scores) = &score.values else {
                            panic!("score renders as i32")
                        };
                        assert_eq!(
                            (bands[at], scores[at]),
                            (band_code(X), expected.score),
                            "Home::RenderColumn: the rendered row, after {after}"
                        );
                    }
                    _ => assert!(
                        score.is_present(at),
                        "Home::RenderPresence: the rendered score is marked absent, after {after}"
                    ),
                }
            }
            Home::ValueColumn => {
                let exactly = |v: i32| FilterOperand::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(i128::from(v)),
                        inclusive: true,
                    }),
                    hi: Some(Endpoint {
                        value: Scalar::Int(i128::from(v)),
                        inclusive: true,
                    }),
                };
                assert_eq!(
                    served(engine, &full, "s0", leaf("score", exactly(expected.score))),
                    BTreeSet::from([tid.raw()]),
                    "Home::ValueColumn: the score column, after {after}"
                );
            }
            Home::CategoryPostings => assert!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "band",
                        FilterOperand::Equals(AttrLocalId::new(u32::from(band_code(X))))
                    )
                )
                .contains(&tid.raw()),
                "Home::CategoryPostings: the band postings, after {after}"
            ),
            Home::KeywordDictionary => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf("tag", FilterOperand::TextEquals(tag_of(X)))
                ),
                BTreeSet::from([tid.raw()]),
                "Home::KeywordDictionary: the tag, after {after}"
            ),
            Home::TextIndex => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "prose",
                        FilterOperand::Match {
                            query: format!("p{X}q"),
                            minimum: None
                        }
                    )
                ),
                BTreeSet::from([tid.raw()]),
                "Home::TextIndex: the prose's words, after {after}"
            ),
            Home::RecordBlob => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    (field(card, "note"), field(card, "prose")),
                    (
                        Some(ScalarOut::Utf8(note_of(X))),
                        Some(ScalarOut::Utf8(prose_of(X)))
                    ),
                    "Home::RecordBlob: the stored note and prose, after {after}"
                );
                for name in ["band", "tag", "ident"] {
                    assert_eq!(
                        field(card, name),
                        field(&expected.built, name),
                        "Home::RecordBlob: {name} on the card, after {after}"
                    );
                }
            }
            Home::TermPostings => {
                assert!(
                    served(engine, &full, "s0", None).contains(&tid.raw()),
                    "Home::TermPostings: a principal holding its label cannot see it, after {after}"
                );
                assert!(
                    !served(engine, &subset, "s0", None).contains(&tid.raw()),
                    "Home::TermPostings: a principal without its label sees it, after {after}"
                );
            }
            Home::UniqueIndex => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "ident",
                        FilterOperand::NumIn(vec![Scalar::Int(i128::from(ident_of(X)))])
                    )
                ),
                BTreeSet::from([tid.raw()]),
                "Home::UniqueIndex: the item's unique value, after {after}"
            ),
            Home::Membership => {
                let t0 = artifacts_of(engine, &full_coverage_credential())
                    .into_iter()
                    .find(|a| a.key.as_deref() == Some("t0"))
                    .expect("the artifact is served");
                assert_eq!(
                    t0.masked_count, N,
                    "Home::Membership: the artifact counts every item, after {after}"
                );
            }
            Home::GeneratingSet => {
                let t0 = artifacts_of(engine, &full_coverage_credential())
                    .into_iter()
                    .find(|a| a.key.as_deref() == Some("t0"))
                    .expect("the artifact is served");
                assert_eq!(
                    t0.content,
                    vec![CONTENT.to_string()],
                    "Home::GeneratingSet: the content generated from the item, after {after}"
                );
            }
            Home::ScopedValue => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    scoped_of(card, "heat"),
                    vec![
                        ("q1".to_string(), ScalarOut::F32(expected.heat[0])),
                        ("q2".to_string(), ScalarOut::F32(expected.heat[1])),
                    ],
                    "Home::ScopedValue: heat per key, after {after}"
                );
                for (slot, (key, _)) in QUARTERS.iter().enumerate() {
                    let value = f64::from(expected.heat[slot]);
                    let exactly = FilterOperand::Range {
                        lo: Some(Endpoint {
                            value: Scalar::Float(value),
                            inclusive: true,
                        }),
                        hi: Some(Endpoint {
                            value: Scalar::Float(value),
                            inclusive: true,
                        }),
                    };
                    assert!(
                        served(
                            engine,
                            &full,
                            &format!("quarter:{key}"),
                            leaf(&format!("heat@quarter:{key}"), exactly)
                        )
                        .contains(&tid.raw()),
                        "Home::ScopedValue: heat under {key}, after {after}"
                    );
                }
            }
            Home::ScopedProse => {
                for (slot, (key, _)) in QUARTERS.iter().enumerate() {
                    let word = format!("m{slot}n{X}x");
                    assert_eq!(
                        served(
                            engine,
                            &full,
                            &format!("quarter:{key}"),
                            leaf(
                                &format!("memo@quarter:{key}"),
                                FilterOperand::Match {
                                    query: word,
                                    minimum: None
                                }
                            )
                        ),
                        BTreeSet::from([tid.raw()]),
                        "Home::ScopedProse: memo under {key}, after {after}"
                    );
                }
            }
        }
    }
}

fn edit(engine: &Engine, batch: &str, view: &str, row: IngestRow) {
    let receipt = engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash: {
                let mut hash = [0u8; 32];
                hash[..batch.len()].copy_from_slice(batch.as_bytes());
                hash
            },
            view: Some(view.to_string()),
            rows: vec![row],
            artifacts: Default::default(),
        })
        .expect("the edit is accepted");
    assert_eq!(receipt.edited, 1, "{batch} edits the item: {receipt:?}");
}

/// A row naming the item by its `tessera_id` and carrying only what `set` gives it.
fn naming(tid: TesseraId, set: impl FnOnce(&mut IngestRow)) -> IngestRow {
    let mut row = IngestRow {
        tessera_id: Some(tid),
        external_id: None,
        labels: None,
        position: None,
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).collect(),
    };
    set(&mut row);
    row
}

#[test]
fn an_edit_carries_every_home() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_fixture(tmp.path());
    let mut engine = open(tmp.path(), &root);
    publish(&engine, &root);

    let first = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let tid = engine.tessera_id_of(first).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let mut expected = Expected {
        tid,
        first,
        suppressed: false,
        score: score_of(X),
        heat: [heat(0, X), heat(1, X)],
        built: engine
            .item(&full, tid)
            .unwrap()
            .expect("the built item has a card"),
    };
    check(&engine, &expected, "the build");

    // Suppressed, then edited through `s0`: the suppression goes with the item.
    engine.accept_change(first, ChangeOp::Suppress).unwrap();
    expected.suppressed = true;
    expected.score = 555;
    edit(
        &engine,
        "score",
        "s0",
        naming(tid, |row| {
            row.scalars[SCORE_AT] = WalScalar::I32(expected.score);
            row.omitted.retain(|at| *at != SCORE_AT);
        }),
    );
    check(&engine, &expected, "an edit of the suppressed item");
    publish_buffered(&engine);
    check(&engine, &expected, "its flush");
    let moved = engine.resolve_tessera_ids(&[tid]).unwrap()[0].unwrap();
    assert_ne!(moved, expected.first, "the edit gave the item a new entity");
    engine.accept_change(moved, ChangeOp::Unsuppress).unwrap();
    expected.suppressed = false;
    check(&engine, &expected, "the suppression lifted");

    // Edited through a group's view with no coordinates, and the log replayed before the flush.
    expected.heat[0] = 99.5;
    edit(
        &engine,
        "heat",
        "quarter:q1",
        naming(tid, |row| {
            row.scoped[0] = WalScalar::F32(expected.heat[0]);
            row.omitted.retain(|at| *at != DECLARED);
        }),
    );
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    for view in VIEWS {
        assert!(
            !served(&engine, &full, view, None).contains(&tid.raw()),
            "an edited item is placed again by its flush, not before: {view}"
        );
    }
    drop(engine);
    engine = open(tmp.path(), &root);
    publish_buffered(&engine);
    check(&engine, &expected, "a restart before the edit's flush");
    // Two edits, each giving the item an entity with a row in its three views; the first entity
    // is deleted and still on disc until the fold.
    let verified = tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the edited items agree with the rows");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (2, 6));

    fold(&engine);
    check(&engine, &expected, "a fold");
    let verified = tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the folded edited items agree with the rows");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (1, 3));
    drop(engine);
    let engine = open(tmp.path(), &root);
    check(&engine, &expected, "a restart after the fold");
}

/// **A view dropped while an edit waits in the same commit window** does not strand the edit.
/// The edit is committed first, its own row in the dropped view gives way to its row in another
/// view, and the item is served there with what the edit changed, before and after a restart.
#[test]
fn a_view_dropped_behind_an_edit_in_one_window_keeps_the_item() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_fixture(tmp.path());
    let mut engine = open(tmp.path(), &root);
    let first = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let tid = engine.tessera_id_of(first).unwrap();

    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    std::thread::scope(|scope| {
        let edited = scope.spawn(|| {
            edit(
                &engine,
                "heat",
                "quarter:q1",
                naming(tid, |row| {
                    row.scoped[0] = WalScalar::F32(99.5);
                    row.omitted.retain(|at| *at != DECLARED);
                }),
            )
        });
        wait_until(
            "the edit is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued,
        );
        let dropped = scope.spawn(|| {
            engine
                .drop_view("quarter".to_string(), "q1".to_string(), false)
                .expect("the drop is accepted")
        });
        wait_until(
            "the drop is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued + 1,
        );
        engine.set_work_pass_paused_for_test(false);
        edited.join().unwrap();
        dropped.join().unwrap();
    });
    publish_buffered(&engine);
    for pass in ["the flush", "a restart"] {
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        let card = engine
            .item(&full, tid)
            .unwrap()
            .unwrap_or_else(|| panic!("the edited item has a card after {pass}"));
        let views: Vec<&str> = card.views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(views, ["quarter:q2", "s0"], "after {pass}");
        assert!(
            served(&engine, &full, "s0", None).contains(&tid.raw()),
            "s0 serves the item after {pass}"
        );
        assert_eq!(
            engine.generation().buffer.oldest_wal_pos(),
            None,
            "nothing buffered holds the log after {pass}"
        );
        drop(engine);
        engine = open(tmp.path(), &root);
        publish_buffered(&engine);
    }
}

/// **A growth and a publication naming an item, queued behind an edit of it**, reach the item
/// where the edit moved it: the item joins the artifact grown, and a content generated from it
/// is served, before and after the fold that retires the entity they named.
#[test]
fn a_growth_and_a_publication_queued_behind_an_edit_follow_the_item() {
    use tessera_lifecycle::membership::IncomingContent;
    use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_fixture(tmp.path());
    let engine = open(tmp.path(), &root);
    let map = source_to_new_map(&root, "v00000");
    let entity = |s: u64| EntityId::new(map[&s]);
    let tid = engine.tessera_id_of(entity(X)).unwrap();
    let mut grown = label_layer();
    grown.name = "topics/b".into();
    engine.register_layer(grown).unwrap();
    engine
        .publish_artifacts(
            "topics/b".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("g0".into()),
                [entity(Y)],
                vec![IncomingContent::new(vec!["grown".to_string()], [entity(Y)])],
            )],
        )
        .unwrap();
    tick(&engine);

    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    std::thread::scope(|scope| {
        let edited = scope.spawn(|| {
            edit(
                &engine,
                "score",
                "s0",
                naming(tid, |row| {
                    row.scalars[SCORE_AT] = WalScalar::I32(555);
                    row.omitted.retain(|at| *at != SCORE_AT);
                }),
            )
        });
        wait_until(
            "the edit is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued,
        );
        let growth = scope.spawn(|| {
            engine
                .grow_memberships(
                    "topics/b".into(),
                    0,
                    vec![IncomingGrowth::from_entities("g0".into(), [entity(X)])],
                )
                .expect("the growth is accepted")
        });
        let publication = scope.spawn(|| {
            engine
                .publish_artifacts(
                    "topics/b".into(),
                    0,
                    vec![IncomingArtifact::with_content(
                        Some("g1".into()),
                        [entity(X), entity(Y)],
                        vec![IncomingContent::new(
                            vec!["published".to_string()],
                            [entity(X)],
                        )],
                    )],
                )
                .expect("the publication is accepted")
        });
        wait_until(
            "both are queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued + 2,
        );
        engine.set_work_pass_paused_for_test(false);
        edited.join().unwrap();
        growth.join().unwrap();
        publication.join().unwrap();
    });
    publish_buffered(&engine);
    assert_ne!(
        engine.resolve_tessera_ids(&[tid]).unwrap()[0],
        Some(entity(X)),
        "the edit moved the item"
    );
    for after in ["the flush", "the fold"] {
        let served = artifacts_of(&engine, &full_coverage_credential());
        let artifact = |key: &str| {
            served
                .iter()
                .find(|a| a.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("{key} is served after {after}"))
        };
        assert_eq!(
            artifact("g0").masked_count,
            2,
            "the grown artifact holds the item after {after}"
        );
        assert_eq!(
            artifact("g1").masked_count,
            2,
            "the published artifact holds it after {after}"
        );
        assert_eq!(
            artifact("g1").content,
            vec!["published".to_string()],
            "the content generated from the item is served after {after}"
        );
        fold(&engine);
    }
}

/// Every file under `from`, copied to `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The newest side manifest of `root`'s partition, and its path.
fn side_manifest(root: &Path) -> (std::path::PathBuf, serde_json::Value) {
    let partition = root.join(current_prefix(root)).join("partitions/default");
    let newest = std::fs::read_dir(&partition)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let n: u64 = name
                .strip_prefix("SEGMENTS-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some(n)
        })
        .max()
        .unwrap();
    let path = partition.join(format!("SEGMENTS-{newest}.json"));
    let manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    (path, manifest)
}

/// **A bundle whose edited items disagree with themselves or with its rows is refused.** An
/// edited item's flushed rows list their entities and the map holds its pairs both ways; a map
/// missing one direction, a segment whose moved rows are not recorded, and a listed file that is
/// missing are each refused, by `verify --deep` or at open.
#[test]
fn verify_refuses_edited_items_that_disagree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_fixture(tmp.path());
    let engine = open(tmp.path(), &root);
    let tid = engine
        .tessera_id_of(EntityId::new(source_to_new_map(&root, "v00000")[&X]))
        .unwrap();
    edit(
        &engine,
        "score",
        "s0",
        naming(tid, |row| {
            row.scalars[SCORE_AT] = WalScalar::I32(555);
            row.omitted.retain(|at| *at != SCORE_AT);
        }),
    );
    publish_buffered(&engine);
    drop(engine);
    let verify =
        |root: &Path| tessera_build::verify_deep(root, &tessera_build::VerifyOpts::default());
    let verified = verify(&root).expect("the edited bundle verifies");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (1, 3));

    // One direction of the map dropped.
    let one_way = tmp.path().join("one-way");
    copy_tree(&root, &one_way);
    let (path, mut manifest) = side_manifest(&one_way);
    manifest["edited_items"]["by_entity"]["live"] = serde_json::json!([]);
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(verify(&one_way).is_err(), "a map held one way is refused");

    // The moved rows' entities no longer recorded.
    let unrecorded = tmp.path().join("unrecorded");
    copy_tree(&root, &unrecorded);
    let (path, mut manifest) = side_manifest(&unrecorded);
    let files = manifest["files"].as_object_mut().unwrap();
    let listed: Vec<String> = files
        .keys()
        .filter(|rel| rel.ends_with(tessera_store::edited::EDITED_ROWS_FILE))
        .cloned()
        .collect();
    assert_eq!(listed.len(), 3, "one list per view the item holds a row in");
    for rel in &listed {
        files.remove(rel);
    }
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(
        verify(&unrecorded).is_err(),
        "rows whose entity is not recorded are refused"
    );

    // A listed file that is missing.
    let missing = tmp.path().join("missing");
    copy_tree(&root, &missing);
    std::fs::remove_file(missing.join(current_prefix(&missing)).join(&listed[0])).unwrap();
    assert!(
        tessera_store::read::open_bundle(&missing).is_err(),
        "a listed edited-rows file that is missing refuses the open"
    );
}
