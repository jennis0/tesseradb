//! `Engine::artifacts_stream`: a viewer reads, page by page, every artifact of one layer they are
//! served, with the properties they name.
//!
//! The oracle is the planted layers themselves: every artifact's members are source ids whose
//! positions and terms are the generator's, so which members a viewer sees, where they lie and
//! which artifacts the verdict serves are computed here from the plant and never read back from
//! the engine. The one comparison against another route is the served set, which must equal the
//! set the viewport serves over the whole map.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{
    Array, BinaryArray, Float64Array, ListArray, StringArray, UInt32Array, UInt64Array,
};
use arrow::record_batch::RecordBatch;

use common::*;
use tessera_engine::filter::{FilterExpr, RegionLeaf};
use tessera_engine::{
    ArtifactsRequest, Engine, EngineError, ItemsRequest, PageEnd, RecordsHead, RecordsLimits,
    RecordsRefused, RecordsSink, RecordsTrailer, Session, SinkResult,
};
use tessera_lifecycle::membership::{IncomingAttachment, IncomingContent};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_spatial::shape::{ShapeF64, Space};
use tessera_types::layer::{
    ArtifactVisibility, ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind,
    LayerDeclaration, LevelDeclaration, MembershipSource, ShapeDeclaration, ShapeKind,
    SuppliedContent, SuppliedRequirement,
};
use tessera_types::{EntityId, TesseraId};

const N: u64 = 900;
const TREE: &str = "clusters/tree";
const LABELS: &str = "labels/tree";
const TOPICS: &str = "topics/contained";
const FLOOR: &str = "clusters/floor";
const BOXES: &str = "regions/boxes";
const TIERS: &str = "clusters/tiers";
/// The tree's roots: each holds 60 members, and each has three children of 20.
const ROOTS: u64 = 12;
/// The root whose own label only the subset viewer holds.
const LABELLED_ROOT: u64 = 3;
/// The child whose own label nobody holds.
const HIDDEN_CHILD: (u64, u64) = (5, 1);
/// A floor artifact is served while at least this many of its members are visible.
const FLOOR_COUNT: u64 = 15;
const BOX_RECTS: [[f64; 4]; 2] = [[100.0, 100.0, 400.0, 300.0], [600.0, 550.0, 950.0, 900.0]];

fn position(s: u64) -> (f64, f64) {
    (((s * 37) % 1000) as f64, ((s * 53) % 1000) as f64)
}

fn root_key(r: u64) -> String {
    format!("r{r:02}")
}

fn child_key(r: u64, c: u64) -> String {
    format!("r{r:02}-c{c}")
}

fn root_members(r: u64) -> std::ops::Range<u64> {
    r * 60..r * 60 + 60
}

fn child_members(r: u64, c: u64) -> std::ops::Range<u64> {
    r * 60 + c * 20..r * 60 + c * 20 + 20
}

fn floor_members(f: u64) -> std::ops::Range<u64> {
    f * 60..f * 60 + 60
}

fn sees(broad: bool, s: u64) -> bool {
    broad || subset_sees(s)
}

fn base_declaration(name: &str, kind: HierarchyKind) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

fn text_content(name: &str, requirement: SuppliedRequirement) -> ContentDeclaration {
    ContentDeclaration {
        computed: Vec::new(),
        supplied: vec![SuppliedContent {
            name: name.into(),
            ty: "text".into(),
            require_member_visibility: requirement,
        }],
    }
}

fn content(text: String) -> Vec<IncomingContent> {
    vec![IncomingContent {
        values: vec![text],
        generated_from: Default::default(),
    }]
}

struct Fx {
    tmp: tempfile::TempDir,
    engine: Option<Engine>,
    /// Source id to entity id.
    map: BTreeMap<u64, u64>,
}

impl Fx {
    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("the engine is open")
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().join("bundle")
    }

    fn open(root: &std::path::Path, tmp: &std::path::Path) -> Engine {
        let engine =
            open_engine_publishing(root, &tmp.join("cache"), &tmp.join("wal.log"));
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn restart(&mut self) {
        drop(self.engine.take());
        self.engine = Some(Fx::open(&self.root(), self.tmp.path()));
    }

    fn entities(&self, sources: impl IntoIterator<Item = u64>) -> Vec<EntityId> {
        sources
            .into_iter()
            .map(|s| EntityId::new(self.map[&s]))
            .collect()
    }

    fn members(&self, sources: impl IntoIterator<Item = u64>) -> croaring::Bitmap {
        self.entities(sources)
            .into_iter()
            .map(|e| e.raw() as u32)
            .collect()
    }

    /// An artifact's entity, by the identifier a read served.
    fn entity_of(&self, id: u64) -> EntityId {
        artifact_entity(self.engine(), TesseraId::new(id))
    }
}

/// The fixture: `N` points, and six layers planted over them.
///
/// - `clusters/tree`: nested, a supplied name; twelve roots and three children each, published
///   root then its children, so publication order interleaves the two depths. One root carries
///   a label only the subset viewer holds and one child a label nobody holds.
/// - `labels/tree`: flat, attached to the tree's roots, one label per root.
/// - `topics/contained`: flat, content served only to a viewer who sees everything it was
///   generated from.
/// - `clusters/floor`: flat, served while [`FLOOR_COUNT`] members are visible, with a derived
///   hull, centroid and box.
/// - `regions/boxes`: two boxes whose membership is the rows inside them.
/// - `clusters/tiers`: stacked over two levels, published level 1 first.
fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        N,
    );
    let engine = Fx::open(&root, tmp.path());
    let map = source_to_new_map(&root, "v00000");
    let fx = Fx {
        tmp,
        engine: Some(engine),
        map,
    };
    let engine = fx.engine();

    let mut tree = base_declaration(TREE, HierarchyKind::Nested);
    tree.artifact_visibility = ArtifactVisibility::carried("visibility");
    tree.content = text_content("label", SuppliedRequirement::Inherited);
    engine.register_layer(tree).unwrap();
    let mut planted = Vec::new();
    for r in 0..ROOTS {
        let mut node = IncomingArtifact::from_entities(Some(root_key(r)), fx.entities(root_members(r)));
        node.contents = content(format!("The {} group", root_key(r)));
        if r == LABELLED_ROOT {
            node.access = vec![b"1".to_vec()];
        }
        planted.push(node);
        for c in 0..3 {
            let mut child =
                IncomingArtifact::from_entities(Some(child_key(r, c)), fx.entities(child_members(r, c)));
            child.parent_keys = vec![root_key(r)];
            child.contents = content(format!("Subgroup {}", child_key(r, c)));
            if (r, c) == HIDDEN_CHILD {
                child.access = vec![b"7".to_vec()];
            }
            planted.push(child);
        }
    }
    engine.publish_artifacts(TREE.into(), 0, planted).unwrap();

    let mut labels = base_declaration(LABELS, HierarchyKind::Flat);
    labels.depends_on = vec![TREE.into()];
    engine.register_layer(labels).unwrap();
    let attached: Vec<IncomingArtifact> = (0..ROOTS)
        .map(|r| {
            let mut label = IncomingArtifact::from_entities(
                Some(format!("label-{}", root_key(r))),
                fx.entities(root_members(r)),
            );
            label.attached_to = Some(IncomingAttachment {
                layer: TREE.into(),
                level: 0,
                key: root_key(r),
            });
            label
        })
        .collect();
    engine.publish_artifacts(LABELS.into(), 0, attached).unwrap();

    let mut topics = base_declaration(TOPICS, HierarchyKind::Flat);
    topics.content = text_content("summary", SuppliedRequirement::All);
    engine.register_layer(topics).unwrap();
    let topic = |key: &str, members: Vec<u64>, generated: Vec<u64>| {
        let mut t = IncomingArtifact::from_entities(Some(key.into()), fx.entities(members));
        t.contents = vec![IncomingContent {
            values: vec![format!("about {key}")],
            generated_from: fx.members(generated),
        }];
        t
    };
    engine
        .publish_artifacts(
            TOPICS.into(),
            0,
            vec![
                // Generated from items the subset viewer cannot all see.
                topic("everyone-sees-some", (0..30).collect(), (0..30).collect()),
                // Generated from items the subset viewer sees.
                topic("subset-sees-all", (0..30).collect(), (0..30).step_by(3).collect()),
            ],
        )
        .unwrap();

    let mut floor = base_declaration(FLOOR, HierarchyKind::Flat);
    floor.require_member_visibility = Some(ExistenceCriterion::Count(FLOOR_COUNT));
    floor.content.computed = vec!["hull".into()];
    engine.register_layer(floor).unwrap();
    let floors: Vec<IncomingArtifact> = (0..10)
        .map(|f| IncomingArtifact::from_entities(Some(format!("f{f}")), fx.entities(floor_members(f))))
        .collect();
    engine.publish_artifacts(FLOOR.into(), 0, floors).unwrap();

    let mut boxes = base_declaration(BOXES, HierarchyKind::Flat);
    boxes.membership = MembershipSource::Spatial;
    boxes.shape = Some(ShapeDeclaration {
        kind: ShapeKind::Bbox,
    });
    engine.register_layer(boxes).unwrap();
    for (i, rect) in BOX_RECTS.iter().enumerate() {
        let canonical = ShapeF64::Bbox {
            min_x: rect[0],
            min_y: rect[1],
            max_x: rect[2],
            max_y: rect[3],
        }
        .canonical(Space::View, &extent())
        .unwrap()
        .0;
        let shape = tessera_lifecycle::membership::ArtifactShapes::new(vec![(
            "s0".to_string(),
            canonical.encode(),
        )])
        .unwrap();
        let artifact = IncomingArtifact {
            shape: Some(shape),
            ..IncomingArtifact::from_entities(Some(format!("box{i}")), [])
        };
        engine.publish_artifacts(BOXES.into(), 0, vec![artifact]).unwrap();
    }

    let mut tiers = base_declaration(TIERS, HierarchyKind::Stacked);
    tiers.levels = (0..2)
        .map(|level| LevelDeclaration {
            level,
            title: None,
            zoom: None,
        })
        .collect();
    engine.register_layer(tiers).unwrap();
    for level in [1u32, 0] {
        let artifacts = (0..4)
            .map(|t| {
                IncomingArtifact::from_entities(
                    Some(format!("t{level}-{t}")),
                    fx.entities(t * 100..t * 100 + 100),
                )
            })
            .collect();
        engine.publish_artifacts(TIERS.into(), level, artifacts).unwrap();
    }
    tick(engine);
    fx
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

fn limits() -> RecordsLimits {
    RecordsLimits {
        max_page_rows: 100_000,
        max_page_bytes: 64 << 20,
        response_bytes: 256 << 20,
        response_time: Duration::from_secs(60),
    }
}

fn request<'a>(layer: &'a str, fields: &'a [String]) -> ArtifactsRequest<'a> {
    ArtifactsRequest {
        view: "s0",
        layer,
        level: None,
        parent: None,
        q: None,
        filter: None,
        keep_unmatched: false,
        count: false,
        fields,
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
    head: Option<RecordsHead>,
    pages: Vec<(RecordBatch, PageEnd)>,
}

impl RecordsSink for Collect {
    fn head(&mut self, head: &RecordsHead) -> SinkResult {
        assert!(self.head.is_none(), "one head per response");
        self.head = Some(head.clone());
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, end: &PageEnd) -> SinkResult {
        assert!(self.head.is_some(), "the head precedes every page");
        self.pages.push((batch.clone(), end.clone()));
        Ok(())
    }
}

fn respond(
    engine: &Engine,
    session: &Session,
    req: ArtifactsRequest<'_>,
) -> Result<(Collect, RecordsTrailer), EngineError> {
    let mut sink = Collect::default();
    match engine.artifacts_stream(session, req, &mut sink) {
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

/// Every page of a read, from `cursor` until a response ends with none.
fn read_from(
    engine: &Engine,
    session: &Session,
    base: &ArtifactsRequest<'_>,
    mut cursor: Option<String>,
) -> Vec<RecordBatch> {
    let mut pages = Vec::new();
    for _ in 0..10_000 {
        let mut req = base.clone();
        req.cursor = cursor.as_deref();
        if cursor.is_some() {
            req.count = false;
        }
        let (sink, trailer) = respond(engine, session, req).expect("a response");
        pages.extend(sink.pages.into_iter().map(|(batch, _)| batch));
        cursor = trailer.next;
        if cursor.is_none() {
            return pages;
        }
    }
    panic!("the read never ended");
}

fn read_all(engine: &Engine, session: &Session, base: &ArtifactsRequest<'_>) -> Vec<RecordBatch> {
    read_from(engine, session, base, None)
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column {name} is not the expected type"))
}

fn ids(pages: &[RecordBatch]) -> Vec<u64> {
    pages
        .iter()
        .flat_map(|b| column::<UInt64Array>(b, "tessera_id").values().to_vec())
        .collect()
}

fn keys(pages: &[RecordBatch]) -> Vec<String> {
    pages
        .iter()
        .flat_map(|b| {
            let keys = column::<StringArray>(b, "key");
            (0..keys.len())
                .map(|i| keys.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn u64s(pages: &[RecordBatch], name: &str) -> Vec<u64> {
    pages
        .iter()
        .flat_map(|b| column::<UInt64Array>(b, name).values().to_vec())
        .collect()
}

/// The identifiers the viewport serves in `layer` over the whole map.
fn viewport_ids(engine: &Engine, credential: &[u8], layer: &str) -> HashSet<u64> {
    artifacts_of(engine, credential)
        .into_iter()
        .filter(|a| a.layer == layer)
        .map(|a| a.tessera_id.raw())
        .collect()
}

/// The tree in publication order, each key with its level-0 ordinal's served verdict for one
/// viewer: the whole oracle for the tree layer.
fn tree_served(broad: bool) -> Vec<String> {
    let mut out = Vec::new();
    for r in 0..ROOTS {
        if r != LABELLED_ROOT || !broad {
            out.push(root_key(r));
        }
        for c in 0..3 {
            if (r, c) != HIDDEN_CHILD {
                out.push(child_key(r, c));
            }
        }
    }
    out
}

fn tree_members(key: &str) -> std::ops::Range<u64> {
    let r: u64 = key[1..3].parse().unwrap();
    match key.split_once("-c") {
        None => root_members(r),
        Some((_, c)) => child_members(r, c.parse().unwrap()),
    }
}

// ---------------------------------------------------------------------------------------------
// Order and completeness
// ---------------------------------------------------------------------------------------------

/// **A read across many small pages returns every served artifact once, in publication order**,
/// with the verdict's own count beside each, and the set is the one the viewport serves.
#[test]
fn a_read_in_small_pages_returns_every_served_artifact_once_in_publication_order() {
    let fx = fixture();
    let engine = fx.engine();
    let fields = names(&["key", "level", "masked_count"]);
    for (credential, broad) in [(full_coverage_credential(), true), (subset_credential(), false)] {
        let session = engine.authorise(&credential).unwrap();
        for page_rows in [1, 5, 1000] {
            let mut base = request(TREE, &fields);
            base.page_rows = Some(page_rows);
            base.pages = Some(2);
            let pages = read_all(engine, &session, &base);
            assert_eq!(keys(&pages), tree_served(broad), "page_rows {page_rows}");
            let served: HashSet<u64> = ids(&pages).into_iter().collect();
            assert_eq!(served.len(), ids(&pages).len(), "an artifact returned twice");
            assert_eq!(served, viewport_ids(engine, &credential, TREE));
            let counts = u64s(&pages, "masked_count");
            for (key, count) in keys(&pages).iter().zip(counts) {
                let want = tree_members(key).filter(|&s| sees(broad, s)).count() as u64;
                assert_eq!(count, want, "{key}");
            }
            assert!(pages
                .iter()
                .all(|b| column::<UInt32Array>(b, "level").values().iter().all(|&l| l == 0)));
        }
    }
}

/// **A levelled layer reads level by level, each in publication order**, and `level` narrows it
/// to one; later runs of a level sit at lower addresses, so this is not entity order.
#[test]
fn a_levelled_layer_reads_by_level_then_publication() {
    let fx = fixture();
    let engine = fx.engine();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["key", "level"]);
    let mut base = request(TIERS, &fields);
    base.page_rows = Some(3);
    let pages = read_all(engine, &session, &base);
    let want: Vec<String> = [0, 1]
        .iter()
        .flat_map(|level| (0..4).map(move |t| format!("t{level}-{t}")))
        .collect();
    assert_eq!(keys(&pages), want);
    let levels: Vec<u32> = pages
        .iter()
        .flat_map(|b| column::<UInt32Array>(b, "level").values().to_vec())
        .collect();
    assert_eq!(levels, vec![0, 0, 0, 0, 1, 1, 1, 1]);
    base.level = Some(1);
    assert_eq!(keys(&read_all(engine, &session, &base)), want[4..].to_vec());
}

// ---------------------------------------------------------------------------------------------
// What is served
// ---------------------------------------------------------------------------------------------

/// **An artifact on a label the viewer lacks, one whose content they cannot read, and an attached
/// artifact whose target is withheld are absent and leave no trace**: not in the rows, not in the
/// counts, not as anything's parent, and no page is spent on them.
#[test]
fn a_withheld_artifact_leaves_no_trace() {
    let fx = fixture();
    let engine = fx.engine();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();

    // The labelled root is served to the subset viewer alone; its children name it there and
    // name nothing for the broad viewer.
    let fields = names(&["key", "parents"]);
    let parents_of = |session: &Session| -> HashMap<String, Vec<u64>> {
        let pages = read_all(engine, session, &request(TREE, &fields));
        let keys = keys(&pages);
        let mut lists = Vec::new();
        for batch in &pages {
            let parents = column::<ListArray>(batch, "parents");
            for i in 0..parents.len() {
                let list = parents.value(i);
                let list = list.as_any().downcast_ref::<UInt64Array>().unwrap();
                lists.push(list.values().to_vec());
            }
        }
        keys.into_iter().zip(lists).collect()
    };
    let subset_parents = parents_of(&subset);
    let full_parents = parents_of(&full);
    let labelled_child = child_key(LABELLED_ROOT, 0);
    assert_eq!(subset_parents[&labelled_child].len(), 1, "named where it is served");
    assert!(full_parents[&labelled_child].is_empty(), "and nowhere else");
    assert_eq!(full_parents[&child_key(0, 0)].len(), 1);

    // The attached labels follow their roots: the broad viewer is served none for the labelled
    // root, and the labels' targets are the roots' own identifiers.
    let fields = names(&["key", "target"]);
    for (session, broad) in [(&full, true), (&subset, false)] {
        let pages = read_all(engine, session, &request(LABELS, &fields));
        let want: Vec<String> = (0..ROOTS)
            .filter(|&r| r != LABELLED_ROOT || !broad)
            .map(|r| format!("label-{}", root_key(r)))
            .collect();
        assert_eq!(keys(&pages), want);
        let credential = if broad {
            full_coverage_credential()
        } else {
            subset_credential()
        };
        let roots = viewport_ids(engine, &credential, TREE);
        for target in u64s(&pages, "target") {
            assert!(roots.contains(&target), "a target the viewer is not served");
        }
    }

    // Content: the broad viewer is served both topics, the subset viewer only the one whose
    // generating set it contains.
    let fields = names(&["key", "content"]);
    assert_eq!(
        keys(&read_all(engine, &full, &request(TOPICS, &fields))),
        vec!["everyone-sees-some", "subset-sees-all"]
    );
    assert_eq!(
        keys(&read_all(engine, &subset, &request(TOPICS, &fields))),
        vec!["subset-sees-all"]
    );

    // No page and no count for what is withheld: one row a page gives one page per served
    // artifact, and the head counts exactly those.
    for (session, broad) in [(&full, true), (&subset, false)] {
        let fields = names(&["key"]);
        let mut req = request(TREE, &fields);
        req.page_rows = Some(1);
        req.count = true;
        let (sink, trailer) = respond(engine, session, req).unwrap();
        let counts = sink.head.unwrap().counts.unwrap();
        let served = tree_served(broad).len() as u64;
        assert_eq!((counts.served, counts.matched), (served, served));
        assert_eq!(trailer.rows, trailer.pages);
    }
}

/// **A parent the viewer is not served gives the response a parent with no children gives**, and
/// a served parent gives its children.
#[test]
fn a_withheld_parent_answers_as_a_parent_with_no_children() {
    let fx = fixture();
    let engine = fx.engine();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["key"]);
    let ids_by_key = |session: &Session| -> HashMap<String, u64> {
        let pages = read_all(engine, session, &request(TREE, &fields));
        keys(&pages).into_iter().zip(ids(&pages)).collect()
    };
    let subset_ids = ids_by_key(&subset);
    let withheld = subset_ids[&root_key(LABELLED_ROOT)];
    let leaf = subset_ids[&child_key(0, 0)];
    let served_root = subset_ids[&root_key(0)];

    let answer = |parent: u64| {
        let mut req = request(TREE, &fields);
        req.parent = Some(TesseraId::new(parent));
        req.count = true;
        let (sink, trailer) = respond(engine, &full, req).unwrap();
        let head = sink.head.unwrap();
        (
            head.counts,
            head.page_rows,
            sink.pages.len(),
            trailer.pages,
            trailer.rows,
            trailer.next,
            trailer.ended_by,
        )
    };
    assert_eq!(answer(withheld), answer(leaf));
    assert_eq!(answer(0x7777_7777_7777_7777), answer(leaf));

    let mut req = request(TREE, &fields);
    req.parent = Some(TesseraId::new(served_root));
    assert_eq!(
        keys(&read_all(engine, &full, &req)),
        (0..3).map(|c| child_key(0, c)).collect::<Vec<_>>()
    );
    let mut req = request(TREE, &fields);
    req.q = Some("SUBGROUP R01");
    assert_eq!(
        keys(&read_all(engine, &full, &req)),
        (0..3).map(|c| child_key(1, c)).collect::<Vec<_>>()
    );
}

/// The filter's region: a box over the middle of the map.
fn middle() -> (FilterExpr, [f64; 4]) {
    let rect = [200.0, 150.0, 700.0, 650.0];
    let shape = ShapeF64::Bbox {
        min_x: rect[0],
        min_y: rect[1],
        max_x: rect[2],
        max_y: rect[3],
    }
    .canonical(Space::View, &extent())
    .unwrap()
    .0;
    (FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape))), rect)
}

fn inside(rect: [f64; 4], s: u64) -> bool {
    let (x, y) = position(s);
    x >= rect[0] && x <= rect[2] && y >= rect[1] && y <= rect[3]
}

/// **A filter narrows the rows to artifacts with a matching visible member and counts them**;
/// `keep_unmatched` returns the rest with a zero, and the head's counts agree with the rows.
#[test]
fn a_filter_counts_matching_members_and_narrows_the_rows() {
    let fx = fixture();
    let engine = fx.engine();
    let (filter, rect) = middle();
    let fields = names(&["key", "masked_count"]);
    for (credential, broad) in [(full_coverage_credential(), true), (subset_credential(), false)] {
        let session = engine.authorise(&credential).unwrap();
        let matched_of = |key: &str| {
            tree_members(key)
                .filter(|&s| sees(broad, s) && inside(rect, s))
                .count() as u64
        };
        let mut req = request(TREE, &fields);
        req.filter = Some(filter.clone());
        req.page_rows = Some(4);
        let pages = read_all(engine, &session, &req);
        let want: Vec<String> = tree_served(broad)
            .into_iter()
            .filter(|k| matched_of(k) > 0)
            .collect();
        assert!(!want.is_empty() && want.len() < tree_served(broad).len());
        assert_eq!(keys(&pages), want);
        for (key, count) in keys(&pages).iter().zip(u64s(&pages, "matched_count")) {
            assert_eq!(count, matched_of(key), "{key}");
        }

        req.keep_unmatched = true;
        let pages = read_all(engine, &session, &req);
        assert_eq!(keys(&pages), tree_served(broad));
        for (key, count) in keys(&pages).iter().zip(u64s(&pages, "matched_count")) {
            assert_eq!(count, matched_of(key), "{key}");
        }

        req.keep_unmatched = false;
        req.count = true;
        req.pages = Some(1);
        let (sink, _) = respond(engine, &session, req).unwrap();
        let counts = sink.head.unwrap().counts.unwrap();
        assert_eq!(counts.served, tree_served(broad).len() as u64);
        assert_eq!(counts.matched, want.len() as u64);
    }
}

// ---------------------------------------------------------------------------------------------
// Writes during a read
// ---------------------------------------------------------------------------------------------

/// **A suppression or deletion of an artifact, and a deletion of members that takes one below its
/// criterion, accepted between two responses, apply from the next response.**
#[test]
fn a_deny_accepted_mid_read_applies_from_the_next_response() {
    let fx = fixture();
    let engine = fx.engine();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["key"]);
    let mut base = request(FLOOR, &fields);
    base.page_rows = Some(2);
    base.pages = Some(1);
    let (sink, trailer) = respond(engine, &session, base.clone()).unwrap();
    let first = keys(&sink.pages.into_iter().map(|(b, _)| b).collect::<Vec<_>>());
    assert_eq!(first, vec!["f0", "f1"]);

    let all = read_all(engine, &session, &request(FLOOR, &fields));
    let id_of: HashMap<String, u64> = keys(&all).into_iter().zip(ids(&all)).collect();
    engine
        .accept_change(fx.entity_of(id_of["f4"]), ChangeOp::Suppress)
        .unwrap();
    engine
        .accept_change(fx.entity_of(id_of["f6"]), ChangeOp::Delete)
        .unwrap();
    // f8 keeps FLOOR_COUNT - 1 of its members.
    for entity in fx.entities(floor_members(8).skip(FLOOR_COUNT as usize - 1)) {
        engine.accept_change(entity, ChangeOp::Delete).unwrap();
    }

    let rest = keys(&read_from(engine, &session, &base, trailer.next));
    assert_eq!(rest, vec!["f2", "f3", "f5", "f7", "f9"]);
}

/// **A read continuing across an ingest's flush, a fold, a restart and a further publication into
/// the layer neither loses nor repeats a row**: ordinals do not move, and an artifact published
/// during the read, which sorts after it, is returned.
#[test]
fn a_read_continues_across_a_flush_a_fold_a_restart_and_a_publication() {
    let mut fx = fixture();
    let fields = names(&["key"]);
    let session = fx.engine().authorise(&full_coverage_credential()).unwrap();
    let mut base = request(TREE, &fields);
    base.page_rows = Some(7);
    base.pages = Some(1);
    let (sink, trailer) = respond(fx.engine(), &session, base.clone()).unwrap();
    let mut read = keys(&sink.pages.into_iter().map(|(b, _)| b).collect::<Vec<_>>());
    let mut cursor = trailer.next;

    // A deletion to fold away, an ingest to flush, then a fold.
    let engine = fx.engine();
    engine
        .accept_change(fx.entities([899])[0], ChangeOp::Delete)
        .unwrap();
    ingest(engine, "late-arrival");
    flush(engine);
    let (sink, trailer) =
        respond(engine, &session, ArtifactsRequest { cursor: cursor.as_deref(), ..base.clone() })
            .unwrap();
    read.extend(keys(&sink.pages.into_iter().map(|(b, _)| b).collect::<Vec<_>>()));
    cursor = trailer.next;
    fold(engine);
    let (sink, trailer) =
        respond(engine, &session, ArtifactsRequest { cursor: cursor.as_deref(), ..base.clone() })
            .unwrap();
    read.extend(keys(&sink.pages.into_iter().map(|(b, _)| b).collect::<Vec<_>>()));
    cursor = trailer.next;

    fx.restart();
    let engine = fx.engine();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let mut late = IncomingArtifact::from_entities(Some("r99".into()), fx.entities(0..10));
    late.contents = content("A late group".into());
    engine.publish_artifacts(TREE.into(), 0, vec![late]).unwrap();
    tick(engine);
    read.extend(keys(&read_from(engine, &session, &base, cursor)));

    let mut want = tree_served(true);
    want.push("r99".into());
    assert_eq!(read, want);
}

// ---------------------------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------------------------

/// **A cursor is refused under another credential, layer, level or route, and after its layer is
/// dropped**; another idset is the item route's refusal.
#[test]
fn a_cursor_is_refused_outside_its_read() {
    let fx = fixture();
    let engine = fx.engine();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let fields = names(&["key"]);
    let first = |layer: &str, level: Option<u32>| -> String {
        let mut req = request(layer, &fields);
        req.level = level;
        req.page_rows = Some(1);
        req.pages = Some(1);
        respond(engine, &full, req).unwrap().1.next.unwrap()
    };
    let cursor = first(TREE, None);
    let present = |session: &Session, layer: &str, level: Option<u32>, cursor: &str| {
        let mut req = request(layer, &fields);
        req.level = level;
        req.cursor = Some(cursor);
        respond(engine, session, req).map(|_| ())
    };
    assert!(present(&full, TREE, None, &cursor).is_ok(), "the cursor resumes its own read");

    let tiers_cursor = first(TIERS, Some(0));
    assert!(present(&full, TIERS, Some(0), &tiers_cursor).is_ok());
    let item_fields: Vec<String> = Vec::new();
    let items_cursor = {
        let mut sink = Collect::default();
        let req = ItemsRequest {
            view: "s0",
            fields: &item_fields,
            system_fields: &[],
            filter: None,
            keep_unmatched: false,
            count: false,
            order: None,
            page_rows: Some(1),
            pages: Some(1),
            cursor: None,
            idset: None,
            limits: limits(),
            cancel: None,
        };
        engine.items_stream(&full, req, &mut sink).unwrap().next.unwrap()
    };

    let mut refusals = vec![
        present(&subset, TREE, None, &cursor).unwrap_err(),
        present(&full, FLOOR, None, &cursor).unwrap_err(),
        present(&full, TIERS, Some(1), &tiers_cursor).unwrap_err(),
        present(&full, TIERS, None, &tiers_cursor).unwrap_err(),
        present(&full, TREE, None, &items_cursor).unwrap_err(),
    ];
    let mut sink = Collect::default();
    let items_req = ItemsRequest {
        view: "s0",
        fields: &item_fields,
        system_fields: &[],
        filter: None,
        keep_unmatched: false,
        count: false,
        order: None,
        page_rows: None,
        pages: None,
        cursor: Some(&cursor),
        idset: None,
        limits: limits(),
        cancel: None,
    };
    refusals.push(engine.items_stream(&full, items_req, &mut sink).unwrap_err());
    for refusal in &refusals {
        assert!(
            matches!(refusal, EngineError::CursorRefused),
            "a foreign cursor was answered {refusal:?}"
        );
    }

    let mut stale = request(TREE, &fields);
    stale.idset = Some(2);
    assert!(matches!(
        respond(engine, &full, stale).map(|_| ()),
        Err(EngineError::StaleIdSet)
    ));

    engine.drop_layer(FLOOR.into()).unwrap();
    let floor_cursor = cursor.clone();
    assert!(matches!(
        present(&full, FLOOR, None, &floor_cursor),
        Err(EngineError::RecordsRefused(RecordsRefused::UnknownLayer(_)))
    ));
}

/// **The request's own mistakes are refused before anything is read.**
#[test]
fn malformed_requests_are_refused() {
    let fx = fixture();
    let engine = fx.engine();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = names(&["key"]);
    let bad_fields = names(&["key", "size"]);
    let twice = names(&["key", "key"]);
    let cases: Vec<(&str, ArtifactsRequest<'_>)> = vec![
        ("an unknown layer", request("clusters/nowhere", &fields)),
        ("an unknown property", request(TREE, &bad_fields)),
        ("a repeated property", request(TREE, &twice)),
        ("level on a one-level kind", ArtifactsRequest { level: Some(0), ..request(TREE, &fields) }),
        ("a level not held", ArtifactsRequest { level: Some(2), ..request(TIERS, &fields) }),
        (
            "parent with q",
            ArtifactsRequest {
                parent: Some(TesseraId::new(1)),
                q: Some("r"),
                ..request(TREE, &fields)
            },
        ),
        ("zero page rows", ArtifactsRequest { page_rows: Some(0), ..request(TREE, &fields) }),
        (
            "count with a cursor",
            ArtifactsRequest {
                count: true,
                cursor: Some("AAAA"),
                ..request(TREE, &fields)
            },
        ),
    ];
    for (what, req) in cases {
        match respond(engine, &session, req) {
            Err(EngineError::RecordsRefused(_)) => {}
            other => panic!("{what} is the caller's mistake, not {:?}", other.map(|_| ())),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------------------------

/// One grid step of the fixture's extent, the precision a stored position keeps.
const STEP: f64 = 1e-6;

fn point_on_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
    (p.0 - qx).hypot(p.1 - qy) <= 1e-4
}

fn inside_ring(p: (f64, f64), ring: &[(f64, f64)]) -> bool {
    let n = ring.len();
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        if point_on_segment(p, a, b) {
            return true;
        }
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
            inside = !inside;
        }
    }
    inside
}

/// **Centroid, box and a derived hull are over the members this viewer can see, in the view's
/// coordinates**; a predicate shape is the published box.
#[test]
fn geometry_is_over_the_visible_members_in_view_coordinates() {
    let fx = fixture();
    let engine = fx.engine();
    let fields = names(&["key", "centroid", "box", "shape"]);
    for (credential, broad) in [(full_coverage_credential(), true), (subset_credential(), false)] {
        let session = engine.authorise(&credential).unwrap();
        let pages = read_all(engine, &session, &request(FLOOR, &fields));
        let keys = keys(&pages);
        assert!(!keys.is_empty());
        let mut at = 0usize;
        for batch in &pages {
            let cx = column::<Float64Array>(batch, "centroid_x");
            let cy = column::<Float64Array>(batch, "centroid_y");
            let bx0 = column::<Float64Array>(batch, "box_x_min");
            let by0 = column::<Float64Array>(batch, "box_y_min");
            let bx1 = column::<Float64Array>(batch, "box_x_max");
            let by1 = column::<Float64Array>(batch, "box_y_max");
            let shapes = column::<BinaryArray>(batch, "shape");
            for i in 0..batch.num_rows() {
                let f: u64 = keys[at][1..].parse().unwrap();
                at += 1;
                let visible: Vec<(f64, f64)> = floor_members(f)
                    .filter(|&s| sees(broad, s))
                    .map(position)
                    .collect();
                let n = visible.len() as f64;
                let mean = (
                    visible.iter().map(|p| p.0).sum::<f64>() / n,
                    visible.iter().map(|p| p.1).sum::<f64>() / n,
                );
                assert!((cx.value(i) - mean.0).abs() < STEP && (cy.value(i) - mean.1).abs() < STEP);
                let lo = |pick: fn(&(f64, f64)) -> f64| visible.iter().map(pick).fold(f64::MAX, f64::min);
                let hi = |pick: fn(&(f64, f64)) -> f64| visible.iter().map(pick).fold(f64::MIN, f64::max);
                assert!((bx0.value(i) - lo(|p| p.0)).abs() < STEP);
                assert!((by0.value(i) - lo(|p| p.1)).abs() < STEP);
                assert!((bx1.value(i) - hi(|p| p.0)).abs() < STEP);
                assert!((by1.value(i) - hi(|p| p.1)).abs() < STEP);

                let hull = tessera_spatial::shape::read_wkb(shapes.value(i)).expect("WKB");
                let rings: Vec<&Vec<(f64, f64)>> = hull.iter().flatten().collect();
                for ring in &rings {
                    for v in ring.iter() {
                        assert!(
                            visible.iter().any(|p| (p.0 - v.0).abs() < STEP && (p.1 - v.1).abs() < STEP),
                            "a hull vertex {v:?} is not a visible member"
                        );
                    }
                }
                for p in &visible {
                    assert!(
                        rings.iter().any(|ring| inside_ring(*p, ring)),
                        "a visible member {p:?} lies outside the hull"
                    );
                }
            }
        }
    }

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let pages = read_all(engine, &session, &request(BOXES, &fields));
    assert_eq!(keys(&pages), vec!["box0", "box1"]);
    let shapes: Vec<Vec<u8>> = pages
        .iter()
        .flat_map(|b| {
            let s = column::<BinaryArray>(b, "shape");
            (0..s.len()).map(|i| s.value(i).to_vec()).collect::<Vec<_>>()
        })
        .collect();
    for (bytes, rect) in shapes.iter().zip(BOX_RECTS) {
        let parts = tessera_spatial::shape::read_wkb(bytes).unwrap();
        let ring = &parts[0][0];
        let corners = [
            (rect[0], rect[1]),
            (rect[2], rect[1]),
            (rect[2], rect[3]),
            (rect[0], rect[3]),
        ];
        for corner in corners {
            assert!(
                ring.iter()
                    .any(|v| (v.0 - corner.0).abs() < 1e-3 && (v.1 - corner.1).abs() < 1e-3),
                "the box's corner {corner:?} is not in {ring:?}"
            );
        }
    }
}
