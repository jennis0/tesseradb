//! **The `region` leaf: a shape is the rows inside it, exactly, for this principal**
//! (`selection-operand.md`; `polygon-membership.md` §8).
//!
//! The oracle is the fixture's own generator and the shape's direct test: every position is
//! quantised through the build's `fixed32`, tested against the canonical shape's own `contains`,
//! and masked by the principal's grant — none of which touches the descent, the boundary-cell
//! test under the mask, the cache or the routed tree, which is where a defect would live. What is
//! asserted: the count is the oracle's under two principals for a polygon, a circle and a box; the
//! verdict is exact and the same for both; `none_of` is the complement within the visible set;
//! two regions compose as intersection and union; past the cell budget the answer is a cover — a
//! superset, with a verdict that is the shape's and not the principal's; and the leaf by artifact
//! is the artifact's own masked count, an empty operand for an id that names nothing, one that is
//! suppressed and one withheld by the principal's criterion, with the three responses equal.

mod common;

use std::sync::Arc;

use common::*;
use tessera_engine::filter::{FilterExpr, RegionLeaf};
use tessera_engine::shapes::ShapeF64;
use tessera_engine::{Engine, LayerSelection, RegionVerdict, ViewportOut, ViewportRequest};
use tessera_lifecycle::{wal::ChangeOp, IncomingArtifact};
use tessera_spatial::fixed32;
use tessera_spatial::shape::Shape;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource,
};
use tessera_types::{EntityId, TesseraId};

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// The fixture's positions — `write_points_n`'s own arithmetic, restated so the oracle reads
/// the generator and not the bundle.
fn position(e: u64) -> (f64, f64) {
    (((e * 37) % 1000) as f64, ((e * 53) % 1000) as f64)
}

fn quantised(e: u64) -> (u32, u32) {
    let ext = extent();
    let (x, y) = position(e);
    (
        fixed32(x, ext.x_min, ext.x_max),
        fixed32(y, ext.y_min, ext.y_max),
    )
}

fn canonical(shape: ShapeF64) -> Arc<Shape> {
    Arc::new(shape.canonical(&extent()).expect("a well-formed shape").0)
}

fn region(shape: &Arc<Shape>) -> FilterExpr {
    FilterExpr::Region(RegionLeaf::Shape(Arc::clone(shape)))
}

fn subset_sees(e: u64) -> bool {
    terms_of(e).contains(&SUBSET_TERM)
}

/// The oracle: how many of the principal's visible items lie inside the shape.
fn inside(shape: &Shape, visible: impl Fn(u64) -> bool) -> u64 {
    (0..N_ITEMS)
        .filter(|&e| visible(e) && shape.contains(quantised(e)))
        .count() as u64
}

fn viewport(engine: &Engine, credential: &[u8], filter: Option<FilterExpr>) -> ViewportOut {
    let session = engine.authorise(credential).unwrap();
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize);
    if let Some(filter) = filter {
        request = request.filter(filter);
    }
    engine.viewport(&session, request).expect("a viewport answers")
}

fn matched(out: &ViewportOut) -> u64 {
    out.tiles.iter().map(|t| t.matched).sum()
}

fn visible(out: &ViewportOut) -> u64 {
    out.tiles.iter().map(|t| t.visible).sum()
}

fn lasso() -> Arc<Shape> {
    canonical(ShapeF64::Polygon(vec![vec![vec![
        (120.0, 80.0),
        (640.0, 140.0),
        (880.0, 560.0),
        (500.0, 930.0),
        (90.0, 610.0),
        (330.0, 420.0),
    ]]]))
}

fn circle() -> Arc<Shape> {
    canonical(ShapeF64::Circle {
        cx: 500.0,
        cy: 500.0,
        r: 260.0,
    })
}

fn bbox() -> Arc<Shape> {
    canonical(ShapeF64::Bbox {
        min_x: 250.0,
        min_y: 100.0,
        max_x: 700.0,
        max_y: 800.0,
    })
}

#[test]
fn a_region_counts_exactly_for_each_principal_and_the_verdict_is_the_shapes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_uncapped(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));

    for (name, shape) in [("lasso", lasso()), ("circle", circle()), ("bbox", bbox())] {
        let full = viewport(&engine, &full_coverage_credential(), Some(region(&shape)));
        let narrow = viewport(&engine, &subset_credential(), Some(region(&shape)));
        let want_full = inside(&shape, |_| true);
        let want_narrow = inside(&shape, subset_sees);
        assert!(want_full > 0 && want_full < N_ITEMS, "{name}: a real subset");
        assert_eq!(matched(&full), want_full, "{name}: the broad principal's count");
        assert_eq!(matched(&narrow), want_narrow, "{name}: the narrow principal's count");
        assert_eq!(full.region, Some(RegionVerdict::Exact), "{name}: exact");
        assert_eq!(narrow.region, full.region, "{name}: one verdict for every principal");
        // Every served point is inside — the marks and the number agree.
        let served: u64 = full.tiles.iter().map(|t| t.served).sum();
        assert_eq!(served, want_full, "{name}: θ is saturated, so every match is served");
        // `visible` is untouched by the filter (I12: a filter moves `matched`, never `visible`).
        assert_eq!(visible(&full), N_ITEMS);
        // A second ask is the cache's hit, and the same answer.
        let again = viewport(&engine, &subset_credential(), Some(region(&shape)));
        assert_eq!(matched(&again), want_narrow, "{name}: the cached decomposition");
    }
    assert!(
        engine.region_cache_stats().hits >= 3,
        "the decomposition is shared: {:?}",
        engine.region_cache_stats()
    );
    // No region leaf, no verdict.
    assert_eq!(viewport(&engine, &subset_credential(), None).region, None);
}

#[test]
fn a_negated_region_is_the_complement_and_two_regions_compose() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_uncapped(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    let a = lasso();
    let b = circle();

    for credential in [full_coverage_credential(), subset_credential()] {
        let inside_a = viewport(&engine, &credential, Some(region(&a)));
        let outside_a = viewport(
            &engine,
            &credential,
            Some(FilterExpr::NoneOf(vec![region(&a)])),
        );
        assert_eq!(
            matched(&inside_a) + matched(&outside_a),
            visible(&inside_a),
            "inside and outside partition the visible set"
        );
        assert_eq!(outside_a.region, Some(RegionVerdict::Exact));

        let both = viewport(
            &engine,
            &credential,
            Some(FilterExpr::AllOf(vec![region(&a), region(&b)])),
        );
        let either = viewport(
            &engine,
            &credential,
            Some(FilterExpr::AnyOf(vec![region(&a), region(&b)])),
        );
        let sees = |e: u64| credential == full_coverage_credential() || subset_sees(e);
        let want_both = (0..N_ITEMS)
            .filter(|&e| sees(e) && a.contains(quantised(e)) && b.contains(quantised(e)))
            .count() as u64;
        let want_either = (0..N_ITEMS)
            .filter(|&e| sees(e) && (a.contains(quantised(e)) || b.contains(quantised(e))))
            .count() as u64;
        assert_eq!(matched(&both), want_both, "all_of is the intersection");
        assert_eq!(matched(&either), want_either, "any_of is the union");

        // Inside the county but not the city: `all_of: [region(a), none_of: [region(b)]]`.
        let a_not_b = viewport(
            &engine,
            &credential,
            Some(FilterExpr::AllOf(vec![
                region(&a),
                FilterExpr::NoneOf(vec![region(&b)]),
            ])),
        );
        assert_eq!(matched(&a_not_b), matched(&inside_a) - want_both);
    }
}

#[test]
fn past_the_cell_budget_the_answer_is_a_cover_whose_verdict_is_the_shapes_alone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_uncapped(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    let shape = lasso();
    let exact_full = inside(&shape, |_| true);
    let exact_narrow = inside(&shape, subset_sees);

    // A budget a lasso's boundary cannot fit at the grid: the descent stops shallow.
    engine.set_max_region_cells(16);
    let full = viewport(&engine, &full_coverage_credential(), Some(region(&shape)));
    let narrow = viewport(&engine, &subset_credential(), Some(region(&shape)));
    let Some(RegionVerdict::Cover { depth }) = full.region else {
        panic!("a cover, not {:?}", full.region);
    };
    assert!(depth < 16, "the descent stopped above the grid: depth {depth}");
    assert_eq!(narrow.region, full.region, "the verdict is a function of the shape and the grid");
    assert!(matched(&full) >= exact_full, "a cover is a superset");
    assert!(matched(&narrow) >= exact_narrow, "for every principal");
    assert!(matched(&full) < N_ITEMS, "and not the whole map");

    // Back at the default budget the same shape is exact again — the budget is in the key.
    engine.set_max_region_cells(tessera_engine::DEFAULT_MAX_REGION_CELLS);
    let exact = viewport(&engine, &full_coverage_credential(), Some(region(&shape)));
    assert_eq!(exact.region, Some(RegionVerdict::Exact));
    assert_eq!(matched(&exact), exact_full);
}

fn declaration(name: &str, criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

fn by_artifact(id: TesseraId) -> FilterExpr {
    FilterExpr::Region(RegionLeaf::Artifact(id))
}

/// The response with the artifacts frame taken out, so two requests naming no layer can be
/// compared whole: tiles, points, sub-cells, stamp, staleness and the verdict.
fn without_artifacts(mut out: ViewportOut) -> ViewportOut {
    out.artifacts.clear();
    out
}

#[test]
fn the_leaf_by_artifact_is_its_masked_count_and_an_empty_operand_wherever_the_artifact_is_not_served(
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine =
        open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    // A criterion the broad principal clears and the narrow one does not: 300 members, of which
    // the subset credential sees every third.
    engine
        .register_layer(declaration("clusters/a", Some(ExistenceCriterion::Count(200))))
        .unwrap();
    let map = source_to_new_map(&root, "v00000");
    let members: Vec<EntityId> = (0..300u64).map(|s| EntityId::new(map[&s])).collect();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(Some("c0".into()), members)],
        )
        .unwrap();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let served = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).layers(LayerSelection::All),
        )
        .unwrap()
        .artifacts;
    assert_eq!(served.len(), 1, "the broad principal is served the cluster");
    let id = served[0].tessera_id;
    assert_eq!(served[0].masked_count, 300);

    // The leaf is the artifact's own membership, under this principal's mask.
    let full = viewport(&engine, &full_coverage_credential(), Some(by_artifact(id)));
    assert_eq!(matched(&full), 300);
    assert_eq!(full.region, Some(RegionVerdict::Exact), "by artifact is always exact");
    let served_points: u64 = full.tiles.iter().map(|t| t.served).sum();
    assert_eq!(served_points, 300.min(engine_cap(&engine)));

    // The narrow principal is below the criterion: the artifact is withheld from them, and so the
    // leaf is empty — the same answer as an id that names nothing.
    let unknown = viewport(
        &engine,
        &subset_credential(),
        Some(by_artifact(TesseraId::new(0x7777_7777_7777_7777))),
    );
    let withheld = viewport(&engine, &subset_credential(), Some(by_artifact(id)));
    assert_eq!(matched(&unknown), 0);
    assert_eq!(
        without_artifacts(withheld),
        without_artifacts(unknown.clone()),
        "withheld and unknown are one response"
    );
    // And so is the outside of it: `none_of` over an empty operand is every visible row.
    let outside_withheld = viewport(
        &engine,
        &subset_credential(),
        Some(FilterExpr::NoneOf(vec![by_artifact(id)])),
    );
    assert_eq!(matched(&outside_withheld), visible(&outside_withheld));

    // Suppressed: the broad principal, who was served it, now gets the empty operand too.
    let idset = engine.generation().bundle.manifest.identity.idset;
    let entity = engine.resolve_tessera_ids(&[id], idset).unwrap()[0].unwrap();
    engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    let suppressed = viewport(&engine, &full_coverage_credential(), Some(by_artifact(id)));
    let unknown_full = viewport(
        &engine,
        &full_coverage_credential(),
        Some(by_artifact(TesseraId::new(0x7777_7777_7777_7777))),
    );
    assert_eq!(matched(&suppressed), 0);
    assert_eq!(
        without_artifacts(suppressed),
        without_artifacts(unknown_full),
        "suppressed and unknown are one response"
    );
    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(
        matched(&viewport(&engine, &full_coverage_credential(), Some(by_artifact(id)))),
        300
    );
}

/// The publishing engine's mark cap — what bounds `served` where `matched` exceeds it.
fn engine_cap(engine: &Engine) -> u64 {
    engine.config().k_max_marks as u64
}
