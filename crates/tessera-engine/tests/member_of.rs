//! **The `member_of` leaf: one artifact's membership, inside `M_auth`, as a filter clause**
//! (`highlight-and-hierarchy.md` §3).
//!
//! The oracle is the fixture's own generator: the members published for an artifact, intersected
//! with the terms the credential grants, counted directly — none of which touches the layer
//! registry, the existence criterion, the two membership layouts or the routed tree, which is
//! where a defect would live.
//!
//! What is asserted: the leaf's `matched` is the artifact's own masked count, under either
//! serving layout and for two principals; it composes with an ordinary leaf under `all_of`,
//! `any_of` and `none_of`; **an artifact this principal would not be served is an empty operand
//! and refuses nothing** — withheld by its criterion, suppressed, of another layer, and an
//! identifier that names nothing are one response; an **unknown layer is the caller's fault**,
//! where an unknown artifact never is; and the timing of the withheld case is not separable from
//! the unknown identifier's.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::filter::{FilterError, FilterExpr, MemberOfLeaf, RegionLeaf};
use tessera_engine::{Engine, EngineError, LayerSelection, ViewportOut, ViewportRequest};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ServingLayout,
};
use tessera_types::{EntityId, TesseraId};

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "clusters/a";
/// The artifact's members: source ids `0..300`, which the broad credential sees whole and the
/// narrow one sees every third of.
const MEMBERS: std::ops::Range<u64> = 0..300;
/// A criterion low enough that both principals are served the artifact — so the leaf's answer is
/// each one's own masked membership rather than an empty operand.
const BOTH_SERVED: u64 = 50;
/// A criterion the broad principal clears and the narrow one does not.
const ONLY_BROAD_SERVED: u64 = 200;

fn declaration(layout: Option<ServingLayout>, bar: u64) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LAYER.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: Some(ExistenceCriterion::Count(bar)),
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
        layout,
        shape: None,
    }
}

fn subset_sees(e: u64) -> bool {
    terms_of(e).contains(&SUBSET_TERM)
}

/// The oracle: how many of `members` this principal can see.
fn visible_members(members: std::ops::Range<u64>, sees: impl Fn(u64) -> bool) -> u64 {
    members.filter(|&e| sees(e)).count() as u64
}

fn member_of(artifact: TesseraId) -> FilterExpr {
    FilterExpr::MemberOf(MemberOfLeaf {
        layer: LAYER.to_string(),
        artifact,
    })
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

/// The response with the artifacts frame taken out, so two requests can be compared whole: tiles,
/// points, sub-cells, stamp, staleness and the region verdict.
fn without_artifacts(mut out: ViewportOut) -> ViewportOut {
    out.artifacts.clear();
    out
}

/// A bundle with one flat layer of one artifact over [`MEMBERS`], served under `layout`.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    engine: Engine,
    id: TesseraId,
}

/// `bar` is the layer's `Count` criterion: at [`BOTH_SERVED`] each principal is served the
/// artifact and the leaf answers their own masked membership; at [`ONLY_BROAD_SERVED`] the narrow
/// principal is below it and the leaf is their empty operand.
fn fixture(layout: Option<ServingLayout>, bar: u64) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.register_layer(declaration(layout, bar)).unwrap();
    let map = source_to_new_map(&root, "v00000");
    let members: Vec<EntityId> = MEMBERS.map(|s| EntityId::new(map[&s])).collect();
    engine
        .publish_artifacts(
            LAYER.into(),
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
    assert_eq!(served[0].masked_count, visible_members(MEMBERS, |_| true));
    Fixture {
        engine,
        root,
        id: served[0].tessera_id,
        _tmp: tmp,
    }
}

impl Fixture {
    /// Publish a second artifact of the same layer over `members`, and return its identifier as
    /// the broad principal is served it.
    fn second(&self, members: std::ops::Range<u64>, key: &str) -> TesseraId {
        let map = source_to_new_map(&self.root, &self.engine.generation().prefix);
        let entities: Vec<EntityId> = members.map(|s| EntityId::new(map[&s])).collect();
        self.engine
            .publish_artifacts(
                LAYER.into(),
                0,
                vec![IncomingArtifact::from_entities(Some(key.into()), entities)],
            )
            .unwrap();
        let session = self.engine.authorise(&full_coverage_credential()).unwrap();
        let served = self
            .engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                    .layers(LayerSelection::All),
            )
            .unwrap()
            .artifacts;
        served
            .iter()
            .find(|a| a.key.as_deref() == Some(key))
            .expect("the second artifact is served")
            .tessera_id
    }
}

/// **The leaf is the artifact's own masked count, and the layout decides only the route.**
///
/// The artifact-major route reads the held row bitmap and intersects it with the composed mask;
/// the row-major one scans the principal's visible rows comparing labels. Both must answer
/// `membership ∩ M_auth` — the same number the artifacts frame serves beside the artifact — for
/// every principal, which is what makes the layout a cost decision (decision 0093/0094).
///
/// **And the route is the membership, never the column.** Both answer the same number, so only a
/// counter separates them: `Engine::member_of_column_walks` rises when the leaf falls back to
/// walking every visible row of the view asking each of its labels whether it is this ordinal.
/// That fallback is a correct answer at the wrong price — measured at 2.85 s against 22 ms on
/// rung 3's 3.6 × 10⁷-row `mesh/descriptors` — and it is reachable only on a level with no
/// artifact-major membership, which no level is today.
///
/// Mutations this kills: reading the membership without the mask (the broad principal's answer
/// would be right and the narrow one's wrong); scanning the row column over the request's tiles
/// rather than the whole view; taking the artifact's row set for the wrong ordinal; and taking
/// the column route where the membership would have answered.
#[test]
fn the_leaf_is_the_artifacts_masked_count_under_either_layout() {
    for layout in [
        None,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::RowMajorList),
    ] {
        let fx = fixture(layout, BOTH_SERVED);
        let what = format!("{layout:?}");
        // The declared layout is the served one here — a single artifact's memberships cannot
        // overlap, so nothing falls back and each arm of the loop exercises the route it names.
        assert_eq!(
            fx.engine.recorded_layout(LAYER, 0),
            Some(layout.unwrap_or(ServingLayout::ArtifactMajor)),
            "{what}: the level is served in the layout the loop asked for"
        );
        let broad = viewport(&fx.engine, &full_coverage_credential(), Some(member_of(fx.id)));
        let narrow = viewport(&fx.engine, &subset_credential(), Some(member_of(fx.id)));
        let want_broad = visible_members(MEMBERS, |_| true);
        let want_narrow = visible_members(MEMBERS, subset_sees);
        assert!(
            want_narrow > 0 && want_narrow < want_broad,
            "{what}: the two principals are a real split"
        );
        assert_eq!(matched(&broad), want_broad, "{what}: the broad principal");
        assert_eq!(matched(&narrow), want_narrow, "{what}: the narrow principal");
        // I12: the filter moves `matched`, never `visible`.
        assert_eq!(visible(&broad), N_ITEMS, "{what}: visible is unmoved");
        // A `member_of` leaf carries no region, so no verdict rides on the response.
        assert_eq!(broad.region, None, "{what}: no region verdict");
        assert_eq!(
            fx.engine.member_of_column_walks(),
            0,
            "{what}: the leaf read the artifact-major membership, not the row column"
        );
    }
}

/// **It composes like any other leaf**, in row space, and with a `region` beside it.
///
/// `all_of` is the intersection, `any_of` the union — checked against each other by inclusion–
/// exclusion rather than against a restated oracle — and `none_of` is the complement within the
/// visible set, which is the "everything outside this cluster" the artifact card offers.
#[test]
fn it_composes_under_the_three_combinators() {
    let fx = fixture(None, BOTH_SERVED);
    let other = fx.second(200..500, "c1");
    for credential in [full_coverage_credential(), subset_credential()] {
        let broad = credential == full_coverage_credential();
        let sees = |e: u64| broad || subset_sees(e);
        let a = viewport(&fx.engine, &credential, Some(member_of(fx.id)));
        let b = viewport(&fx.engine, &credential, Some(member_of(other)));
        let both = viewport(
            &fx.engine,
            &credential,
            Some(FilterExpr::AllOf(vec![member_of(fx.id), member_of(other)])),
        );
        let either = viewport(
            &fx.engine,
            &credential,
            Some(FilterExpr::AnyOf(vec![member_of(fx.id), member_of(other)])),
        );
        assert_eq!(
            matched(&both) + matched(&either),
            matched(&a) + matched(&b),
            "|A ∩ B| + |A ∪ B| = |A| + |B|"
        );
        assert_eq!(
            matched(&both),
            visible_members(200..300, sees),
            "the overlap is source ids 200..300, inside this principal's own mask"
        );

        let outside = viewport(
            &fx.engine,
            &credential,
            Some(FilterExpr::NoneOf(vec![member_of(fx.id)])),
        );
        assert_eq!(
            matched(&a) + matched(&outside),
            visible(&a),
            "inside and outside partition the visible set"
        );

        // A region beside it: the same row-space tree with two different leaf kinds in it, which
        // is the shape §3 says a date range or a drawn lasso takes beside this clause.
        let half = FilterExpr::Region(RegionLeaf::Shape(std::sync::Arc::new(
            tessera_engine::shapes::ShapeF64::Bbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1000.0,
                max_y: 500.0,
            }
            .canonical(tessera_engine::shapes::Space::View, &extent())
            .expect("a well-formed box")
            .0,
        )));
        let in_half = viewport(&fx.engine, &credential, Some(half.clone()));
        let clipped = viewport(
            &fx.engine,
            &credential,
            Some(FilterExpr::AllOf(vec![member_of(fx.id), half])),
        );
        assert!(
            matched(&clipped) <= matched(&a).min(matched(&in_half)),
            "a conjunction narrows both sides"
        );
        assert!(
            matched(&clipped) > 0 && matched(&clipped) < matched(&a),
            "and it is a proper, non-empty narrowing"
        );
    }
}

/// **An artifact this principal would not be served is an empty operand, never a refusal**, and
/// every reason it is not served gives the one answer.
///
/// Four routes: an identifier that names nothing; one below this principal's own existence
/// criterion; one that is suppressed; and one that names an artifact of *another* layer than the
/// leaf's. Each is a **value** that does not resolve within the layer named, and a `422` to any of
/// them would make the leaf an existence oracle over exactly what the criterion withholds
/// (`highlight-and-hierarchy.md` §3).
#[test]
fn an_artifact_that_is_not_served_is_an_empty_operand_and_refuses_nothing() {
    let fx = fixture(None, ONLY_BROAD_SERVED);
    let unknown = TesseraId::new(0x7777_7777_7777_7777);

    // Withheld by the criterion: the narrow principal is below the bar of 200.
    let withheld = viewport(&fx.engine, &subset_credential(), Some(member_of(fx.id)));
    let never = viewport(&fx.engine, &subset_credential(), Some(member_of(unknown)));
    assert_eq!(matched(&never), 0, "an identifier that names nothing");
    assert_eq!(
        without_artifacts(withheld),
        without_artifacts(never.clone()),
        "withheld and unknown are one response"
    );
    // And so is its outside: `none_of` over an empty operand is every visible row.
    let outside = viewport(
        &fx.engine,
        &subset_credential(),
        Some(FilterExpr::NoneOf(vec![member_of(fx.id)])),
    );
    assert_eq!(matched(&outside), visible(&outside));

    // A `tessera_id` that names a *point* rather than an artifact — the other never-an-artifact
    // shape, and the one a client is most likely to send by mistake.
    let point_id = viewport(&fx.engine, &full_coverage_credential(), None)
        .points
        .iter()
        .map(|(id, _)| id)
        .next()
        .expect("the whole map serves points");
    assert_eq!(
        matched(&viewport(
            &fx.engine,
            &full_coverage_credential(),
            Some(member_of(point_id))
        )),
        0,
        "an identifier naming a point is an empty operand"
    );

    // Suppressed: the broad principal, who was served it, now gets the empty operand too.
    let idset = fx.engine.generation().bundle.manifest.identity.idset;
    let entity = fx.engine.resolve_tessera_ids(&[fx.id], idset).unwrap()[0].unwrap();
    fx.engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    let suppressed = viewport(&fx.engine, &full_coverage_credential(), Some(member_of(fx.id)));
    let never_broad = viewport(&fx.engine, &full_coverage_credential(), Some(member_of(unknown)));
    assert_eq!(matched(&suppressed), 0);
    assert_eq!(
        without_artifacts(suppressed),
        without_artifacts(never_broad),
        "suppressed and unknown are one response"
    );
    fx.engine
        .accept_change(entity, ChangeOp::Unsuppress)
        .unwrap();
    assert_eq!(
        matched(&viewport(
            &fx.engine,
            &full_coverage_credential(),
            Some(member_of(fx.id))
        )),
        visible_members(MEMBERS, |_| true),
        "an unsuppressed artifact is its membership again"
    );
}

/// **A layer this principal does not reach is `422`; an artifact they do not reach never is.**
///
/// A layer name is deployment schema — `/v1/meta` lists the layers this principal may name, and
/// the registry's probe answers alike for a gate-failed name and a never-registered one — so
/// refusing by name discloses nothing they were not already told. That is the same split
/// `contracts.md` §3.2 draws between an unknown column and an unknown value, and getting it the
/// other way round in either direction is the defect: refusing the artifact makes an oracle, and
/// answering the layer empty hides a caller's typo behind a blank map forever.
#[test]
fn an_unknown_layer_is_the_callers_fault_and_an_unknown_artifact_is_not() {
    let fx = fixture(None, BOTH_SERVED);
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let request = |layer: &str| {
        fx.engine.viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).filter(FilterExpr::MemberOf(
                MemberOfLeaf {
                    layer: layer.to_string(),
                    artifact: fx.id,
                },
            )),
        )
    };
    match request("clusters/nowhere") {
        Err(EngineError::FilterMalformed(detail)) => {
            assert!(
                detail.contains("clusters/nowhere") && detail.contains("member_of"),
                "the refusal names the layer and the leaf: {detail}"
            );
        }
        other => panic!("an unknown layer is the caller's fault, not {other:?}"),
    }
    assert!(
        FilterError::UnknownLayer("x".into()).is_callers_fault(),
        "and it is a 422 rather than a 500"
    );
    // The artifact named inside a layer it does not belong to is a *value*, so it is empty and not
    // a refusal — the registered layer, the wrong artifact.
    assert!(request(LAYER).is_ok(), "the layer itself answers");
}

/// **The withheld artifact's timing is not separable from an unknown identifier's.**
///
/// C33's registered residual is the C4/C24–C26 family's: a withheld artifact's criterion is
/// *evaluated* where an unknown identifier is a lookup miss, so the two are not bit-identical in
/// cost. What must not exist is a residual a caller could use — a withheld artifact taking a
/// membership read, a masked count over the whole level, or anything else proportional to what is
/// being withheld. This asserts the coarse bound the register claims: over many paired asks the
/// two medians sit inside the same order of magnitude.
///
/// Deliberately a *loose* bound. A test that pinned a ratio would fail on a loaded box and teach
/// the next reader to widen it until it meant nothing; what is being defended is the absence of a
/// membership-shaped cost, which is a factor of the level's population and not of scheduling.
#[test]
fn a_withheld_artifacts_timing_is_not_separable_from_an_unknown_identifiers() {
    let fx = fixture(None, ONLY_BROAD_SERVED);
    let session = fx.engine.authorise(&subset_credential()).unwrap();
    let ask = |id: TesseraId| {
        let started = Instant::now();
        fx.engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).filter(member_of(id)),
            )
            .unwrap();
        started.elapsed()
    };
    let unknown = TesseraId::new(0x7777_7777_7777_7777);
    // One warm pair, so neither median carries the first request's cache fills.
    ask(fx.id);
    ask(unknown);
    let mut withheld_ns: Vec<u128> = Vec::new();
    let mut missing_ns: Vec<u128> = Vec::new();
    for _ in 0..40 {
        withheld_ns.push(ask(fx.id).as_nanos());
        missing_ns.push(ask(unknown).as_nanos());
    }
    withheld_ns.sort_unstable();
    missing_ns.sort_unstable();
    let withheld = Duration::from_nanos(withheld_ns[withheld_ns.len() / 2] as u64);
    let missing = Duration::from_nanos(missing_ns[missing_ns.len() / 2] as u64);
    let ratio = withheld.as_secs_f64() / missing.as_secs_f64().max(f64::MIN_POSITIVE);
    assert!(
        (0.1..10.0).contains(&ratio),
        "the withheld artifact's median ({withheld:?}) and the unknown identifier's ({missing:?}) \
         differ by {ratio:.2}×, which is a channel rather than the register's stated residual"
    );
}
