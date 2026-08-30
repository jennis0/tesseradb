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
use tessera_spatial::shape::{Shape, Space};
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
    Arc::new(
        shape
            .canonical(Space::View, &extent())
            .expect("a well-formed shape")
            .0,
    )
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
    engine
        .viewport(&session, request)
        .expect("a viewport answers")
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
        assert!(
            want_full > 0 && want_full < N_ITEMS,
            "{name}: a real subset"
        );
        assert_eq!(
            matched(&full),
            want_full,
            "{name}: the broad principal's count"
        );
        assert_eq!(
            matched(&narrow),
            want_narrow,
            "{name}: the narrow principal's count"
        );
        assert_eq!(full.region, Some(RegionVerdict::Exact), "{name}: exact");
        assert_eq!(
            narrow.region, full.region,
            "{name}: one verdict for every principal"
        );
        // Every served point is inside — the marks and the number agree.
        let served: u64 = full.tiles.iter().map(|t| t.served).sum();
        assert_eq!(
            served, want_full,
            "{name}: θ is saturated, so every match is served"
        );
        // `visible` is untouched by the filter (I12: a filter moves `matched`, never `visible`).
        assert_eq!(visible(&full), N_ITEMS);
        // A second ask is the cache's hit, and the same answer.
        let again = viewport(&engine, &subset_credential(), Some(region(&shape)));
        assert_eq!(
            matched(&again),
            want_narrow,
            "{name}: the cached decomposition"
        );
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
    assert!(
        depth < 16,
        "the descent stopped above the grid: depth {depth}"
    );
    assert_eq!(
        narrow.region, full.region,
        "the verdict is a function of the shape and the grid"
    );
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
    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    // A criterion the broad principal clears and the narrow one does not: 300 members, of which
    // the subset credential sees every third.
    engine
        .register_layer(declaration(
            "clusters/a",
            Some(ExistenceCriterion::Count(200)),
        ))
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
    assert_eq!(
        full.region,
        Some(RegionVerdict::Exact),
        "by artifact is always exact"
    );
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
        matched(&viewport(
            &engine,
            &full_coverage_credential(),
            Some(by_artifact(id))
        )),
        300
    );
}

/// The publishing engine's mark cap — what bounds `served` where `matched` exceeds it.
fn engine_cap(engine: &Engine) -> u64 {
    engine.config().k_max_marks as u64
}

// =================================================================================================
// The region answer across a generation that renumbered the rows under it (I11)
// =================================================================================================

/// `MergePolicy::tier_width` — how many adjacent, same-tier extents select a merge.
const TIER_WIDTH: usize = 4;
/// Items per interleaved segment, above [`TIER_WIDTH`] so the merged order cycles through every
/// segment more than once and no row's position is a coincidence of the first cycle.
const ROWS_EACH: usize = 8;
/// The built corpus these cases start from. Small, because what is under test is the row space's
/// renumbering rather than a count over a large one.
const BASE_ITEMS: u64 = 64;

/// The `x` a flushed item is written at: segment `s` takes the positions congruent to `s` modulo
/// [`TIER_WIDTH`], at a constant `y`, so the segments interleave in Morton order and the merge's
/// permutation is not the identity. Clear of the extent's edge, which the ingest guard refuses.
fn flushed_x(s: usize, t: usize) -> f64 {
    ((t * TIER_WIDTH + s) * 20 + 10) as f64
}

const FLUSHED_Y: f64 = 5.0;

/// The shape these cases draw: a band that takes a **proper subset** of the flushed span — fifteen
/// of its thirty-two items — and no built item at all. A decomposition taken before the merge and
/// re-used after it names rows that now belong to other items, and a shape that covered the whole
/// span could not tell that apart from the right answer.
fn band() -> Arc<Shape> {
    canonical(ShapeF64::Bbox {
        min_x: 1.0,
        min_y: 1.0,
        max_x: 310.0,
        max_y: 10.0,
    })
}

/// The oracle for a corpus of `(x, y, visible)` items: how many lie inside the shape.
fn inside_positions(shape: &Shape, items: &[(f64, f64, bool)]) -> u64 {
    let ext = extent();
    items
        .iter()
        .filter(|&&(x, y, visible)| {
            visible
                && shape.contains((
                    fixed32(x, ext.x_min, ext.x_max),
                    fixed32(y, ext.y_min, ext.y_max),
                ))
        })
        .count() as u64
}

/// Every item the engine holds after [`flush_interleaved_segments`], as `(x, y, subset_sees)`.
fn all_positions() -> Vec<(f64, f64, bool)> {
    let mut items: Vec<(f64, f64, bool)> = (0..BASE_ITEMS)
        .map(|e| {
            let (x, y) = position(e);
            (x, y, subset_sees(e))
        })
        .collect();
    for s in 0..TIER_WIDTH {
        for t in 0..ROWS_EACH {
            items.push((flushed_x(s, t), FLUSHED_Y, t.is_multiple_of(2)));
        }
    }
    items
}

fn merge_engine(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        tessera_engine::EngineConfig {
            flush_max_age_secs: 3600,
            max_merged_segment_bytes: None,
            compaction: tessera_engine::CompactionSchedule::off(),
            ..common::config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

/// Flush [`TIER_WIDTH`] segments of [`ROWS_EACH`] items each, interleaved in Morton order — the
/// same fixture shape, and for the same reason, as `tests/merge.rs`'s: four segments already in
/// Morton order concatenate unchanged and every row keeps its id, which would make the case below
/// vacuously true.
fn flush_interleaved_segments(engine: &Engine) -> Vec<EntityId> {
    let mut entities = Vec::new();
    for s in 0..TIER_WIDTH {
        let rows: Vec<tessera_lifecycle::UnallocatedRow> = (0..ROWS_EACH)
            .map(|t| {
                let descriptors = if t.is_multiple_of(2) {
                    vec![b"0".to_vec(), b"1".to_vec()]
                } else {
                    vec![b"0".to_vec()]
                };
                tessera_lifecycle::UnallocatedRow {
                    external_id: Some(format!("ext-{s}-{t}").into_bytes()),
                    view: "s0".to_string(),
                    x: flushed_x(s, t),
                    y: FLUSHED_Y,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                }
            })
            .collect();
        entities.extend(
            engine
                .accept_ingest(rows, format!("batch-{s}"), [s as u8; 32])
                .expect("ingest is accepted"),
        );
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    }
    entities
}

/// Flush one more segment of items placed **outside the band**, so the corpus grows and the
/// segment list lengthens without moving the answer under test.
fn flush_filler_segment(engine: &Engine, n: usize) {
    let rows: Vec<tessera_lifecycle::UnallocatedRow> = (0..4)
        .map(|i| tessera_lifecycle::UnallocatedRow {
            external_id: Some(format!("filler-{n}-{i}").into_bytes()),
            view: "s0".to_string(),
            x: (700 + i * 20) as f64,
            y: 900.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            descriptors: vec![b"0".to_vec()],
        })
        .collect();
    engine
        .accept_ingest(rows, format!("filler-{n}"), [(200 + n) as u8; 32])
        .expect("ingest is accepted");
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the filler flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn segment_count(engine: &Engine) -> usize {
    engine.generation().bundle.partitions["default"].views["s0"]
        .segments
        .len()
}

fn rows_of(engine: &Engine, entities: &[EntityId]) -> Vec<Option<u32>> {
    let generation = engine.generation();
    let row_space = &generation.bundle.partitions["default"].views["s0"].row_space;
    entities
        .iter()
        .map(|e| row_space.row_of(*e).map(|r| r.raw()))
        .collect()
}

/// A viewport, retried past the bounded `ProjectionBuilding` a merge's refresh window answers with
/// (decision 0044), and past the `FragmentBuilding` a fresh publication answers an authorisation
/// with — a test that did not retry would be asserting those residuals do not exist.
fn region_viewport(engine: &Engine, credential: &[u8], shape: &Arc<Shape>) -> ViewportOut {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let request =
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).filter(region(shape));
        match engine
            .authorise(credential)
            .map_err(|e| e.to_string())
            .and_then(|session| {
                engine
                    .viewport(&session, request)
                    .map_err(|e| e.to_string())
            }) {
            Ok(out) => return out,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out retrying a region viewport: {e}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

fn served_ids(out: &ViewportOut) -> std::collections::BTreeSet<TesseraId> {
    out.points.iter().map(|(id, _)| id).collect()
}

/// **A region answer taken at one generation is not the answer at the next.** A merge renumbers
/// row ids inside the span it consumes, and a region's row set is a row-space artefact: the
/// decomposition held for the superseded generation names rows that afterwards belong to other
/// items. `RegionKey` carries `segments_version` so that entry can never be reached again (I11),
/// and `Executor::prune_region_cache` keeps superseded generations resident for two publications
/// — so within that window the key field is the whole of what stands between a request at the new
/// generation and a set of rows from the old one. Both halves are asserted: the answer is the new
/// row space's, and the ask that produced it missed the cache rather than hitting the old
/// decomposition.
///
/// The shape takes a proper subset of the merged span and no built item, so a stale answer is a
/// different **set** of items rather than merely a different count — which is the discrimination
/// `tests/merge.rs`'s module doc explains a count cannot make, since `tessera_id` is a function of
/// the entity and never of the row (I10).
///
/// **The two asks are taken at the same segment count, and that is load-bearing.** A stale
/// decomposition carries one range list per segment, and `RegionDecomposition::rows_under` debug-
/// asserts that count against the request's — so a case that asked across a merge directly would
/// trip that assertion in a debug build and never reach the answer. The filler flushes restore the
/// list's length, which leaves the renumbering itself as the only difference between the two
/// generations and makes a stale answer an observably wrong one rather than a panic in a debug
/// build.
///
/// Mutations this kills: dropping `segments_version` from `RegionKey`, or holding it constant —
/// as a simplification would, `prefix` and the digest being unchanged across a merge — which makes
/// the pre-merge decomposition a hit at the post-merge generation.
#[test]
fn a_region_answer_is_re_taken_at_the_generation_a_merge_renumbered_its_rows_in() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        BASE_ITEMS,
    );
    let engine = merge_engine(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let flushed = flush_interleaved_segments(&engine);

    let shape = band();
    let items = all_positions();
    let everything: Vec<(f64, f64, bool)> = items.iter().map(|&(x, y, _)| (x, y, true)).collect();
    let want_full = inside_positions(&shape, &everything);
    let want_narrow = inside_positions(&shape, &items);
    assert!(
        want_full > 0 && want_full < items.len() as u64,
        "the band must take a real subset, or this test proves nothing: {want_full}"
    );
    assert!(
        want_narrow > 0,
        "and a real subset for the narrow principal too"
    );

    let before_full = region_viewport(&engine, &full_coverage_credential(), &shape);
    let before_narrow = region_viewport(&engine, &subset_credential(), &shape);
    assert_eq!(
        matched(&before_full),
        want_full,
        "the broad principal's count before the merge"
    );
    assert_eq!(
        matched(&before_narrow),
        want_narrow,
        "the narrow principal's count before the merge"
    );
    let before_ids = served_ids(&before_full);
    assert!(
        !before_ids.is_empty(),
        "the region must serve marks, or this test proves nothing"
    );
    let segments_before = segment_count(&engine);
    let misses_before = engine.region_cache_stats().misses;

    // The merge: the same row ids mean different items afterwards.
    let version_before = engine.generation().segments_version;
    let rows_before = rows_of(&engine, &flushed);
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });
    engine.set_merge_for_test(false);
    let rows_after = rows_of(&engine, &flushed);
    assert!(
        rows_before.iter().all(Option::is_some) && rows_after.iter().all(Option::is_some),
        "every flushed entity must still have a row: {rows_before:?} -> {rows_after:?}"
    );
    assert_ne!(
        rows_before, rows_after,
        "this merge permuted nothing, so the case below is vacuous — see \
         flush_interleaved_segments"
    );
    assert!(
        engine.generation().segments_version > version_before,
        "the generation must advance, or the key term under test never moves"
    );

    // Back to the segment count the first answer was decomposed against — see this case's doc.
    let mut n = 0;
    while segment_count(&engine) < segments_before {
        flush_filler_segment(&engine, n);
        n += 1;
        assert!(
            n < 16,
            "the filler flushes must reach the earlier segment count"
        );
    }
    assert_eq!(
        segment_count(&engine),
        segments_before,
        "the second ask is taken against as many segments as the first"
    );

    let after_full = region_viewport(&engine, &full_coverage_credential(), &shape);
    let after_narrow = region_viewport(&engine, &subset_credential(), &shape);
    assert_eq!(
        matched(&after_full),
        want_full,
        "the broad principal's count at the new row space"
    );
    assert_eq!(
        matched(&after_narrow),
        want_narrow,
        "the narrow principal's count at the new row space"
    );
    assert_eq!(
        served_ids(&after_full),
        before_ids,
        "the same items are inside the band after the merge as before it — a stale decomposition \
         serves rows that now belong to other items"
    );
    assert!(
        engine.region_cache_stats().misses > misses_before,
        "the ask at the new generation missed the cache rather than reaching the decomposition \
         built against the old row space: {:?}",
        engine.region_cache_stats()
    );
}
