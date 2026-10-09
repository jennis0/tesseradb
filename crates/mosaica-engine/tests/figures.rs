//! **Every figure served beside an artifact is computed over exactly the rows its viewer may see.**
//!
//! A level served from its row column is counted once per grant over the grant's base rows, and
//! each request subtracts its denied rows and adds its rows above the base. These cases serve the
//! result through the viewport and the identifier route and hold it against two oracles: the
//! corpus's own model of which items each principal may see, for the count and whether the artifact
//! is served at all, and a walk of the request's composed mask one visible row at a time, for the
//! count, the centroid and the box.
//!
//! Four corpora: one access key per item, several per item, conjunctions, and a label per document.
//! Principals who see everything, part and nothing, and a session that reads every item. Then the
//! moves a running service makes: deletion, suppression and its lift, ingest before and after a
//! flush, growth, a fold, a restart, a projection one generation behind.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, ListArray, StringArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use mosaica_build::config::{AccessInput, AccessSource};
use mosaica_build::BuildArgs;
use mosaica_engine::{ArtifactOut, Engine, LayerSelection, Session};
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::{IncomingArtifact, IncomingGrowth, UnallocatedRow};
use mosaica_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ServingLayout,
};
use mosaica_types::EntityId;

const N: u64 = 1_200;
/// A partition: item `e` is in artifact `e % 24`, served from a label column.
const FLAT: &str = "figures/flat";
const FLAT_ARTIFACTS: u64 = 24;
/// Overlapping: item `e` is in `e % 10`, and every seventh also in the next, served from a list
/// column.
const LIST: &str = "figures/list";
const LIST_ARTIFACTS: u64 = 10;

/// How a corpus labels its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Corpus {
    SingleKey,
    MultiKey,
    Conjunction,
    PerDocument,
}

const CORPORA: [Corpus; 4] = [
    Corpus::SingleKey,
    Corpus::MultiKey,
    Corpus::Conjunction,
    Corpus::PerDocument,
];

impl Corpus {
    /// The access labels of the item at source `e`.
    fn labels(self, e: u64) -> Vec<String> {
        match self {
            Corpus::SingleKey => vec![format!("k{}", e % 4)],
            Corpus::MultiKey => vec![format!("k{}", e % 4), format!("k{}", 4 + (e / 4) % 3)],
            Corpus::Conjunction => match e % 5 {
                0 => vec![format!("k{}", e % 3)],
                _ => vec![format!("k{}&m{}", e % 3, e % 2)],
            },
            Corpus::PerDocument => vec![format!("doc{e}")],
        }
    }

    /// The credentials swept, by name: the terms each holds. Reading every item is a session of
    /// its own ([`Principal::ReadAll`]).
    fn principals(self) -> Vec<Principal> {
        let held = |terms: Vec<String>| Principal::Terms(terms);
        let keys = |names: &[&str]| held(names.iter().map(|s| s.to_string()).collect());
        let mut out = vec![Principal::ReadAll, keys(&[])];
        match self {
            Corpus::SingleKey => {
                out.push(keys(&["k0", "k1", "k2", "k3"]));
                out.push(keys(&["k1"]));
            }
            Corpus::MultiKey => {
                out.push(keys(&["k0", "k1", "k2", "k3", "k4", "k5", "k6"]));
                out.push(keys(&["k2", "k5"]));
            }
            Corpus::Conjunction => {
                out.push(keys(&["k0", "k1", "k2", "m0", "m1"]));
                out.push(keys(&["k1", "m0"]));
                out.push(keys(&["k0", "m1", "k2"]));
            }
            Corpus::PerDocument => {
                out.push(held(
                    (0..N)
                        .filter(|e| e.is_multiple_of(3))
                        .map(|e| format!("doc{e}"))
                        .collect(),
                ));
                out.push(held(
                    (0..N)
                        .filter(|e| e % 7 < 2)
                        .map(|e| format!("doc{e}"))
                        .collect(),
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
enum Principal {
    ReadAll,
    Terms(Vec<String>),
}

impl Principal {
    fn session(&self, engine: &Engine) -> Session {
        match self {
            Principal::ReadAll => engine.authorise_all().unwrap(),
            Principal::Terms(terms) => {
                let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
                engine
                    .authorise(format!("{{\"terms\": [{}]}}", quoted.join(", ")).as_bytes())
                    .unwrap()
            }
        }
    }

    /// Whether this principal satisfies an item carrying `labels`.
    fn satisfies(&self, labels: &[String]) -> bool {
        let Principal::Terms(terms) = self else {
            return true;
        };
        labels.iter().any(|label| {
            label
                .split('&')
                .all(|term| terms.iter().any(|held| held == term))
        })
    }
}

fn x_of(e: u64) -> f64 {
    ((e * 37) % 1000) as f64
}

fn y_of(e: u64) -> f64 {
    ((e * 53) % 1000) as f64
}

fn write_points(path: &Path, corpus: Corpus) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(
            "access",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let mut offsets: Vec<i32> = vec![0];
    let mut flat: Vec<String> = Vec::new();
    for &e in &ids {
        flat.extend(corpus.labels(e));
        offsets.push(flat.len() as i32);
    }
    let list = ListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(offsets.into()),
        Arc::new(StringArray::from(flat)) as ArrayRef,
        None,
    );
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| x_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| y_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(list),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn declaration(name: &str, layout: ServingLayout) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: Some(ExistenceCriterion::Count(1)),
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into(), "box".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: Some(layout),
        shape: None,
    }
}

/// Which artifacts of each layer the item at source `e` is a member of.
fn memberships(e: u64) -> Vec<(&'static str, String)> {
    let mut out = vec![
        (FLAT, format!("f{}", e % FLAT_ARTIFACTS)),
        (LIST, format!("l{}", e % LIST_ARTIFACTS)),
    ];
    if e.is_multiple_of(7) {
        out.push((LIST, format!("l{}", (e + 1) % LIST_ARTIFACTS)));
    }
    out
}

/// One corpus built and served, with the model the oracle reads: every item's labels and the
/// artifacts it is a member of, by entity, and what is denied.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    wal: PathBuf,
    corpus: Corpus,
    /// Source id to entity, for the built items.
    entity: BTreeMap<u64, EntityId>,
    /// Per entity, its labels.
    labels: BTreeMap<EntityId, Vec<String>>,
    /// Per `(layer, key)`, its members.
    members: BTreeMap<(&'static str, String), BTreeSet<EntityId>>,
    /// Entities deleted.
    deleted: BTreeSet<EntityId>,
    /// Entities suppressed and not lifted.
    suppressed: BTreeSet<EntityId>,
    /// Entities a flush has given a row: every built one, and ingested ones once flushed.
    placed: BTreeSet<EntityId>,
}

impl Fixture {
    fn new(corpus: Corpus) -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("bundle");
        let points = tmp.path().join("points.parquet");
        write_points(&points, corpus);
        let schema = id_schema();
        let args = BuildArgs {
            views: vec![mosaica_build::ViewArgs {
                visibility: None,
                view_id: "s0".to_string(),
                projection: mosaica_spatial::Projection::None,
                extent: extent(),
                points: points.clone(),
                point_fields: Default::default(),
                select: None,
                access: AccessInput {
                    source: AccessSource::Field("access".to_string()),
                    default: None,
                },
            }],
            anchor: 0,
            groups: Vec::new(),
            scoped_attributes: Vec::new(),
            attribute_sources: mosaica_build::config::AttributeSource::over(points, &schema),
            out: root.clone(),
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
        };
        mosaica_build::build(&args).expect("the corpus builds");
        let entity: BTreeMap<u64, EntityId> = source_to_new_map(&root, "v00000")
            .into_iter()
            .map(|(s, e)| (s, EntityId::new(e)))
            .collect();
        let mut labels = BTreeMap::new();
        let mut members: BTreeMap<(&'static str, String), BTreeSet<EntityId>> = BTreeMap::new();
        for (&s, &e) in &entity {
            labels.insert(e, corpus.labels(s));
            for (layer, key) in memberships(s) {
                members.entry((layer, key)).or_default().insert(e);
            }
        }
        Fixture {
            root,
            cache: tmp.path().join("cache"),
            wal: tmp.path().join("wal.log"),
            corpus,
            placed: entity.values().copied().collect(),
            entity,
            labels,
            members,
            deleted: BTreeSet::new(),
            suppressed: BTreeSet::new(),
            _tmp: tmp,
        }
    }

    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// Open and publish both layers' artifacts from the model.
    fn open_published(&self) -> Engine {
        let engine = self.open();
        for (layer, layout) in [
            (FLAT, ServingLayout::RowMajorLabel),
            (LIST, ServingLayout::RowMajorList),
        ] {
            engine.register_layer(declaration(layer, layout)).unwrap();
            let artifacts = self
                .members
                .iter()
                .filter(|((l, _), _)| *l == layer)
                .map(|((_, key), members)| {
                    IncomingArtifact::from_entities(Some(key.clone()), members.iter().copied())
                })
                .collect();
            engine
                .publish_artifacts(layer.into(), 0, artifacts)
                .unwrap();
        }
        tick(&engine);
        engine
    }

    fn change(&mut self, engine: &Engine, sources: impl IntoIterator<Item = u64>, op: ChangeOp) {
        let entities: Vec<EntityId> = sources.into_iter().map(|s| self.entity[&s]).collect();
        self.change_entities(engine, &entities, op);
    }

    fn change_entities(&mut self, engine: &Engine, entities: &[EntityId], op: ChangeOp) {
        engine
            .accept_changes(entities.iter().map(|e| (*e, op)).collect())
            .unwrap();
        for e in entities {
            match op {
                ChangeOp::Delete => {
                    self.deleted.insert(*e);
                }
                ChangeOp::Suppress => {
                    self.suppressed.insert(*e);
                }
                // Lifts a suppression and never a deletion.
                ChangeOp::Unsuppress => {
                    self.suppressed.remove(e);
                }
            }
        }
    }

    /// Ingest one item per `(x, y, labels)`, recording them; they have no row until a flush.
    fn ingest(
        &mut self,
        engine: &Engine,
        batch: &str,
        items: &[(f64, f64, Vec<String>)],
    ) -> Vec<EntityId> {
        let rows = items
            .iter()
            .map(|(x, y, labels)| {
                let descriptors: Vec<Vec<u8>> =
                    labels.iter().map(|l| l.as_bytes().to_vec()).collect();
                UnallocatedRow {
                    view: "s0".to_string(),
                    join: None,
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    x: *x,
                    y: *y,
                    scalars: Vec::new(),
                    scoped: Vec::new(),
                }
            })
            .collect();
        let entities = engine
            .ingest_rows(rows, batch.to_string(), [0u8; 32])
            .expect("ingest is accepted");
        for (entity, (_, _, labels)) in entities.iter().zip(items) {
            self.labels.insert(*entity, labels.clone());
        }
        entities
    }

    /// Flush what is buffered, after which every ingested item has its row.
    fn flush(&mut self, engine: &Engine) {
        publish_buffered(engine);
        self.placed.extend(self.labels.keys().copied());
    }

    fn grow(&mut self, engine: &Engine, layer: &'static str, key: &str, entities: &[EntityId]) {
        engine
            .grow_memberships(
                layer.into(),
                0,
                vec![IncomingGrowth {
                    key: key.to_string(),
                    view: None,
                    joining: entities.iter().map(|e| e.raw() as u32).collect(),
                    leaving: Default::default(),
                    rank: None,
                    parts: Default::default(),
                }],
            )
            .unwrap();
        self.members
            .entry((layer, key.to_string()))
            .or_default()
            .extend(entities);
        tick(engine);
    }

    /// The model's count of `(layer, key)` for `principal`.
    fn modelled(&self, principal: &Principal, layer: &str, key: &str) -> u64 {
        self.members
            .get(&(layer_static(layer), key.to_string()))
            .map_or(0, |members| {
                members
                    .iter()
                    .filter(|e| {
                        self.placed.contains(e)
                            && !self.deleted.contains(e)
                            && !self.suppressed.contains(e)
                            && principal.satisfies(&self.labels[e])
                    })
                    .count() as u64
            })
    }
}

fn layer_static(layer: &str) -> &'static str {
    if layer == FLAT {
        FLAT
    } else {
        LIST
    }
}

fn served(engine: &Engine, session: &Session) -> Vec<ArtifactOut> {
    engine
        .viewport_artifacts(
            session,
            mosaica_engine::ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX)
                .layers(LayerSelection::Named(&[FLAT, LIST])),
        )
        .expect("a viewport naming the layers")
        .artifacts()
}

/// For every principal and both layers, what is served against the model and the walk, through the
/// viewport and the identifier route.
fn check(fx: &Fixture, engine: &Engine, what: &str) {
    let mut served_any = false;
    for principal in fx.corpus.principals() {
        served_any |= check_one(fx, engine, &principal, what);
    }
    assert!(
        served_any,
        "{what}: nothing was served, so nothing was compared"
    );
}

/// [`check`] for one principal, answering whether anything was served to compare.
fn check_one(fx: &Fixture, engine: &Engine, principal: &Principal, what: &str) -> bool {
    let mut served_any = false;
    {
        let session = principal.session(engine);
        let artifacts = served(engine, &session);
        for layer in [FLAT, LIST] {
            let walked = engine
                .walked_figures_for_test(&session, "s0", layer, 0)
                .expect("the walk answers");
            let served: BTreeMap<String, &ArtifactOut> = artifacts
                .iter()
                .filter(|a| a.layer == layer)
                .map(|a| (a.key.clone().expect("every artifact is keyed"), a))
                .collect();
            for (l, key) in fx.members.keys() {
                if *l != layer {
                    continue;
                }
                let at = format!("{what}: {:?} {principal:?} {layer} {key}", fx.corpus);
                let modelled = fx.modelled(principal, layer, key);
                let walked = walked.get(key).copied().unwrap_or((0, None, None));
                assert_eq!(walked.0, modelled, "{at}: the walk and the model disagree");
                match served.get(key) {
                    Some(artifact) => {
                        served_any = true;
                        assert_eq!(artifact.masked_count, modelled, "{at}: the count");
                        assert_eq!(artifact.derived.centroid, walked.1, "{at}: the centroid");
                        assert_eq!(artifact.derived.bbox, walked.2, "{at}: the box");
                        let by_id = engine
                            .artifact(&session, artifact.mosaica_id, "s0", None)
                            .unwrap()
                            .unwrap_or_else(|| panic!("{at}: the identifier route withholds it"));
                        assert_eq!(
                            by_id.masked_count, modelled,
                            "{at}: the count by identifier"
                        );
                        assert_eq!(
                            by_id.derived.centroid, walked.1,
                            "{at}: the centroid by identifier"
                        );
                        assert_eq!(by_id.derived.bbox, walked.2, "{at}: the box by identifier");
                    }
                    None => assert_eq!(modelled, 0, "{at}: withheld though a member is visible"),
                }
            }
        }
    }
    served_any
}

/// The rows of `(layer, key)`'s members sorted by `x`, lowest first, as source ids.
fn by_x(fx: &Fixture, layer: &'static str, key: &str) -> Vec<u64> {
    let mut sources: Vec<u64> = fx
        .entity
        .iter()
        .filter(|(_, e)| fx.members[&(layer, key.to_string())].contains(e))
        .map(|(s, _)| *s)
        .collect();
    sources.sort_by(|a, b| x_of(*a).total_cmp(&x_of(*b)).then(a.cmp(b)));
    sources
}

/// Every corpus and principal, as published and after a fold, then through a suppression on a
/// box's edge, its lift, a deletion, and a suppression the next request must see.
#[test]
fn the_figures_are_the_visible_rows_for_every_corpus_and_principal() {
    for corpus in CORPORA {
        let mut fx = Fixture::new(corpus);
        let engine = fx.open_published();
        check(&fx, &engine, "as published");
        // The fold writes each level's column, after which a level is served from its column
        // alone and its centroids and boxes come from its figures too.
        fold(&engine);
        check(&fx, &engine, "folded");

        // The lowest-x member of an artifact, on its box's edge, under a key some principals hold
        // and others do not.
        let edge = by_x(&fx, FLAT, "f3")[0];
        fx.change(&engine, [edge], ChangeOp::Suppress);
        check(&fx, &engine, "an edge suppressed");
        fx.change(&engine, [edge], ChangeOp::Unsuppress);
        check(&fx, &engine, "the edge's suppression lifted");
        fx.change(
            &engine,
            (0..N).filter(|s| s.is_multiple_of(11)),
            ChangeOp::Delete,
        );
        fx.change(&engine, (0..N).filter(|s| s % 13 == 1), ChangeOp::Suppress);
        check(&fx, &engine, "a scattering deleted and suppressed");
        fx.change(
            &engine,
            (0..N).filter(|s| s % 13 == 1),
            ChangeOp::Unsuppress,
        );
        check(&fx, &engine, "the suppressions lifted");
    }
}

/// **A suppression applies to the first request that starts after it is accepted**, though the
/// fragment's counts were filled before it and are not filled again.
#[test]
fn a_suppression_reaches_the_first_request_after_it_without_a_refill() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    check(&fx, &engine, "before");
    let fills = engine.figures_stats().fills;
    let members: Vec<u64> = by_x(&fx, FLAT, "f5");
    fx.change(&engine, members.iter().copied().take(3), ChangeOp::Suppress);
    let session = engine.authorise_all().unwrap();
    let after = served(&engine, &session);
    let f5 = after
        .iter()
        .find(|a| a.key.as_deref() == Some("f5"))
        .expect("f5 keeps visible members");
    assert_eq!(f5.masked_count, members.len() as u64 - 3);
    check(&fx, &engine, "after the suppression");
    assert_eq!(
        engine.figures_stats().fills,
        fills,
        "a deny is corrected, not refilled"
    );
}

/// **A deny on a box's edge takes the next row from the artifact's reserve; a deny that spends a
/// side's reserve works the box out from the artifact's rows.** Either way the box served is the
/// walk's. The rows are denied from all four sides of the item positions, so a side of the box in
/// grid units takes them whichever way the grid is laid over the map.
#[test]
fn a_denied_edge_is_replaced_from_the_reserve_until_it_is_spent() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    // The fold writes each level's column, after which a level is served from its column alone
    // and its centroids and boxes come from its figures.
    fold(&engine);
    check(&fx, &engine, "before");
    let members = by_x(&fx, FLAT, "f7");
    assert!(members.len() > 40);
    let extremes = |n: usize| -> Vec<u64> {
        let mut by_y = members.clone();
        by_y.sort_by(|a, b| y_of(*a).total_cmp(&y_of(*b)).then(a.cmp(b)));
        let mut out: BTreeSet<u64> = BTreeSet::new();
        for sorted in [&members, &by_y] {
            out.extend(sorted.iter().take(n));
            out.extend(sorted.iter().rev().take(n));
        }
        out.into_iter().collect()
    };

    let spent = engine.figures_stats().reserve_spent;
    fx.change(&engine, extremes(1), ChangeOp::Suppress);
    check(&fx, &engine, "each side's edge row suppressed");
    assert_eq!(
        engine.figures_stats().reserve_spent,
        spent,
        "one row off each edge is answered from the reserve"
    );

    fx.change(&engine, extremes(10), ChangeOp::Suppress);
    check(&fx, &engine, "ten rows off each edge suppressed");
    assert!(
        engine.figures_stats().reserve_spent > spent,
        "ten rows off an edge spend a reserve of eight"
    );
}

/// **Ingest, flush, growth and a fold**: items buffered and not yet placed count for nobody, a
/// flush places them above the base, a growth adds both base and flushed rows to artifacts, and a
/// fold takes them all into the base. A growth after a deny, a fold with suppressions standing.
#[test]
fn ingest_growth_and_a_fold_under_standing_denies() {
    for corpus in [Corpus::MultiKey, Corpus::Conjunction] {
        let mut fx = Fixture::new(corpus);
        let engine = fx.open_published();
        fx.change(&engine, (0..N).filter(|s| s % 9 == 4), ChangeOp::Suppress);
        check(&fx, &engine, "suppressed");

        let items: Vec<(f64, f64, Vec<String>)> = (0..60u64)
            .map(|i| {
                (
                    x_of(i * 7 + 3) + 0.5,
                    y_of(i * 11 + 1) + 0.5,
                    corpus.labels(i * 13 + 2),
                )
            })
            .collect();
        let fresh = fx.ingest(&engine, "fresh", &items);
        check(&fx, &engine, "ingested, not flushed");
        fx.flush(&engine);
        check(&fx, &engine, "flushed above the base");

        // A growth naming flushed rows, base rows and denied base rows.
        let base: Vec<EntityId> = (0..N)
            .filter(|s| s % 9 == 4 || s % 17 == 0)
            .map(|s| fx.entity[&s])
            .collect();
        fx.grow(&engine, LIST, "l3", &fresh[..30]);
        fx.grow(&engine, LIST, "l4", &base);
        check(&fx, &engine, "grown after the deny");

        fold(&engine);
        check(&fx, &engine, "folded with suppressions standing");
        fx.change(&engine, (0..N).filter(|s| s % 9 == 4), ChangeOp::Unsuppress);
        check(&fx, &engine, "lifted after the fold");
    }
}

/// **An ingest window does not refill a fragment's counts**: flushes and growths between folds are
/// followed in place, and every request is still the walk.
#[test]
fn an_ingest_window_does_not_refill_the_fragments_counts() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    fold(&engine);
    check(&fx, &engine, "before the window");
    let fills = engine.figures_stats().fills;
    for window in 0..3u64 {
        let items: Vec<(f64, f64, Vec<String>)> = (0..20u64)
            .map(|i| {
                (
                    x_of(i * 3 + window) + 0.25,
                    y_of(i + window) + 0.25,
                    vec![format!("k{}", i % 4)],
                )
            })
            .collect();
        let fresh = fx.ingest(&engine, &format!("window-{window}"), &items);
        fx.flush(&engine);
        let base: Vec<EntityId> = (0..N)
            .filter(|s| s % 23 == window)
            .map(|s| fx.entity[&s])
            .collect();
        fx.grow(&engine, LIST, "l1", &base);
        fx.grow(&engine, LIST, "l2", &fresh);
        check(&fx, &engine, &format!("window {window}"));
    }
    assert_eq!(
        engine.figures_stats().fills,
        fills,
        "flushes and growths are followed in place, not walked again"
    );
    assert_eq!(
        engine.figures_stats().exact,
        0,
        "no request walked its whole mask"
    );
}

/// **A read-all session after a flush that promotes a new key**: the item flushed under it is in
/// that session's figures, and the counts over the base are not walked again.
#[test]
fn a_flush_promoting_a_key_reaches_a_read_all_session() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    check(&fx, &engine, "before");
    let fills = engine.figures_stats().fills;
    let fresh = fx.ingest(
        &engine,
        "promoting",
        &[
            (500.5, 500.5, vec!["brand-new".to_string()]),
            (12.5, 900.5, vec!["brand-new".to_string()]),
        ],
    );
    fx.flush(&engine);
    fx.grow(&engine, FLAT, "f1", &fresh);
    check(&fx, &engine, "a new key promoted");
    let session = engine.authorise_all().unwrap();
    let f1 = served(&engine, &session)
        .into_iter()
        .find(|a| a.key.as_deref() == Some("f1"))
        .unwrap();
    assert_eq!(
        f1.masked_count,
        fx.modelled(&Principal::ReadAll, FLAT, "f1")
    );
    assert_eq!(
        engine.figures_stats().fills,
        fills,
        "the base was not walked again"
    );
}

/// **A projection one generation behind**: with the background refresh off, a flush leaves a
/// session served the projection it already has, and its figures are still the walk of that mask.
#[test]
fn a_projection_one_generation_behind_is_corrected_over_its_own_rows() {
    let mut fx = Fixture::new(Corpus::MultiKey);
    let engine = fx.open_published();
    let base: Vec<EntityId> = (0..N)
        .filter(|s| s % 19 == 3)
        .map(|s| fx.entity[&s])
        .collect();
    fx.grow(&engine, LIST, "l2", &base);
    fx.change(&engine, (0..N).filter(|s| s % 8 == 1), ChangeOp::Suppress);
    let session = Principal::Terms(vec!["k2".into(), "k5".into()]).session(&engine);
    served(&engine, &session);
    fx.ingest(&engine, "behind", &[(250.5, 250.5, vec!["k2".to_string()])]);
    fx.flush(&engine);
    let stale_before = engine.stale_serves();
    let artifacts = served(&engine, &session);
    assert!(
        engine.stale_serves() > stale_before,
        "the session was served its previous projection"
    );
    for layer in [FLAT, LIST] {
        let walked = engine
            .walked_figures_for_test(&session, "s0", layer, 0)
            .unwrap();
        let mut compared = 0;
        for artifact in artifacts.iter().filter(|a| a.layer == layer) {
            let key = artifact.key.clone().unwrap();
            let (count, centroid, bbox) = walked[&key];
            assert_eq!(
                (
                    artifact.masked_count,
                    artifact.derived.centroid,
                    artifact.derived.bbox
                ),
                (count, centroid, bbox),
                "{layer} {key}"
            );
            compared += 1;
        }
        assert!(compared > 0, "{layer}: nothing served to compare");
    }
}

/// **A restart reads the fragments' counts and the denied rows' labels back** rather than walking
/// them, and serves what it served before.
#[test]
fn a_restart_reads_the_counts_and_the_denied_labels_back() {
    let mut fx = Fixture::new(Corpus::PerDocument);
    {
        let engine = fx.open_published();
        fold(&engine);
        fx.change(
            &engine,
            (0..N).filter(|s| s.is_multiple_of(6)),
            ChangeOp::Suppress,
        );
        check(&fx, &engine, "before the restart");
        // The writes are off the request path; they are all on disk once the worker has run them.
        engine.figures_settled_for_test();
        let dir = fx.cache.join(mosaica_engine::figures::FIGURES_DIR);
        assert!(files_under(&dir, "counts") >= 2 * fx.corpus.principals().len());
        assert!(files_under(&dir, "denied") >= 2);
    }
    let engine = fx.open();
    check(&fx, &engine, "after the restart");
    let stats = engine.figures_stats();
    assert_eq!(stats.fills, 0, "nothing was walked again");
    assert!(stats.loads > 0, "the counts were read back");
    assert_eq!(stats.exact, 0, "no request walked its whole mask");
}

fn files_under(dir: &Path, extension: &str) -> usize {
    let mut found = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found += files_under(&path, extension);
        } else if path.extension().is_some_and(|e| e == extension) {
            found += 1;
        }
    }
    found
}

/// **A level published after a deny** is corrected for the deny from its first request, and a
/// growth after it brings the labels of the denied rows forward.
#[test]
fn a_level_published_after_a_deny_is_corrected_from_its_first_request() {
    let mut fx = Fixture::new(Corpus::Conjunction);
    let engine = fx.open();
    fx.change(&engine, (0..N).filter(|s| s % 5 == 2), ChangeOp::Suppress);
    for (layer, layout) in [
        (FLAT, ServingLayout::RowMajorLabel),
        (LIST, ServingLayout::RowMajorList),
    ] {
        engine.register_layer(declaration(layer, layout)).unwrap();
        let artifacts = fx
            .members
            .iter()
            .filter(|((l, _), _)| *l == layer)
            .map(|((_, key), members)| {
                IncomingArtifact::from_entities(Some(key.clone()), members.iter().copied())
            })
            .collect();
        engine
            .publish_artifacts(layer.into(), 0, artifacts)
            .unwrap();
    }
    tick(&engine);
    check(&fx, &engine, "published after the deny");
    let denied: Vec<EntityId> = (0..N)
        .filter(|s| s % 5 == 2 && s % 3 == 0)
        .map(|s| fx.entity[&s])
        .collect();
    fx.grow(&engine, LIST, "l6", &denied);
    check(&fx, &engine, "grown over denied rows");
}

/// **A deny after a level's labels are held brings them forward by the new rows alone**: at the
/// deny, on the figures' thread, reading from the column only the rows the deny added.
#[test]
fn a_deny_brings_held_labels_forward_by_its_new_rows_alone() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    fold(&engine);
    let first: Vec<u64> = (0..N).filter(|s| s % 12 == 5).collect();
    fx.change(&engine, first.iter().copied(), ChangeOp::Suppress);
    check(&fx, &engine, "a first deny");
    engine.figures_settled_for_test();
    let read = engine.figures_stats().labels_rows_read;
    assert_eq!(
        read,
        2 * first.len() as u64,
        "each level read the first deny's rows once"
    );

    let second: Vec<u64> = (0..N).filter(|s| s % 12 == 7).collect();
    fx.change(&engine, second.iter().copied(), ChangeOp::Suppress);
    let wanted = read + 2 * second.len() as u64;
    wait_until(
        "the deny brought the held labels forward",
        std::time::Duration::from_secs(30),
        || engine.figures_stats().labels_rows_read >= wanted,
    );
    check(&fx, &engine, "a second deny");
    engine.figures_settled_for_test();
    assert_eq!(
        engine.figures_stats().labels_rows_read,
        wanted,
        "the second deny read its own rows and none the first had"
    );
    assert_eq!(
        engine.figures_stats().exact,
        0,
        "no request walked its whole mask"
    );
}

/// **A level whose reserve is larger than the bound keeps its counts**: the reserve is not kept,
/// the counts are, and a box a deny reaches is worked out from the artifact's rows.
#[test]
fn counts_are_kept_where_their_reserve_is_too_large_for_the_bound() {
    let mut fx = Fixture::new(Corpus::SingleKey);
    let engine = fx.open_published();
    fold(&engine);
    // Above the two levels' counts and sums, below the flat level's reserve.
    engine.set_masked_count_cache_bytes(2_000);
    let reader = Principal::ReadAll;
    assert!(check_one(&fx, &engine, &reader, "under a small bound"));
    let fills = engine.figures_stats().fills;
    assert!(
        engine.figures_stats().not_admitted > 0,
        "the flat level's reserve was not kept"
    );
    assert!(check_one(&fx, &engine, &reader, "again"));
    assert_eq!(
        engine.figures_stats().fills,
        fills,
        "the counts were kept and read again"
    );
    let edge = by_x(&fx, FLAT, "f3");
    fx.change(&engine, edge.iter().copied().take(3), ChangeOp::Suppress);
    assert!(check_one(
        &fx,
        &engine,
        &reader,
        "an edge denied without a reserve"
    ));
}

/// **The figures' directory stays under its byte bound**, the files least recently written or
/// read removed first.
#[test]
fn the_figures_directory_is_held_under_its_bound() {
    let fx = Fixture::new(Corpus::PerDocument);
    let engine = fx.open_published();
    fold(&engine);
    check(&fx, &engine, "every principal's counts written");
    engine.figures_settled_for_test();
    let unbounded = engine.figures_stats().disk_bytes;
    assert!(unbounded > 0);
    engine.set_figures_disk_bytes(unbounded / 2);
    engine.figures_settled_for_test();
    let held = engine.figures_stats().disk_bytes;
    assert!(
        held > 0 && held <= unbounded / 2,
        "{held} bytes held under a bound of {}",
        unbounded / 2
    );
    check(&fx, &engine, "after the bound");
    engine.figures_settled_for_test();
    assert!(engine.figures_stats().disk_bytes <= unbounded / 2);
}
