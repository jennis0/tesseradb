//! Cluster slots: each cluster a viewer is served carries a palette slot, fixed for that viewer,
//! chosen over the viewer's whole visible tree.
//!
//! The fixture is a grid of items whose density rises towards one corner, and a quadtree over it:
//! a root, four quadrants, sixteen blocks, sixty-four blocks and 256 leaves, each the items of its
//! square. Its oracle is the grid itself: each node's members and centre are computed here.

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Array, Float64Array, UInt64Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_engine::filter::{FilterExpr, RegionLeaf};
use tessera_engine::{
    AggregateCaps, AggregateHead, AggregateRequest, AggregateSink, ArtifactsRequest, By,
    CancelToken, Cut, Engine, EngineError, Grouping, LayerSelection, PageEnd, Pick, RecordsHead,
    RecordsLimits, RecordsSink, Session, SinkResult, TableHead, ViewportArtifactsRequest,
};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_spatial::shape::{ShapeF64, Space};
use tessera_types::layer::{
    ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource,
};
use tessera_types::TesseraId;

const SIDE: u64 = 64;
const CELL: f64 = 1000.0 / SIDE as f64;
const TREE: &str = "clusters/tree";
const FLAT: &str = "clusters/flat";

/// Whether grid cell `(x, y)` holds an item: about a third of the cells at `(0, 0)`, rising to
/// every cell at the far corner.
fn holds(x: u64, y: u64) -> bool {
    let scattered = (x * 2_654_435_761 + y * 40_503) % 4_294_967_291 % 100;
    scattered < 35 + 65 * (x + y) / (2 * SIDE - 2)
}

fn position(x: u64, y: u64) -> (f64, f64) {
    ((x as f64 + 0.5) * CELL, (y as f64 + 0.5) * CELL)
}

/// A node of the quadtree: its key, its parent's, its members' source ids, and the label it
/// carries.
#[derive(Debug, Clone)]
struct Node {
    key: String,
    parent: Option<String>,
    members: Vec<u64>,
    label: Option<&'static str>,
    centre: (f64, f64),
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    engine: Engine,
    /// Each item's grid cell, by source id.
    cells: Vec<(u64, u64)>,
}

fn build(dir: &Path, cells: &[(u64, u64)]) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(0..cells.len() as u64)),
            Arc::new(Float64Array::from_iter_values(
                cells.iter().map(|&(x, y)| position(x, y).0),
            )),
            Arc::new(Float64Array::from_iter_values(
                cells.iter().map(|&(x, y)| position(x, y).1),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    write_pairs_n(&pairs, cells.len() as u64);
    let schema = id_schema();
    let out = dir.join("bundle");
    tessera_build::build(&tessera_build::BuildArgs {
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

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let cells: Vec<(u64, u64)> = (0..SIDE)
        .flat_map(|y| (0..SIDE).map(move |x| (x, y)))
        .filter(|&(x, y)| holds(x, y))
        .collect();
    let root = build(tmp.path(), &cells);
    let engine = engine_at(tmp.path(), &root, 3600);
    engine.set_background_refresh_for_test(false);
    Fx {
        _tmp: tmp,
        root,
        engine,
        cells,
    }
}

impl Fx {
    /// The quadtree, coarsest first, with `labelled` carrying a label no viewer holds.
    fn tree(&self, labelled: &[&str]) -> Vec<Node> {
        let mut nodes = Vec::new();
        for depth in 0..5u32 {
            let blocks = 1u64 << depth;
            let size = SIDE / blocks;
            for by in 0..blocks {
                for bx in 0..blocks {
                    let key = format!("b{depth}-{bx}-{by}");
                    let members: Vec<u64> = (0..self.cells.len() as u64)
                        .filter(|&s| {
                            let (x, y) = self.cells[s as usize];
                            x / size == bx && y / size == by
                        })
                        .collect();
                    assert!(!members.is_empty(), "{key} holds an item");
                    let n = members.len() as f64;
                    let sum = members.iter().fold((0.0, 0.0), |(a, b), &s| {
                        let (x, y) = self.cells[s as usize];
                        let (px, py) = position(x, y);
                        (a + px, b + py)
                    });
                    nodes.push(Node {
                        parent: (depth > 0)
                            .then(|| format!("b{}-{}-{}", depth - 1, bx / 2, by / 2)),
                        label: labelled.contains(&key.as_str()).then_some("7"),
                        key,
                        members,
                        centre: (sum.0 / n, sum.1 / n),
                    });
                }
            }
        }
        nodes
    }

    /// Register `name` over `nodes`, nested or flat, and answer each node's id by key.
    fn plant(&self, name: &str, kind: HierarchyKind, nodes: &[Node]) -> BTreeMap<String, TesseraId> {
        self.plant_as(name, kind, nodes, None, Vec::new())
    }

    /// [`Self::plant`] with a layout and computed properties of the caller's.
    fn plant_as(
        &self,
        name: &str,
        kind: HierarchyKind,
        nodes: &[Node],
        layout: Option<tessera_types::layer::ServingLayout>,
        computed: Vec<String>,
    ) -> BTreeMap<String, TesseraId> {
        self.engine
            .register_layer(LayerDeclaration {
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
                    kind,
                    prune_children: true,
                },
                content: ContentDeclaration {
                    computed,
                    ..ContentDeclaration::default()
                },
                depends_on: Vec::new(),
                levels: Vec::new(),
                layout,
                shape: None,
            })
            .unwrap();
        let map = source_to_new_map(&self.root, "v00000");
        let artifacts = nodes
            .iter()
            .map(|node| {
                let mut artifact = IncomingArtifact::from_entities(
                    Some(node.key.clone()),
                    node.members
                        .iter()
                        .map(|s| tessera_types::EntityId::new(map[s]))
                        .collect::<Vec<_>>(),
                );
                artifact.access = Some(
                    node.label
                        .map_or_else(Vec::new, |l| vec![l.as_bytes().to_vec()]),
                );
                if kind == HierarchyKind::Nested {
                    artifact.parent_keys = node.parent.iter().cloned().collect();
                }
                artifact
            })
            .collect();
        let ids = self
            .engine
            .publish_artifacts(name.into(), 0, artifacts)
            .unwrap();
        tick(&self.engine);
        nodes.iter().map(|n| n.key.clone()).zip(ids).collect()
    }

    fn session(&self, broad: bool) -> Session {
        let credential = if broad {
            full_coverage_credential()
        } else {
            subset_credential()
        };
        self.engine.authorise(&credential).unwrap()
    }
}

/// Each artifact's slot over every frame of a viewport, checking an artifact served twice carries
/// one slot.
fn viewport_slots(
    engine: &Engine,
    session: &Session,
    request: ViewportArtifactsRequest<'_>,
) -> BTreeMap<u64, Option<u8>> {
    let out = engine.viewport_artifacts(session, request).unwrap();
    let mut slots = BTreeMap::new();
    for artifact in out.frames.iter().flat_map(|frame| &frame.artifacts) {
        let held = slots.insert(artifact.tessera_id.raw(), artifact.slot);
        assert!(held.is_none_or(|held| held == artifact.slot));
    }
    slots
}

const ONLY_TREE: &[&str] = &[TREE];
const BOTH: &[&str] = &[TREE, FLAT];

/// The finest cut of the whole map over `names`.
fn whole<'a>(names: &'a [&'a str], palette: Option<u8>) -> ViewportArtifactsRequest<'a> {
    ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX)
        .layers(LayerSelection::Named(names))
        .palette_size(palette)
}

/// Every served node of `layer` with its slot for a palette of `palette`, as `/v1/artifacts`
/// reads them.
fn read_slots(
    engine: &Engine,
    session: &Session,
    layer: &str,
    palette: u8,
) -> BTreeMap<u64, Option<u8>> {
    let fields = ["slot".to_string()];
    let mut pages = Pages::default();
    engine
        .artifacts_stream(
            session,
            ArtifactsRequest {
                view: "s0",
                layer,
                level: None,
                parent: None,
                q: None,
                ids: None,
                filter: None,
                keep_unmatched: false,
                count: false,
                fields: &fields,
                palette_size: Some(palette),
                page_rows: None,
                pages: None,
                cursor: None,
                limits: limits(),
                cancel: None,
            },
            &mut pages,
        )
        .unwrap();
    id_slots(&pages.pages, "tessera_id").into_iter().collect()
}

/// Every served tree node's slot for a palette of ten.
fn slots_of(engine: &Engine, session: &Session) -> BTreeMap<u64, Option<u8>> {
    read_slots(engine, session, TREE, 10)
}

#[derive(Default)]
struct Tables {
    pages: Vec<RecordBatch>,
}

impl AggregateSink for Tables {
    fn head(&mut self, _: &AggregateHead) -> SinkResult {
        Ok(())
    }
    fn table(&mut self, _: &TableHead) -> SinkResult {
        Ok(())
    }
    fn page(&mut self, _: u32, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.pages.push(batch.clone());
        Ok(())
    }
}

#[derive(Default)]
struct Pages {
    pages: Vec<RecordBatch>,
}

impl RecordsSink for Pages {
    fn head(&mut self, _: &RecordsHead) -> SinkResult {
        Ok(())
    }
    fn page(&mut self, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.pages.push(batch.clone());
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

/// `(id, slot)` per row of `batches`, from the `id_column` and `slot` columns.
fn id_slots(batches: &[RecordBatch], id_column: &str) -> Vec<(u64, Option<u8>)> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name(id_column)
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let slots = batch
            .column_by_name("slot")
            .expect("a slot column")
            .as_any()
            .downcast_ref::<UInt8Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            if ids.is_valid(i) {
                out.push((ids.value(i), slots.is_valid(i).then(|| slots.value(i))));
            }
        }
    }
    out
}

fn aggregate_slots(
    engine: &Engine,
    session: &Session,
    by: By,
    filter: Option<FilterExpr>,
) -> Vec<(u64, Option<u8>)> {
    let groupings = [Grouping {
        by: Some(by),
        cells: None,
        area: None,
    }];
    let mut sink = Tables::default();
    engine
        .aggregate_stream(
            session,
            AggregateRequest {
                view: "s0",
                filter,
                reference: None,
                groupings: &groupings,
                page_rows: None,
                pages: None,
                cursor: None,
                limits: limits(),
                caps: AggregateCaps {
                    groupings: 8,
                    top: 1000,
                    named: 1000,
                    bins: 100,
                    cells: u64::MAX,
                },
                cancel: None::<CancelToken>,
            },
            &mut sink,
        )
        .unwrap();
    id_slots(&sink.pages, "key")
}

fn box_filter(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> FilterExpr {
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

/// **A slot is the viewer's, not the request's**: every artifact a nested layer and a flat one
/// serve carries the same slot at any zoom, box, budget or filter, in the treed frame and in every
/// tile frame, and the aggregate's layer rows, browse rows and `/v1/artifacts` rows carry the slot
/// the viewport does. Without a palette size there is none.
#[test]
fn a_slot_is_the_same_at_any_zoom_box_budget_or_filter() {
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let leaves: Vec<Node> = nodes.iter().filter(|n| n.key.starts_with("b4-")).cloned().collect();
    fx.plant(FLAT, HierarchyKind::Flat, &leaves);
    for broad in [true, false] {
        let session = fx.session(broad);
        let served = |node: &&Node| node.members.iter().any(|&s| broad || subset_sees(s));
        let tree_served = nodes.iter().filter(served).count();
        let leaves_served = leaves.iter().filter(served).count();
        let mut held: BTreeMap<u64, u8> = BTreeMap::new();
        let mut agree = |slots: &BTreeMap<u64, Option<u8>>, what: &str| {
            for (&id, &slot) in slots {
                let slot = slot.unwrap_or_else(|| panic!("{what}: {id} has no slot"));
                assert!(slot < 10, "{what}");
                let first = *held.entry(id).or_insert(slot);
                assert_eq!(first, slot, "{what}: {id}");
            }
        };
        let filters = [None, Some(box_filter(0.0, 0.0, 400.0, 1000.0))];
        for (zoom, bbox, budget) in [
            (0u8, WHOLE_MAP, None),
            (0, WHOLE_MAP, Some(1)),
            (0, WHOLE_MAP, Some(5)),
            (1, WHOLE_MAP, Some(20)),
            (1, WHOLE_MAP, Some(100)),
            (3, [100.0, 200.0, 450.0, 600.0], None),
            (5, [620.0, 610.0, 700.0, 690.0], Some(3)),
        ] {
            for filter in &filters {
                let mut request = ViewportArtifactsRequest::new("s0", zoom, bbox, 1000)
                    .layers(LayerSelection::Named(BOTH))
                    .budget(budget)
                    .palette_size(Some(10));
                if let Some(filter) = filter {
                    request = request.filter(filter.clone());
                }
                let slots = viewport_slots(engine, &session, request);
                assert!(!slots.is_empty());
                agree(&slots, &format!("zoom {zoom}, {bbox:?}, budget {budget:?}"));
            }
        }
        // Every tree node and every flat leaf this viewer is served has been seen.
        assert_eq!(held.len(), tree_served + leaves_served, "broad {broad}");

        let cut = Cut {
            zoom: 2,
            bbox: WHOLE_MAP,
            budget: Some(30),
        };
        for filter in filters {
            let rows = aggregate_slots(
                engine,
                &session,
                By::Layer {
                    layer: TREE.into(),
                    level: None,
                    pick: Pick::Top(1000),
                    cut: Some(cut),
                    palette: Some(10),
                },
                filter,
            );
            assert!(!rows.is_empty());
            for (id, slot) in rows {
                assert_eq!(slot, Some(held[&id]), "aggregate row {id}");
            }
        }
        let flat = aggregate_slots(
            engine,
            &session,
            By::Layer {
                layer: FLAT.into(),
                level: None,
                pick: Pick::Top(1000),
                cut: None,
                palette: Some(10),
            },
            None,
        );
        assert_eq!(flat.len(), leaves_served);
        for (id, slot) in flat {
            assert_eq!(slot, Some(held[&id]), "flat aggregate row {id}");
        }

        let browse = |q: Option<String>, palette_size| {
            engine
                .browse(
                    &session,
                    tessera_engine::browse::BrowseRequest {
                        view: "s0",
                        layer: TREE,
                        level: None,
                        form: match q {
                            Some(q) => tessera_engine::browse::BrowseForm::Search(q),
                            None => tessera_engine::browse::BrowseForm::Roots,
                        },
                        filter: None,
                        limit: 1000,
                        cursor: None,
                        palette_size,
                    },
                )
                .unwrap()
                .artifacts
        };
        let found = browse(Some("b3-".into()), Some(10));
        assert_eq!(
            found.len(),
            nodes.iter().filter(served).filter(|n| n.key.starts_with("b3-")).count()
        );
        for row in found {
            assert_eq!(row.slot, Some(held[&row.tessera_id.raw()]), "browse row");
        }
        assert!(browse(None, None).iter().all(|row| row.slot.is_none()));

        // A node with no member this viewer sees is listed, draws nothing and has no slot.
        let rows = read_slots(engine, &session, TREE, 10);
        assert_eq!(rows.values().filter(|s| s.is_some()).count(), tree_served);
        for (id, slot) in rows {
            assert_eq!(slot, held.get(&id).copied(), "artifacts row {id}");
        }

        let unasked = viewport_slots(engine, &session, whole(BOTH, None));
        assert!(unasked.values().all(Option::is_none));
    }
}

/// **Neighbours mostly differ**: across the tree's depths, two clusters whose squares touch share
/// a slot rarely, for every palette size from eight up.
#[test]
fn touching_clusters_rarely_share_a_slot() {
    let fx = fixture();
    let nodes = fx.tree(&[]);
    let ids = fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    for palette in [8u8, 10, 20, 22] {
        let slots = read_slots(&fx.engine, &session, TREE, palette);
        let (mut pairs, mut shared) = (0, 0);
        for depth in 1..5u32 {
            let blocks = 1i64 << depth;
            let slot = |bx: i64, by: i64| slots[&ids[&format!("b{depth}-{bx}-{by}")].raw()];
            for by in 0..blocks {
                for bx in 0..blocks {
                    for (dx, dy) in [(1, 0), (0, 1)] {
                        if bx + dx < blocks && by + dy < blocks {
                            pairs += 1;
                            shared += u32::from(slot(bx, by) == slot(bx + dx, by + dy));
                        }
                    }
                }
            }
        }
        assert!(
            f64::from(shared) / f64::from(pairs) < 0.05,
            "{shared} of {pairs} touching pairs share a slot at {palette}"
        );
    }
}

/// **A hidden cluster is no input**: a viewer who is not served a node, a leaf or a block with
/// children, has the slots they would have where it does not exist, which a suppression of it
/// makes the case.
#[test]
fn hiding_a_cluster_gives_the_slots_of_a_corpus_without_it() {
    let fx = fixture();
    let engine = &fx.engine;
    let hidden = ["b4-9-6", "b2-1-2"];
    let nodes = fx.tree(&hidden);
    let ids = fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    let before = slots_of(engine, &session);
    for key in hidden {
        assert!(!before.contains_key(&ids[key].raw()), "{key} is not served");
    }
    for key in hidden {
        let entity = artifact_entity(engine, ids[key]);
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    }
    let after = slots_of(engine, &session);
    assert_eq!(after, before);
}

/// **A suppression moves only the slots near it**: a cluster suppressed at the running service
/// leaves every cluster farther than two of its widths with the slot it had.
#[test]
fn after_a_suppression_only_nearby_clusters_change() {
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    let ids = fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    let before = slots_of(engine, &session);
    let gone = nodes.iter().find(|n| n.key == "b4-7-8").unwrap();
    engine
        .accept_change(artifact_entity(engine, ids[&gone.key]), ChangeOp::Suppress)
        .unwrap();
    let after = slots_of(engine, &session);
    let width = 1000.0 / 16.0;
    let (mut far, mut moved) = (0, 0);
    for node in &nodes {
        let id = ids[&node.key].raw();
        if node.key == gone.key {
            assert!(!after.contains_key(&id));
            continue;
        }
        let distance = (node.centre.0 - gone.centre.0).hypot(node.centre.1 - gone.centre.1);
        if before[&id] != after[&id] {
            moved += 1;
            assert!(
                distance < 2.5 * width,
                "{} moved, {distance:.0} from the suppressed cluster",
                node.key
            );
        } else if distance >= 2.5 * width {
            far += 1;
        }
    }
    assert!(far > nodes.len() / 2, "{far} far clusters, {moved} moved");
}

/// A palette size outside 2 to 32 is refused.
#[test]
fn a_palette_size_outside_two_to_thirty_two_is_refused() {
    let fx = fixture();
    let nodes = fx.tree(&[]);
    fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    for size in [0u32, 1, 33, 300] {
        assert!(matches!(
            tessera_engine::check_palette_size(size),
            Err(EngineError::PaletteRefused(s)) if s == size
        ));
    }
    for size in [0u8, 1, 33] {
        let refused = fx
            .engine
            .viewport_artifacts(&session, whole(ONLY_TREE, Some(size)))
            .unwrap_err();
        assert!(matches!(refused, EngineError::PaletteRefused(_)), "{size}");
    }
}

/// **Every kind is coloured level by level**: a stacked layer's levels and a tiered layer's each
/// carry a slot per artifact, and on the tiered layer most parents' heirs, their children with the
/// most visible items, keep the parent's slot.
#[test]
fn stacked_and_tiered_layers_are_coloured_level_by_level() {
    use tessera_types::layer::LevelDeclaration;
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    let map = source_to_new_map(&fx.root, "v00000");
    for kind in [HierarchyKind::Stacked, HierarchyKind::Tiered] {
        let name = format!("levels/{kind:?}");
        engine
            .register_layer(LayerDeclaration {
                scope: Default::default(),
                name: name.clone(),
                title: None,
                views: vec!["s0".into()],
                membership: MembershipSource::Enumerated,
                value_set: Default::default(),
                visibility: None,
                artifact_visibility: ArtifactVisibility::inherited(),
                require_member_visibility: None,
                hierarchy: Hierarchy {
                    kind,
                    prune_children: true,
                },
                content: ContentDeclaration::default(),
                depends_on: Vec::new(),
                levels: (0..3)
                    .map(|level| LevelDeclaration {
                        level,
                        title: None,
                        zoom: None,
                    })
                    .collect(),
                layout: None,
                shape: None,
            })
            .unwrap();
        let mut ids: BTreeMap<String, TesseraId> = BTreeMap::new();
        for level in 0..3u32 {
            let at: Vec<&Node> = nodes
                .iter()
                .filter(|n| n.key.starts_with(&format!("b{}-", level + 1)))
                .collect();
            let artifacts = at
                .iter()
                .map(|node| {
                    let mut artifact = IncomingArtifact::from_entities(
                        Some(node.key.clone()),
                        node.members
                            .iter()
                            .map(|s| tessera_types::EntityId::new(map[s]))
                            .collect::<Vec<_>>(),
                    );
                    if kind == HierarchyKind::Tiered && level > 0 {
                        artifact.parent_keys = node.parent.iter().cloned().collect();
                    }
                    artifact
                })
                .collect();
            let published = engine
                .publish_artifacts(name.clone(), level, artifacts)
                .unwrap();
            ids.extend(at.iter().map(|n| n.key.clone()).zip(published));
        }
        tick(engine);
        let session = fx.session(true);
        let slots = read_slots(engine, &session, &name, 10);
        assert_eq!(slots.len(), 4 + 16 + 64, "{name}");
        assert!(
            slots.values().all(|s| s.is_some_and(|s| s < 10)),
            "{name}"
        );
        if kind == HierarchyKind::Tiered {
            let slot = |key: &str| slots[&ids[key].raw()];
            let (mut heirs, mut kept) = (0, 0);
            let parents = nodes
                .iter()
                .filter(|n| n.key.starts_with("b1-") || n.key.starts_with("b2-"));
            for parent in parents {
                let heir = nodes
                    .iter()
                    .filter(|n| n.parent.as_deref() == Some(parent.key.as_str()))
                    .max_by(|a, b| {
                        a.members
                            .len()
                            .cmp(&b.members.len())
                            .then(ids[&b.key].raw().cmp(&ids[&a.key].raw()))
                    })
                    .expect("a child");
                heirs += 1;
                kept += u32::from(slot(&heir.key) == slot(&parent.key));
            }
            assert!(
                kept * 2 > heirs,
                "{kept} of {heirs} heirs keep their parent's slot"
            );
        }
    }
}

/// **Hidden members are no input either**: a viewer denied some items has the slots the broad
/// viewer has once those items are suppressed.
#[test]
fn a_viewer_denied_items_has_the_slots_of_a_corpus_without_them() {
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let narrow = slots_of(engine, &fx.session(false));
    for s in (0..fx.cells.len() as u64).filter(|&s| !subset_sees(s)) {
        let entity = item_of_id(engine, s).unwrap().expect("a built item");
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    }
    let broad = slots_of(engine, &fx.session(true));
    let served = |slots: &BTreeMap<u64, Option<u8>>| -> BTreeMap<u64, u8> {
        slots
            .iter()
            .filter_map(|(&id, &slot)| Some((id, slot?)))
            .collect()
    };
    assert!(!served(&narrow).is_empty());
    assert_eq!(served(&broad), served(&narrow));
}

/// **A small change in the visible count moves few items in or out of the sample**: the cut
/// follows the visible count with no step, so ten items suppressed where a cut at powers of two
/// would double the sample change it by a few items, and move few slots.
#[test]
fn a_small_change_in_the_visible_count_moves_few_items_in_or_out_of_the_sample() {
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    let visible = fx.cells.len() as u64;
    // The visible count sits just above four times the sample.
    engine.set_slot_sample_for_test(visible / 4 - 2);
    let stats = || engine.cluster_slot_stats(&session, "s0", TREE, 10).unwrap();
    let before = (stats(), slots_of(engine, &session));
    for s in 0..10u64 {
        let entity = item_of_id(engine, s).unwrap().expect("a built item");
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    }
    let after = (stats(), slots_of(engine, &session));
    let (was, now) = (before.0.sampled_items, after.0.sampled_items);
    assert!(now.abs_diff(was) <= 20, "{was} items sampled, then {now}");
    let moved = before
        .1
        .iter()
        .filter(|&(id, slot)| after.1.get(id).is_some_and(|now| now != slot))
        .count();
    assert!(moved * 10 < before.1.len(), "{moved} of {} slots moved", before.1.len());
}

/// **An ingest is answered from the slots held, and they are rebuilt after the response**; a
/// suppression is answered from slots built over it.
#[test]
fn an_ingest_is_answered_from_the_slots_held_until_they_are_rebuilt() {
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    let ids = fx.plant(TREE, HierarchyKind::Nested, &nodes);
    let session = fx.session(true);
    let before = slots_of(engine, &session);
    assert!(!engine.cluster_slots_stale(&session));

    ingest(engine, "slots-ingest");
    tick(engine);
    assert_eq!(slots_of(engine, &session), before, "the slots held answer");
    assert!(engine.cluster_slots_stale(&session), "a rebuild is left for later");
    engine.refresh_cluster_slots(&session).unwrap();
    assert!(!engine.cluster_slots_stale(&session));
    let rebuilt = slots_of(engine, &session);
    assert!(!engine.cluster_slots_stale(&session), "the rebuilt slots are current");

    let leaf = ids["b4-3-3"];
    engine
        .accept_change(artifact_entity(engine, leaf), ChangeOp::Suppress)
        .unwrap();
    let after = slots_of(engine, &session);
    assert!(!after.contains_key(&leaf.raw()), "the suppression applies to the next request");
    assert!(!engine.cluster_slots_stale(&session));
    assert_eq!(rebuilt.len(), after.len() + 1);
}

/// **A level whose layer declares a centroid is centred on its figures**, the centroids the map
/// fills when it draws the level, whichever route asks first: browse alone serves the slots the
/// viewport then serves, and a bulk read in another session serves them too.
#[test]
fn a_level_centred_by_its_figures_is_coloured_alike_whichever_route_asks() {
    use tessera_types::layer::ServingLayout;
    const FIGURED: &str = "clusters/figured";
    const ONLY_FIGURED: &[&str] = &[FIGURED];
    let fx = fixture();
    let engine = &fx.engine;
    let leaves: Vec<Node> = fx
        .tree(&[])
        .into_iter()
        .filter(|n| n.key.starts_with("b4-"))
        .collect();
    fx.plant_as(
        FIGURED,
        HierarchyKind::Flat,
        &leaves,
        Some(ServingLayout::RowMajorLabel),
        vec!["centroid".to_string()],
    );
    assert_eq!(
        engine.recorded_layout(FIGURED, 0),
        Some(ServingLayout::RowMajorLabel)
    );
    // A fold writes the level's column, and a reopened engine serves the level from it alone.
    fold(engine);
    let Fx {
        _tmp,
        root,
        engine,
        cells,
    } = fx;
    drop(engine);
    let fx = Fx {
        engine: engine_at(_tmp.path(), &root, 3600),
        _tmp,
        root,
        cells,
    };
    let engine = &fx.engine;
    let session = fx.session(true);
    // Browse first, in a session no other route has coloured for.
    let browsed: BTreeMap<u64, Option<u8>> = engine
        .browse(
            &session,
            tessera_engine::browse::BrowseRequest {
                view: "s0",
                layer: FIGURED,
                level: None,
                form: tessera_engine::browse::BrowseForm::Roots,
                filter: None,
                limit: 1000,
                cursor: None,
                palette_size: Some(10),
            },
        )
        .unwrap()
        .artifacts
        .iter()
        .map(|row| (row.tessera_id.raw(), row.slot))
        .collect();
    assert_eq!(browsed.len(), leaves.len());
    assert!(browsed.values().all(Option::is_some), "browse alone serves slots");
    let drawn = viewport_slots(engine, &session, whole(ONLY_FIGURED, Some(10)));
    assert_eq!(drawn, browsed, "the viewport serves the slots browse did");
    assert_eq!(read_slots(engine, &fx.session(true), FIGURED, 10), drawn);
    let stats = engine
        .cluster_slot_stats(&session, "s0", FIGURED, 10)
        .unwrap();
    assert_eq!(stats.from_figures, leaves.len() as u64);
    assert_eq!(stats.sampled_items, 0, "no sample is read");
}

/// **A tiered parent has one heir, at the shallowest level holding a child it is served**: where
/// a middle node is withheld, the grandchild beneath it is the parent's child in the viewer's tree,
/// but a child one level down is the heir however much smaller; where the parent has no child one
/// level down, the grandchild is.
#[test]
fn a_tiered_parent_has_one_heir_at_its_shallowest_child_level() {
    use tessera_types::layer::LevelDeclaration;
    const TIERS: &str = "clusters/tiers";
    let fx = fixture();
    let engine = &fx.engine;
    let nodes = fx.tree(&[]);
    let members = |key: &str| nodes.iter().find(|n| n.key == key).expect("a node").members.clone();
    engine
        .register_layer(LayerDeclaration {
            scope: Default::default(),
            name: TIERS.into(),
            title: None,
            views: vec!["s0".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: ArtifactVisibility::carried("visibility"),
            require_member_visibility: None,
            hierarchy: Hierarchy {
                kind: HierarchyKind::Tiered,
                prune_children: true,
            },
            content: ContentDeclaration::default(),
            depends_on: Vec::new(),
            levels: (0..3)
                .map(|level| LevelDeclaration {
                    level,
                    title: None,
                    zoom: None,
                })
                .collect(),
            layout: None,
            shape: None,
        })
        .unwrap();
    let map = source_to_new_map(&fx.root, "v00000");
    // (level, key, members of, parent, label)
    let planted = [
        (0u32, "p", "b1-0-0", None, None),
        (0, "q", "b1-1-1", None, None),
        (1, "hidden", "b2-0-0", Some("p"), Some("7")),
        (1, "small", "b4-4-0", Some("p"), None),
        (1, "q-hidden", "b2-2-2", Some("q"), Some("7")),
        (2, "large", "b3-0-0", Some("hidden"), None),
        (2, "under-small", "b4-5-0", Some("small"), None),
        (2, "q-grandchild", "b3-4-4", Some("q-hidden"), None),
    ];
    let mut ids: BTreeMap<&str, TesseraId> = BTreeMap::new();
    for level in 0..3u32 {
        let at: Vec<_> = planted.iter().filter(|p| p.0 == level).collect();
        let artifacts = at
            .iter()
            .map(|&&(_, key, of, parent, label)| {
                let mut artifact = IncomingArtifact::from_entities(
                    Some(key.into()),
                    members(of)
                        .iter()
                        .map(|s| tessera_types::EntityId::new(map[s]))
                        .collect::<Vec<_>>(),
                );
                artifact.access = Some(label.map_or_else(Vec::new, |l: &str| vec![l.as_bytes().to_vec()]));
                artifact.parent_keys = parent.iter().map(|p: &&str| p.to_string()).collect();
                artifact
            })
            .collect();
        let published = engine.publish_artifacts(TIERS.into(), level, artifacts).unwrap();
        ids.extend(at.iter().map(|p| p.1).zip(published));
    }
    tick(engine);
    assert!(members("b3-0-0").len() > members("b4-4-0").len());
    let slots = read_slots(engine, &fx.session(true), TIERS, 10);
    let slot = |key: &str| slots[&ids[key].raw()].expect("a slot");
    assert!(!slots.contains_key(&ids["hidden"].raw()));
    assert_eq!(slot("small"), slot("p"), "the child one level down is the heir");
    assert_ne!(slot("large"), slot("p"), "the larger grandchild is not a second heir");
    assert_eq!(slot("q-grandchild"), slot("q"), "with no child one level down, it is");
}

/// **Colouring a tiered level reads no deeper level**: asking for the slots of the coarsest level
/// of a three-level layer, served from its columns with a centroid declared, starts the figures of
/// that level alone, and colouring the finest then starts the two levels below the coarsest.
#[test]
fn colouring_a_tiered_level_fills_no_deeper_level() {
    use tessera_types::layer::{LevelDeclaration, ServingLayout};
    const TIERS: &str = "clusters/lazy";
    let fx = fixture();
    let nodes = fx.tree(&[]);
    fx.engine
        .register_layer(LayerDeclaration {
            scope: Default::default(),
            name: TIERS.into(),
            title: None,
            views: vec!["s0".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: ArtifactVisibility::inherited(),
            require_member_visibility: None,
            hierarchy: Hierarchy {
                kind: HierarchyKind::Tiered,
                prune_children: true,
            },
            content: ContentDeclaration {
                computed: vec!["centroid".to_string()],
                ..ContentDeclaration::default()
            },
            depends_on: Vec::new(),
            levels: (0..3)
                .map(|level| LevelDeclaration {
                    level,
                    title: None,
                    zoom: None,
                })
                .collect(),
            layout: Some(ServingLayout::RowMajorLabel),
            shape: None,
        })
        .unwrap();
    let map = source_to_new_map(&fx.root, "v00000");
    for level in 0..3u32 {
        let artifacts = nodes
            .iter()
            .filter(|n| n.key.starts_with(&format!("b{}-", level + 1)))
            .map(|node| {
                let mut artifact = IncomingArtifact::from_entities(
                    Some(node.key.clone()),
                    node.members
                        .iter()
                        .map(|s| tessera_types::EntityId::new(map[s]))
                        .collect::<Vec<_>>(),
                );
                if level > 0 {
                    artifact.parent_keys = node.parent.iter().cloned().collect();
                }
                artifact
            })
            .collect();
        fx.engine
            .publish_artifacts(TIERS.into(), level, artifacts)
            .unwrap();
    }
    tick(&fx.engine);
    fold(&fx.engine);
    let Fx {
        _tmp,
        root,
        engine,
        cells,
    } = fx;
    drop(engine);
    let fx = Fx {
        engine: engine_at(_tmp.path(), &root, 3600),
        _tmp,
        root,
        cells,
    };
    let engine = &fx.engine;
    let session = fx.session(true);
    let started = |level: Option<u32>| {
        let before = engine.figures_stats().misses;
        let fields = ["slot".to_string()];
        let mut pages = Pages::default();
        engine
            .artifacts_stream(
                &session,
                ArtifactsRequest {
                    view: "s0",
                    layer: TIERS,
                    level,
                    parent: None,
                    q: None,
                    ids: None,
                    filter: None,
                    keep_unmatched: false,
                    count: false,
                    fields: &fields,
                    palette_size: Some(10),
                    page_rows: None,
                    pages: None,
                    cursor: None,
                    limits: limits(),
                    cancel: None,
                },
                &mut pages,
            )
            .unwrap();
        let rows = id_slots(&pages.pages, "tessera_id");
        assert!(rows.iter().all(|(_, slot)| slot.is_some()), "{level:?}");
        engine.figures_stats().misses - before
    };
    // The read takes the level's counts, and the slots the map's counts and centroids beside them.
    assert_eq!(started(Some(0)), 2, "the coarsest level alone is filled");
    // The finest level's read, and the map's entries of the two levels below the coarsest, which
    // colouring the coarsest did not start.
    assert_eq!(started(Some(2)), 3, "the deeper levels are filled when they are asked for");
}
