//! **The per-point membership column names the deepest served artifact and never one the
//! response withheld** (D12, `client-components.md` §5.10; `tessera_engine::membership_column`).
//!
//! The property the leak argument rests on is the join: every non-null value in the column is an
//! identifier in the same response's artifacts frame. It is asserted on every response these
//! cases take, whatever else they check, so a resolver that reached past the served set — to a
//! leaf the cut removed, a child a masked principal was not served, a label whose cluster went —
//! fails here before anything reads the value.
//!
//! The tree is planted, as `artifact_hierarchy` plants it, so the expected ancestor is arithmetic
//! on source ids rather than anything the engine said. The two serving layouts are compared on
//! the same corpus, because the column is read off whichever the level has and the two routes
//! share no code.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use tessera_engine::{ArtifactOut, Engine, LayerSelection, ViewportOut, ViewportRequest};
use tessera_lifecycle::membership::{IncomingAttachment, IncomingContent};
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ServingLayout, SuppliedContent, SuppliedRequirement,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const TREE: &str = "clusters/tree";
const LABELS: &str = "clusters/labels";

fn declaration(
    name: &str,
    criterion: Option<ExistenceCriterion>,
    prune_children: bool,
    layout: Option<ServingLayout>,
) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Nested,
            prune_children,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

/// A label layer over the tree, the shape a toponymy layer publishes.
fn labels() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LABELS.into(),
        title: Some("labels".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "topic".into(),
                ty: "text".into(),
                require_member_visibility: SuppliedRequirement::All,
            }],
            withdraw_on_member_deletion: true,
        },
        depends_on: vec![TREE.into()],
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
    map: BTreeMap<u64, u64>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let map = source_to_new_map(&root, "v00000");
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
        map,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }

    /// Every visible point served — the measurement's shape, where a per-tile cap would make the
    /// column's cost a function of the cap rather than of the viewport.
    fn open_uncapped(&self) -> Engine {
        let mut engine = open_engine_uncapped(&self.root, &self.cache, &self.wal);
        engine
            .start_write_executor(8)
            .expect("the executor starts once");
        engine
    }

    fn members(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        source_ids.map(|s| EntityId::new(self.map[&s])).collect()
    }

    /// The wire identity of a source id's point — `shard 0`, as the fixture builds.
    fn point_id(&self, source: u64) -> u64 {
        test_key()
            .forward(0, EntityId::new(self.map[&source]))
            .unwrap()
            .raw()
    }

    /// Wait for a publication's membership extents to land, which is when a pinned row-major
    /// column exists to be served from.
    fn wait_for_publication(&self, engine: &Engine, files: usize) {
        let dir = self
            .root
            .join(&engine.generation().prefix)
            .join("partitions")
            .join("default")
            .join("members");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let held = std::fs::read_dir(&dir).into_iter().flatten().count();
            if held >= files {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the membership extents were never published"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn node(
    fx: &Fixture,
    key: &str,
    parent: Option<&str>,
    sources: impl Iterator<Item = u64>,
) -> IncomingArtifact {
    let mut artifact = IncomingArtifact::from_entities(Some(key.into()), fx.members(sources));
    artifact.parent_key = parent.map(str::to_string);
    artifact
}

/// The planted tree: `root` over `0..400`, two children, four grandchildren, each a half of its
/// parent. The frontier is the grandchildren; a budget of three climbs to the children.
fn plant(fx: &Fixture, engine: &Engine) {
    engine
        .publish_artifacts(
            TREE.into(),
            0,
            vec![
                node(fx, "root", None, 0..400),
                node(fx, "a", Some("root"), 0..200),
                node(fx, "b", Some("root"), 200..400),
                node(fx, "a1", Some("a"), 0..100),
                node(fx, "a2", Some("a"), 100..200),
                node(fx, "b1", Some("b"), 200..300),
                node(fx, "b2", Some("b"), 300..400),
            ],
        )
        .unwrap();
}

fn viewport(
    engine: &Engine,
    credential: &[u8],
    bbox: [f64; 4],
    layers: LayerSelection,
    budget: Option<u32>,
) -> ViewportOut {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, bbox, N_ITEMS as usize)
                .layers(layers)
                .artifact_budget(budget),
        )
        .expect("a viewport")
}

/// **The join.** Every non-null value of every membership column is the identifier of an artifact
/// of that layer in this response's artifacts frame — the property the leak argument rests on.
/// Returns `point id → layer → artifact key` for the assertions that follow.
fn joined(out: &ViewportOut) -> BTreeMap<u64, BTreeMap<String, String>> {
    let by_id: BTreeMap<(String, u64), &ArtifactOut> = out
        .artifacts
        .iter()
        .map(|a| ((a.layer.clone(), a.tessera_id.raw()), a))
        .collect();
    let mut joined: BTreeMap<u64, BTreeMap<String, String>> = BTreeMap::new();
    for column in &out.points.membership {
        assert_eq!(
            column.ids.len(),
            out.points.len(),
            "a column is one value per point"
        );
        assert!(
            out.artifacts.iter().any(|a| a.layer == column.layer),
            "a column exists only for a layer with an artifact in this response: {}",
            column.layer
        );
        for (point, id) in out.points.tessera_ids.iter().zip(&column.ids) {
            let Some(id) = id else { continue };
            let artifact = by_id.get(&(column.layer.clone(), *id)).unwrap_or_else(|| {
                panic!(
                    "point {point} names artifact {id} on {}, which is not in the artifacts \
                         frame of the same response",
                    column.layer
                )
            });
            joined.entry(*point).or_default().insert(
                column.layer.clone(),
                artifact.key.clone().unwrap_or_default(),
            );
        }
    }
    joined
}

/// What the tree column should say for every point of the fixture, given which keys were served.
fn expected_tree_key(source: u64, served: &BTreeSet<&str>) -> Option<&'static str> {
    let chain: &[&'static str] = match source {
        0..=99 => &["a1", "a", "root"],
        100..=199 => &["a2", "a", "root"],
        200..=299 => &["b1", "b", "root"],
        300..=399 => &["b2", "b", "root"],
        _ => &[],
    };
    chain.iter().copied().find(|k| served.contains(k))
}

fn assert_tree_column(fx: &Fixture, out: &ViewportOut, sources: impl Iterator<Item = u64>) {
    let served: BTreeSet<&str> = out
        .artifacts
        .iter()
        .filter(|a| a.layer == TREE)
        .filter_map(|a| a.key.as_deref())
        .collect();
    let joined = joined(out);
    let mut checked = 0usize;
    for source in sources {
        let id = fx.point_id(source);
        if !out.points.tessera_ids.contains(&id) {
            continue;
        }
        checked += 1;
        let got = joined
            .get(&id)
            .and_then(|m| m.get(TREE))
            .map(String::as_str);
        assert_eq!(
            got,
            expected_tree_key(source, &served),
            "source {source} (served keys {served:?})"
        );
    }
    assert!(checked > 0, "the sweep must have checked some points");
}

/// The headline: the frontier is the leaves, so each point names its leaf; a budget that climbs
/// to the children renames every point to its child — the leaf is withheld and the column never
/// says it existed; a point under no served artifact is null.
#[test]
fn a_point_names_its_deepest_served_ancestor_and_null_under_none() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(TREE, None, true, None))
        .unwrap();
    plant(&fx, &engine);
    let credential = full_coverage_credential();

    let leaves = viewport(&engine, &credential, WHOLE_MAP, LayerSelection::All, None);
    assert_eq!(
        leaves.points.membership.len(),
        1,
        "one column, for the one layer served"
    );
    assert_tree_column(&fx, &leaves, 0..N_ITEMS);
    let named = leaves.points.membership[0]
        .ids
        .iter()
        .filter(|id| id.is_some())
        .count();
    let nulls = leaves.points.len() - named;
    assert!(
        named > 0 && nulls > 0,
        "both a named and a null point were exercised"
    );

    let climbed = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::All,
        Some(3),
    );
    let keys: BTreeSet<&str> = climbed
        .artifacts
        .iter()
        .filter_map(|a| a.key.as_deref())
        .collect();
    assert_eq!(keys, BTreeSet::from(["a", "b"]), "the cut climbed");
    assert_tree_column(&fx, &climbed, 0..N_ITEMS);

    // Whole tree served (no pruning): still the deepest, which is the leaf.
    let engine_whole = {
        let fx2 = fixture();
        let e = fx2.open();
        e.register_layer(declaration(TREE, None, false, None))
            .unwrap();
        plant(&fx2, &e);
        (fx2, e)
    };
    let whole = viewport(
        &engine_whole.1,
        &credential,
        WHOLE_MAP,
        LayerSelection::All,
        None,
    );
    assert_eq!(
        whole.artifacts.len(),
        7,
        "every node passes and none is pruned"
    );
    assert_tree_column(&engine_whole.0, &whole, 0..N_ITEMS);
}

/// **A masked principal served a coarser cut gets the coarser ancestor.** Under a count criterion
/// the grandchildren fail for a principal who sees a third of the corpus while the children pass,
/// so the same point names `a` for them and `a1` for a principal who sees everything — and the
/// narrower response never carries `a1`'s identifier anywhere.
#[test]
fn a_masked_principal_is_named_the_coarser_served_ancestor() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(
            TREE,
            Some(ExistenceCriterion::Count(50)),
            true,
            None,
        ))
        .unwrap();
    plant(&fx, &engine);

    let broad = viewport(
        &engine,
        &full_coverage_credential(),
        WHOLE_MAP,
        LayerSelection::All,
        None,
    );
    let broad_keys: BTreeSet<&str> = broad
        .artifacts
        .iter()
        .filter_map(|a| a.key.as_deref())
        .collect();
    assert_eq!(broad_keys, BTreeSet::from(["a1", "a2", "b1", "b2"]));
    assert_tree_column(&fx, &broad, 0..N_ITEMS);

    let narrow = viewport(
        &engine,
        &subset_credential(),
        WHOLE_MAP,
        LayerSelection::All,
        None,
    );
    let narrow_keys: BTreeSet<&str> = narrow
        .artifacts
        .iter()
        .filter_map(|a| a.key.as_deref())
        .collect();
    assert_eq!(
        narrow_keys,
        BTreeSet::from(["a", "b"]),
        "the grandchildren fail the bar on a third of their members and the children pass"
    );
    assert_tree_column(&fx, &narrow, 0..N_ITEMS);

    // One of `a1`'s members this principal can see and was served: named `a`, not `a1`.
    let by_point = joined(&narrow);
    let a1_point = (0..100)
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .map(|s| fx.point_id(s))
        .find(|id| narrow.points.tessera_ids.contains(id))
        .expect("a visible member of a1 is served in a whole-map viewport");
    assert_eq!(by_point[&a1_point][TREE], "a");
    assert!(
        !narrow
            .artifacts
            .iter()
            .any(|a| a.key.as_deref() == Some("a1")),
        "and a1 is nowhere in the response"
    );
}

/// A request naming no layers gets no column; one naming a layer with nothing served gets none
/// either; a layer named alongside gets only its own.
#[test]
fn the_column_set_follows_the_layers_the_response_served() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(TREE, None, true, None))
        .unwrap();
    plant(&fx, &engine);
    let credential = full_coverage_credential();

    let none = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::Named(&[]),
        None,
    );
    assert!(none.artifacts.is_empty());
    assert!(none.points.membership.is_empty());
    assert!(
        !none.points.is_empty(),
        "points still flow without a column"
    );

    // A viewport over a region the tree does not reach: points, no artifact, no column.
    let mut away = None;
    for bbox in [[900.0, 900.0, 1000.0, 1000.0], [0.0, 900.0, 100.0, 1000.0]] {
        let out = viewport(&engine, &credential, bbox, LayerSelection::All, None);
        if out.artifacts.is_empty() && !out.points.is_empty() {
            away = Some(out);
            break;
        }
    }
    if let Some(away) = away {
        assert!(away.points.membership.is_empty());
    }

    let unknown = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::Named(&["nobody/registered"]),
        None,
    );
    assert!(unknown.points.membership.is_empty());

    let named = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::Named(&[TREE]),
        None,
    );
    assert_eq!(named.points.membership.len(), 1);
    assert_eq!(named.points.membership[0].layer, TREE);
}

/// **A dependent layer's column is its artifacts' own membership.** A label published with the
/// members it describes names itself on those points; one published with no members is a
/// candidate nowhere and so is in neither the frame nor the column; and a label whose cluster
/// this response does not hold is absent from the frame and so from the column.
#[test]
fn a_dependent_layer_resolves_over_its_own_members() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(TREE, None, true, None))
        .unwrap();
    engine.register_layer(labels()).unwrap();
    plant(&fx, &engine);
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![
                IncomingArtifact::attached(
                    Some("l-a1".into()),
                    fx.members(0..100),
                    vec![IncomingContent::new(
                        vec!["a1 topic".into()],
                        fx.members(0..100),
                    )],
                    IncomingAttachment {
                        layer: TREE.into(),
                        level: 0,
                        key: "a1".into(),
                    },
                ),
                IncomingArtifact::attached(
                    Some("l-b1".into()),
                    Vec::new(),
                    vec![IncomingContent::new(
                        vec!["b1 topic".into()],
                        fx.members(200..300),
                    )],
                    IncomingAttachment {
                        layer: TREE.into(),
                        level: 0,
                        key: "b1".into(),
                    },
                ),
            ],
        )
        .unwrap();
    let credential = full_coverage_credential();

    let out = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::Named(&[TREE, LABELS]),
        None,
    );
    let by_point = joined(&out);
    let layers: Vec<&str> = out
        .points
        .membership
        .iter()
        .map(|c| c.layer.as_str())
        .collect();
    assert_eq!(layers, vec![TREE, LABELS], "columns in request order");
    let reversed = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::Named(&[LABELS, TREE]),
        None,
    );
    let layers: Vec<&str> = reversed
        .points
        .membership
        .iter()
        .map(|c| c.layer.as_str())
        .collect();
    assert_eq!(
        layers,
        vec![LABELS, TREE],
        "and the request's order, not the registry's"
    );
    let served_from = |range: std::ops::Range<u64>| {
        range
            .map(|s| fx.point_id(s))
            .find(|id| out.points.tessera_ids.contains(id))
            .expect("a member is served in a whole-map viewport")
    };
    let a1_point = served_from(0..100);
    assert_eq!(by_point[&a1_point][LABELS], "l-a1");
    assert_eq!(by_point[&a1_point][TREE], "a1");
    let b1_point = served_from(200..300);
    assert_eq!(by_point[&b1_point][TREE], "b1");
    assert!(
        !by_point[&b1_point].contains_key(LABELS),
        "a label with no members names no point"
    );
    assert!(
        !out.artifacts
            .iter()
            .any(|a| a.key.as_deref() == Some("l-b1")),
        "a memberless label has no visible member in any viewport, so it is no candidate and \
         is absent from the frame as well as the column"
    );

    // Cut to the children: `a1` goes, its label goes with it (decision 0089), and the column says
    // nothing about either.
    let climbed = viewport(
        &engine,
        &credential,
        WHOLE_MAP,
        LayerSelection::All,
        Some(3),
    );
    assert!(climbed.artifacts.iter().all(|a| a.layer == TREE));
    let layers: Vec<&str> = climbed
        .points
        .membership
        .iter()
        .map(|c| c.layer.as_str())
        .collect();
    assert_eq!(layers, vec![TREE]);
    joined(&climbed);
}

/// **Both layouts agree ordinal for ordinal** on the same corpus: the row-major route reads a leaf
/// and climbs, the artifact-major route intersects each served artifact's rows, and a point must
/// get the same answer from either. Swept over principals, viewports and budgets.
#[test]
fn the_two_layouts_answer_identically() {
    /// One case's column, keyed by source id.
    type Case = BTreeMap<u64, Option<u64>>;
    let mut answers: Vec<(ServingLayout, Vec<Case>)> = Vec::new();
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorList] {
        let fx = fixture();
        let engine = fx.open();
        engine
            .register_layer(declaration(
                TREE,
                Some(ExistenceCriterion::Count(20)),
                false,
                Some(layout),
            ))
            .unwrap();
        plant(&fx, &engine);
        fx.wait_for_publication(&engine, 1);
        let served_layout = viewport(
            &engine,
            &full_coverage_credential(),
            WHOLE_MAP,
            LayerSelection::All,
            None,
        );
        assert_eq!(
            engine.recorded_layout(TREE, 0),
            Some(layout),
            "the pin took"
        );
        assert_eq!(
            engine.layout_fallbacks(),
            0,
            "and the level was served as pinned"
        );
        drop(served_layout);

        let mut per_case = Vec::new();
        for credential in [full_coverage_credential(), subset_credential()] {
            for bbox in [
                WHOLE_MAP,
                [0.0, 0.0, 500.0, 500.0],
                [250.0, 250.0, 750.0, 750.0],
            ] {
                for budget in [None, Some(3), Some(1)] {
                    let out = viewport(&engine, &credential, bbox, LayerSelection::All, budget);
                    assert_tree_column(&fx, &out, 0..N_ITEMS);
                    let column = out.points.membership.first();
                    // Keyed by **source id**, because the two fixtures are two builds and an
                    // identifier is a permutation of an entity id assigned per build.
                    let by_source: BTreeMap<u64, u64> =
                        (0..N_ITEMS).map(|s| (fx.point_id(s), s)).collect();
                    let served_keys: BTreeMap<u64, String> = out
                        .artifacts
                        .iter()
                        .map(|a| (a.tessera_id.raw(), a.key.clone().unwrap()))
                        .collect();
                    let case: Case = out
                        .points
                        .tessera_ids
                        .iter()
                        .enumerate()
                        .map(|(i, id)| {
                            let key = column.and_then(|c| c.ids[i]).map(|a| {
                                served_keys[&a].as_bytes()[0] as u64 * 1000
                                    + served_keys[&a].len() as u64
                            });
                            (by_source[id], key)
                        })
                        .collect();
                    per_case.push(case);
                }
            }
        }
        answers.push((layout, per_case));
    }
    let (_, artifact_major) = &answers[0];
    let (_, row_major) = &answers[1];
    assert_eq!(artifact_major.len(), row_major.len());
    for (i, (a, r)) in artifact_major.iter().zip(row_major).enumerate() {
        assert!(!a.is_empty(), "case {i} served points");
        assert_eq!(
            a, r,
            "case {i}: the two layouts disagreed on the membership column"
        );
    }
}

/// **The measurement** — run with `--ignored --nocapture`. The column's per-response cost at the
/// fixture's scale, both layouts, isolated by differencing: the artifact pass is paid whether or
/// not any point is served and the gather is paid whether or not any layer is named, so
/// `(layers, k) − (layers, 0) − ((no layers, k) − (no layers, 0))` is the column alone.
#[test]
#[ignore = "measurement: the column's per-response cost by differencing — run with --ignored --nocapture"]
fn measure_the_column_cost() {
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorList] {
        let fx = fixture();
        let engine = fx.open_uncapped();
        engine
            .register_layer(declaration(TREE, None, false, Some(layout)))
            .unwrap();
        // A four-deep tree over the whole fixture: 4 + 16 + 64 + 256 nodes, the leaves ~39 points.
        let mut nodes = Vec::new();
        let n = N_ITEMS;
        for d in 1..=4u64 {
            let fanout = 4u64.pow(d as u32);
            let width = n / fanout;
            for i in 0..fanout {
                let key = format!("d{d}-{i}");
                let parent = (d > 1).then(|| format!("d{}-{}", d - 1, i / 4));
                nodes.push(node(
                    &fx,
                    &key,
                    parent.as_deref(),
                    (i * width)..((i + 1) * width),
                ));
            }
        }
        engine.publish_artifacts(TREE.into(), 0, nodes).unwrap();
        fx.wait_for_publication(&engine, 1);
        assert_eq!(engine.recorded_layout(TREE, 0), Some(layout));
        let credential = full_coverage_credential();
        let session = engine.authorise(&credential).unwrap();
        let run = |layers: LayerSelection, k: usize, budget: Option<u32>| {
            engine
                .viewport(
                    &session,
                    ViewportRequest::new("s0", 2, WHOLE_MAP, k)
                        .layers(layers)
                        .artifact_budget(budget),
                )
                .unwrap()
        };
        let time = |layers: LayerSelection, k: usize, budget: Option<u32>| {
            run(layers, k, budget);
            let runs = 30;
            let t = std::time::Instant::now();
            for _ in 0..runs {
                run(layers, k, budget);
            }
            t.elapsed() / runs
        };
        let k = N_ITEMS as usize;
        for budget in [None, Some(64), Some(16)] {
            let out = run(LayerSelection::All, k, budget);
            let served = out.artifacts.len();
            let points = out.points.len();
            assert_eq!(engine.layout_fallbacks(), 0);
            let layers_k = time(LayerSelection::All, k, budget);
            let layers_0 = time(LayerSelection::All, 0, budget);
            let none_k = time(LayerSelection::Named(&[]), k, budget);
            let none_0 = time(LayerSelection::Named(&[]), 0, budget);
            let column = layers_k
                .saturating_sub(layers_0)
                .saturating_sub(none_k.saturating_sub(none_0));
            println!(
                "{layout:?} budget {budget:?}: {served} served, {points} points — column ≈ \
                 {column:?} (layers,k {layers_k:?}; layers,0 {layers_0:?}; none,k {none_k:?}; \
                 none,0 {none_0:?})"
            );
        }
    }
}

/// **A label carries its cluster's masked count** (D13): the number beside a label is the
/// target's, as this principal sees it, so the two agree in one response — and a label whose
/// target this response does not hold is absent, so there is no count to disagree with.
#[test]
fn a_dependent_artifact_carries_its_targets_masked_count() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(
            TREE,
            Some(ExistenceCriterion::Count(50)),
            true,
            None,
        ))
        .unwrap();
    engine.register_layer(labels()).unwrap();
    plant(&fx, &engine);
    // A label over **ten** of `a1`'s hundred members — its own count is ten for a broad principal,
    // which is exactly the number that must not be served.
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::attached(
                Some("l-a1".into()),
                fx.members(0..10),
                vec![IncomingContent::new(
                    vec!["a1 topic".into()],
                    fx.members(0..10),
                )],
                IncomingAttachment {
                    layer: TREE.into(),
                    level: 0,
                    key: "a1".into(),
                },
            )],
        )
        .unwrap();

    for credential in [full_coverage_credential(), subset_credential()] {
        let out = viewport(&engine, &credential, WHOLE_MAP, LayerSelection::All, None);
        let by_key: BTreeMap<&str, &ArtifactOut> = out
            .artifacts
            .iter()
            .map(|a| (a.key.as_deref().unwrap(), a))
            .collect();
        match by_key.get("a1") {
            Some(target) => {
                let label = by_key
                    .get("l-a1")
                    .expect("the label is served beside its cluster");
                assert_eq!(
                    label.masked_count, target.masked_count,
                    "the label's count is its cluster's"
                );
                assert!(
                    target.masked_count > 10,
                    "and not the label's own membership"
                );
            }
            // The subset principal: `a1` fails the bar and `a` is served instead, so the label —
            // describing an artifact this response does not hold — is absent whole.
            None => {
                assert!(by_key.contains_key("a"));
                assert!(
                    !by_key.contains_key("l-a1"),
                    "a label whose cluster is withheld is withheld with it"
                );
            }
        }
    }
    // Both arms ran: the broad principal serves `a1`, the narrow one does not.
    let broad = viewport(
        &engine,
        &full_coverage_credential(),
        WHOLE_MAP,
        LayerSelection::All,
        None,
    );
    assert!(broad
        .artifacts
        .iter()
        .any(|a| a.key.as_deref() == Some("a1")));
    let narrow = viewport(
        &engine,
        &subset_credential(),
        WHOLE_MAP,
        LayerSelection::All,
        None,
    );
    assert!(!narrow
        .artifacts
        .iter()
        .any(|a| a.key.as_deref() == Some("a1")));
}

/// A flat layer whose two artifacts overlap — the shape a multi-membership layer has.
fn flat_overlapping(layout: ServingLayout) -> LayerDeclaration {
    let mut declaration = declaration("clusters/flat", None, false, Some(layout));
    declaration.hierarchy = Hierarchy {
        kind: HierarchyKind::Flat,
        prune_children: false,
    };
    declaration
}

/// **On a flat layer with overlapping artifacts the tie is the lowest `tessera_id`, on both
/// layouts** (`dag-hierarchies.md` §6). Two served artifacts hold every point of the overlap at
/// one depth; the artifact-major route iterates a map of the served set and would otherwise
/// answer in whichever order it met them, and the row-major route reads a list of labels and
/// would otherwise answer with the first. Asserted against the rule, and the two layouts against
/// each other by source id.
#[test]
fn a_flat_overlap_names_the_lowest_identifier_on_both_layouts() {
    const FLAT: &str = "clusters/flat";
    let mut answers: Vec<BTreeMap<u64, Option<&'static str>>> = Vec::new();
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorList] {
        let fx = fixture();
        let engine = fx.open();
        engine.register_layer(flat_overlapping(layout)).unwrap();
        engine
            .publish_artifacts(
                FLAT.into(),
                0,
                vec![
                    IncomingArtifact::from_entities(Some("left".into()), fx.members(0..250)),
                    IncomingArtifact::from_entities(Some("right".into()), fx.members(150..400)),
                ],
            )
            .unwrap();
        fx.wait_for_publication(&engine, 1);
        let out = viewport(
            &engine,
            &full_coverage_credential(),
            WHOLE_MAP,
            LayerSelection::All,
            None,
        );
        assert_eq!(
            engine.recorded_layout(FLAT, 0),
            Some(layout),
            "the pin took"
        );
        assert_eq!(
            engine.layout_fallbacks(),
            0,
            "and the level was served as pinned"
        );
        let by_key: BTreeMap<&str, u64> = out
            .artifacts
            .iter()
            .map(|a| (a.key.as_deref().unwrap(), a.tessera_id.raw()))
            .collect();
        assert_eq!(by_key.len(), 2, "both artifacts pass and both are served");
        let lowest = if by_key["left"] < by_key["right"] {
            "left"
        } else {
            "right"
        };
        let joined = joined(&out);
        let mut by_source: BTreeMap<u64, Option<&'static str>> = BTreeMap::new();
        let mut on_overlap = 0usize;
        for source in 0..N_ITEMS {
            let id = fx.point_id(source);
            if !out.points.tessera_ids.contains(&id) {
                continue;
            }
            let got = joined
                .get(&id)
                .and_then(|m| m.get(FLAT))
                .map(String::as_str);
            let want = match source {
                0..=149 => Some("left"),
                150..=249 => {
                    on_overlap += 1;
                    Some(lowest)
                }
                250..=399 => Some("right"),
                _ => None,
            };
            assert_eq!(got, want, "source {source} under {layout:?}");
            by_source.insert(source, want);
        }
        assert!(on_overlap > 0, "the overlap was exercised");
        answers.push(by_source);
    }
    assert_eq!(answers[0], answers[1], "the two layouts disagreed");
}
