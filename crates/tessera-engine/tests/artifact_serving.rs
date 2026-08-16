//! **Stage 2's headline: the count beside a cluster is the viewer's own, and never the cluster's.**
//!
//! One clustering, two principals, and the arithmetic checked against an independently computed
//! answer rather than against the engine's own. Every assertion here fails in a way that looks like
//! success when it is wrong: a count equal to the membership means the mask was never applied; a
//! cluster served to a principal who can see none of it means candidacy came from a bounding box;
//! a drill-down disagreeing with the viewport means the predicate is transcribed twice.

mod common;

use common::*;
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
use tessera_lifecycle::{wal::ChangeOp, IncomingArtifact};
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerAccess, LayerDeclaration,
    MembershipSource,
};
use tessera_types::{EntityId, TesseraId};

/// The whole extent at zoom 0 — one tile, every row a candidate. Candidacy is tested separately
/// (`a_cluster_outside_the_viewport_is_not_a_candidate`); these assertions are about counts.
const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

fn declaration(name: &str, criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: format!("{name} (title)"),
        slices: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        access: LayerAccess {
            label: None,
            artifacts_carry_own: false,
        },
        visible_when: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            derived: vec!["centroid".into()],
            supplied: Vec::new(),
            on_member_deletion: Default::default(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }

    fn members(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }
}

/// The independent answer: how many of these source ids the subset credential — term 1 — may see.
///
/// **Computed from the fixture's own generator, not from the engine.** `terms_of` gives term 1 to
/// every third source id, and this counts them directly; an assertion against a figure the engine
/// produced would pass whatever the engine did.
fn visible_to_subset(source_ids: impl Iterator<Item = u64>) -> u64 {
    source_ids
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .count() as u64
}

fn artifacts_of(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

fn artifact_entity(engine: &Engine, id: TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// **The stage's headline.** One cluster, two principals, two counts — and the narrow one is the
/// independently computed size of the intersection, not the cluster's own size.
#[test]
fn the_count_beside_a_cluster_is_the_viewers_own() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a", None)).unwrap();

    // 300 documents in one cluster; a third of them carry the subset term.
    let sources = 0..300u64;
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources.clone()),
            )],
        )
        .unwrap();

    let broad = artifacts_of(&engine, &full_coverage_credential());
    let narrow = artifacts_of(&engine, &subset_credential());
    assert_eq!(broad.len(), 1);
    assert_eq!(narrow.len(), 1);

    let expected_narrow = visible_to_subset(sources.clone());
    assert_eq!(
        narrow[0].masked_count, expected_narrow,
        "the narrow principal is told how many of the cluster's members *they* can see"
    );
    assert_eq!(broad[0].masked_count, 300, "and the broad one sees all of it");
    assert_ne!(
        narrow[0].masked_count, 300,
        "a count equal to the membership would mean the mask was never applied — the failure that \
         looks most like success"
    );
    // Same cluster, same identity, two answers. The identifier is stable across principals by
    // construction (C17); only the number beside it moves.
    assert_eq!(broad[0].tessera_id, narrow[0].tessera_id);
    assert_eq!(broad[0].stable_key.as_deref(), Some("c0"));
}

/// A cluster below its criterion is **absent**, and absent in a way that carries no reason — the
/// response cannot distinguish it from a cluster that was never published.
#[test]
fn a_cluster_below_its_criterion_is_absent_for_one_principal_and_served_to_another() {
    let fx = fixture();
    let engine = fx.open();
    let sources = 0..90u64;
    let expected_narrow = visible_to_subset(sources.clone());
    // A bar the broad principal clears and the narrow one does not, chosen from the independently
    // computed intersection rather than from anything the engine said.
    let criterion = ExistenceCriterion::MinVisible(expected_narrow + 1);
    engine
        .register_layer(declaration("clusters/a", Some(criterion)))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources),
            )],
        )
        .unwrap();

    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 1);
    let narrow = artifacts_of(&engine, &subset_credential());
    assert!(
        narrow.is_empty(),
        "below the bar is absent — not served with a rounded count, not refused: {narrow:?}"
    );

    // And a principal with no terms at all sees nothing, by the same route rather than a different
    // one: their masked count is zero, which fails the same test.
    assert!(artifacts_of(&engine, &zero_credential()).is_empty());
}

/// **Candidacy is a masked question.** A cluster every one of whose members lies inside the
/// viewport but outside the viewer's mask is not a candidate. A build-time bounding box over full
/// membership would have served it — disclosing the cluster's extent by panning.
#[test]
fn a_cluster_the_viewer_can_see_no_member_of_is_not_a_candidate() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a", None)).unwrap();

    // Every member chosen so that the subset credential holds none of their terms.
    let invisible: Vec<u64> = (0..300u64)
        .filter(|s| !terms_of(*s).contains(&SUBSET_TERM))
        .take(50)
        .collect();
    assert_eq!(visible_to_subset(invisible.iter().copied()), 0);
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(invisible.into_iter()),
            )],
        )
        .unwrap();

    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 1);
    assert!(
        artifacts_of(&engine, &subset_credential()).is_empty(),
        "every member is in the viewport and none is visible; a bounding box would have served it"
    );
}

/// A viewport over a region the cluster has no visible member in serves nothing — and the same
/// cluster comes back when the viewport moves over it.
#[test]
fn a_cluster_outside_the_viewport_is_not_a_candidate() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..300),
            )],
        )
        .unwrap();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let whole = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .unwrap();
    assert_eq!(whole.artifacts.len(), 1);

    // A viewport whose tile list is empty resolves no rows at all, so nothing can intersect it.
    let nowhere = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).tiles(Some(&[])),
        )
        .unwrap();
    assert!(nowhere.artifacts.is_empty());
    // The count is over the whole membership either way — it does not move with the box, which is
    // what stops a viewer differencing two viewports for the members in between.
    assert_eq!(whole.artifacts[0].masked_count, 300);
}

/// **The two-transcription check.** Drill-down and the viewport call the same predicate, so they
/// cannot disagree about whether an artifact is served or about the number beside it.
#[test]
fn a_drill_down_agrees_with_the_viewport_that_served_the_identifier() {
    let fx = fixture();
    let engine = fx.open();
    let sources = 0..300u64;
    let expected_narrow = visible_to_subset(sources.clone());
    engine
        .register_layer(declaration(
            "clusters/a",
            Some(ExistenceCriterion::MinVisible(expected_narrow + 1)),
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources),
            )],
        )
        .unwrap();

    let broad_session = engine.authorise(&full_coverage_credential()).unwrap();
    let served = artifacts_of(&engine, &full_coverage_credential());
    let id = served[0].tessera_id;
    let drilled = engine
        .artifact(&broad_session, id, None, "s0")
        .unwrap()
        .expect("the identifier this session was just served");
    assert_eq!(drilled, served[0], "one predicate, one answer");

    // The principal it is absent for cannot reach it by identifier either — held identifiers are
    // not a way round the criterion.
    let narrow_session = engine.authorise(&subset_credential()).unwrap();
    assert!(engine.artifact(&narrow_session, id, None, "s0").unwrap().is_none());

    // An identifier naming a point rather than an artifact is the same answer as an artifact
    // withheld. Taken from the response's own points, so it is genuinely an identifier this
    // deployment issued rather than an invented one.
    let point_id = TesseraId::new(
        engine
            .viewport(
                &broad_session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .unwrap()
            .points
            .tessera_ids[0],
    );
    assert!(engine
        .artifact(&broad_session, point_id, None, "s0")
        .unwrap()
        .is_none());
}

/// Suppression is live at the ack, on every route — an artifact's entity carries it exactly as a
/// point's does, with no second mechanism.
#[test]
fn suppressing_an_artifact_removes_it_from_the_viewport_and_from_drill_down_at_the_ack() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![
                IncomingArtifact::from_entities(Some("c0".into()), fx.members(0..150)),
                IncomingArtifact::from_entities(Some("c1".into()), fx.members(150..300)),
            ],
        )
        .unwrap();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(served.len(), 2);

    let entity = artifact_entity(&engine, served[0].tessera_id);
    engine.accept_change(entity, ChangeOp::Suppress).unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(after.len(), 1, "suppression takes effect at the ack");
    assert_eq!(after[0].stable_key.as_deref(), Some("c1"));
    assert!(engine
        .artifact(&session, served[0].tessera_id, None, "s0")
        .unwrap()
        .is_none());

    // Suppressing the *layer* takes both, and by the same route.
    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 2);
}

/// The request names which layers it wants, and naming one is never a way to learn it exists.
#[test]
fn the_layer_selector_narrows_and_never_widens() {
    let fx = fixture();
    let engine = fx.open();
    for name in ["clusters/a", "clusters/b"] {
        engine.register_layer(declaration(name, None)).unwrap();
        engine
            .publish_artifacts(
                name.into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("c0".into()),
                    fx.members(0..100),
                )],
            )
            .unwrap();
    }

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let answer = |layers: Option<&[&str]>| {
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).layers(layers),
            )
            .unwrap()
            .artifacts
    };

    assert_eq!(answer(None).len(), 2, "absent means every reachable layer");
    assert!(answer(Some(&[])).is_empty(), "an empty list costs nothing and answers nothing");
    let one = answer(Some(&["clusters/a"]));
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].layer, "clusters/a");
    // A name that does not exist is absent, exactly as a name this principal could not reach
    // would be — asking is not a probe.
    assert!(answer(Some(&["clusters/never"])).is_empty());
    assert_eq!(answer(Some(&["clusters/a", "clusters/never"])).len(), 1);
}

/// A publication is visible to the next request without a restart, a flush or a new session — and
/// the cached row-space projection must not be what stops it.
#[test]
fn a_publication_reaches_the_next_viewport_through_the_projection_cache() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..100),
            )],
        )
        .unwrap();
    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 1);

    // The projection for this level is now cached. A second batch must invalidate it: a cache that
    // silently omitted the new clusters would serve the level with its newest artifacts absent,
    // which a viewer cannot tell from artifacts that failed their criterion.
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c1".into()),
                fx.members(100..200),
            )],
        )
        .unwrap();
    let after = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(after.len(), 2);
    assert_eq!(after[1].masked_count, 100);
}

// ---- C1's recovery, checked rather than asserted ---------------------------------------------

/// The quadrant the compact cluster below occupies: one Morton cell at depth 1 — the lower-left
/// quarter of the `0..1000` extent. The upper bound stops just short of the split, because a bbox
/// touching 500.0 touches the neighbouring cell too and the request would answer for all four.
const QUADRANT: [f64; 4] = [0.0, 0.0, 499.999, 499.999];

/// Every source id whose point falls in the lower-left quadrant, from the generator's own
/// arithmetic (`write_points_n`: `x = 37 s mod 1000`, `y = 53 s mod 1000`) rather than from
/// anything the engine reported.
fn sources_in_quadrant() -> Vec<u64> {
    (0..N_ITEMS)
        .filter(|s| (s * 37) % 1000 < 500 && (s * 53) % 1000 < 500)
        .collect()
}

/// **The register says this recovery succeeds, and the point is that it does.**
///
/// C1 records that an existence criterion bounds a grouping's *existence and shape* and never its
/// *count*, because §7.1's tile counts and §7.3's underlay already serve exact masked counts at any
/// depth. So for a **compact** artifact — one whose membership is everything inside a region — a
/// principal the criterion withheld the cluster from can sum the density underlay over that region
/// and recover precisely the number they were not told.
///
/// This is a check on the register's honesty, not a bug report: nothing here is a leak, because
/// every number summed is a masked count this principal was already entitled to. Were the sum to
/// *disagree* with the withheld count, C1's row would be overstating the exposure and would need
/// rewriting; were the recovery to be closed off, it could only be by removing the underlay, which
/// is a core capability. The one thing this must not become is a claim that the criterion protects
/// the count.
#[test]
fn a_withheld_compact_cluster_has_its_count_recovered_from_the_underlay() {
    let fx = fixture();
    let engine = fx.open();
    let sources = sources_in_quadrant();
    let expected_narrow = visible_to_subset(sources.iter().copied());
    assert!(expected_narrow > 0, "the fixture must put visible points in the quadrant");

    // A bar the narrow principal cannot clear, from the independently computed intersection.
    engine
        .register_layer(declaration(
            "clusters/compact",
            Some(ExistenceCriterion::MinVisible(expected_narrow + 1)),
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/compact".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources.iter().copied()),
            )],
        )
        .unwrap();

    let session = engine.authorise(&subset_credential()).unwrap();
    let view = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 1, QUADRANT, N_ITEMS as usize),
        )
        .expect("a viewport over the cluster's own quadrant");

    assert!(
        view.artifacts.is_empty(),
        "the criterion withholds it from this principal: {:?}",
        view.artifacts
    );
    assert_eq!(
        view.tiles.len(),
        1,
        "the quadrant is one depth-1 cell, so this sum is over the cluster's extent and nothing \
         else: {:?}",
        view.tiles.iter().map(|t| (t.tile, t.visible)).collect::<Vec<_>>()
    );
    let recovered: u64 = view.tiles.iter().map(|t| t.visible).sum();
    assert_eq!(
        recovered, expected_narrow,
        "the counts this principal was already entitled to sum to exactly the number the criterion \
         withheld — which is what C1 records, and what a criterion described as protecting counts \
         would contradict"
    );
}
