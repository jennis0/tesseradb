//! **What a write invalidates, and what it leaves alone.**
//!
//! Two structures are derived from a level's records and held between requests: its row form
//! ([`tessera_engine`]'s `ArtifactProjections`) and its lineage (`Lineages`). Both were keyed on a
//! single store-wide version, so *any* artifact write invalidated *every* level's form in *every*
//! view — 138 s of rebuild at 10⁷ artifacts over 10⁹ rows, which is a cache that never survives a
//! write to be used (`design/artifact-serving-at-scale.md` §8.1, §8.2).
//!
//! **Both failures of the narrower key look like success and only one of them is loud.** Too
//! coarse, and the system is merely slow. Too narrow — a level whose version did not move when its
//! records did — and a request is served a stale row form, which is a wrong masked count with
//! nothing reporting a fault: a viewer cannot tell it from a membership that really is that size.
//! So every case here asserts the two halves together, the reuse and the freshness, and neither
//! alone would be worth writing.
//!
//! The counters are `Engine::artifact_cache_builds`. They count *derivations*, not requests, which
//! is the only thing that distinguishes a cache that is working from one that is being rebuilt
//! into the same answer.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// **No existence criterion**, so a count that moved is a membership that moved rather than an
/// artifact that appeared or vanished — the distinction these cases turn on.
fn declaration(name: &str, kind: HierarchyKind) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind,
            prune_children: true,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
    }
}

fn flat(name: &str) -> LayerDeclaration {
    declaration(name, HierarchyKind::Flat)
}

fn treed(name: &str) -> LayerDeclaration {
    declaration(name, HierarchyKind::Nested)
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
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }

    fn member(&self, source_id: u64) -> EntityId {
        self.members(source_id..source_id + 1)[0]
    }
}

/// One node of a planted tree: a key, its parent's key, and the source ids it holds.
fn node(
    fx: &Fixture,
    key: &str,
    parent: Option<&str>,
    sources: std::ops::Range<u64>,
) -> IncomingArtifact {
    let mut artifact = IncomingArtifact::from_entities(Some(key.into()), fx.members(sources));
    artifact.parent_key = parent.map(str::to_string);
    artifact
}

fn publish(engine: &Engine, layer: &str, artifacts: Vec<IncomingArtifact>) {
    engine
        .publish_artifacts(layer.into(), 0, artifacts)
        .expect("the publication is accepted");
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// One layer's served keys with their masked counts, sorted — what a request said about a layer,
/// in the form a later request can be compared against.
fn served(engine: &Engine, layer: &str) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = artifacts_of(engine)
        .into_iter()
        .filter(|a| a.layer == layer)
        .filter_map(|a| a.key.clone().map(|key| (key, a.masked_count)))
        .collect();
    out.sort();
    out
}

/// Request a fold and block until it has published, asserting it was not discarded.
fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **The headline: a write to one layer does not rebuild another layer's row form.**
///
/// Under the store-wide version this assertion was false by construction — every level in every
/// view rebuilt on every write — and no test could tell the difference, because both keys serve
/// the same answer. What separates them is the count of derivations, so that is what is asserted
/// here, together with the half that a too-narrow key would break: `a`'s own next request sees
/// the members that just joined.
#[test]
fn a_write_to_one_layer_leaves_another_layers_row_form_alone() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(flat("clusters/a")).unwrap();
    engine.register_layer(flat("clusters/b")).unwrap();
    publish(
        &engine,
        "clusters/a",
        vec![IncomingArtifact::from_entities(
            Some("a0".into()),
            fx.members(0..100),
        )],
    );
    publish(
        &engine,
        "clusters/b",
        vec![IncomingArtifact::from_entities(
            Some("b0".into()),
            fx.members(200..300),
        )],
    );

    // Cold: both layers' forms are derived, whatever the number of them is.
    let cold = engine.artifact_cache_builds().0;
    let before_b = served(&engine, "clusters/b");
    assert_eq!(served(&engine, "clusters/a"), vec![("a0".to_string(), 100)]);
    let warm = engine.artifact_cache_builds().0;
    assert!(warm > cold, "the first request has to derive something");

    artifacts_of(&engine);
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "a second request over an unmoved store derives nothing at all"
    );

    engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities(
                "a0".into(),
                fx.members(100..150),
            )],
        )
        .expect("points joining an artifact that exists is an ordinary write");

    assert_eq!(
        served(&engine, "clusters/a"),
        vec![("a0".to_string(), 150)],
        "the growth is in the very next request — a level whose version did not move would go on \
         serving the count it had before, which nothing distinguishes from a smaller membership"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm + 1,
        "exactly one form was rebuilt: the level that was written to, and no other layer's"
    );
    assert_eq!(
        served(&engine, "clusters/b"),
        before_b,
        "the layer nobody wrote to is serving what it served, from the form it already held"
    );
}

/// **A publication into a *third* layer leaves the first two alone**, which is the same rule at
/// the grain a suppression or a mint arrives at: the write is not to a level any earlier request
/// read.
#[test]
fn a_publication_into_a_new_layer_rebuilds_only_its_own_level() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(flat("clusters/a")).unwrap();
    publish(
        &engine,
        "clusters/a",
        vec![IncomingArtifact::from_entities(
            Some("a0".into()),
            fx.members(0..100),
        )],
    );
    artifacts_of(&engine);
    let warm = engine.artifact_cache_builds().0;

    engine.register_layer(flat("clusters/c")).unwrap();
    publish(
        &engine,
        "clusters/c",
        vec![IncomingArtifact::from_entities(
            Some("c0".into()),
            fx.members(300..400),
        )],
    );

    assert_eq!(served(&engine, "clusters/c"), vec![("c0".to_string(), 100)]);
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm + 1,
        "the new layer's own level is derived and nothing else is"
    );
}

/// **The lineage is a property of the tree, so two requests over one tree derive it once.**
///
/// It depends on neither the mask nor the viewport — which is why rebuilding it per request was
/// ~96 ms at a level of ten million, larger than everything the cut itself costs.
#[test]
fn two_requests_over_one_tree_derive_its_lineage_once() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree")).unwrap();
    publish(
        &engine,
        "clusters/tree",
        vec![
            node(&fx, "root", None, 0..300),
            node(&fx, "left", Some("root"), 0..100),
            node(&fx, "right", Some("root"), 100..200),
        ],
    );

    let cold = engine.artifact_cache_builds().1;
    let first = served(&engine, "clusters/tree");
    let warm = engine.artifact_cache_builds().1;
    assert!(warm > cold, "the first request has to derive the lineage");

    let second = served(&engine, "clusters/tree");
    assert_eq!(
        engine.artifact_cache_builds().1,
        warm,
        "the second request reuses it — the tree did not move, and neither did its lineage"
    );
    assert_eq!(
        first,
        vec![("left".to_string(), 100), ("right".to_string(), 100)],
        "the cut is the frontier: the root is covered by its own children"
    );
    assert_eq!(second, first, "and the reused lineage serves the same cut");
}

/// **An edge published after the lineage was held is in the next request's cut.**
///
/// The freshness half, and the one that fails silently: a held lineage that missed the new edge
/// would go on drawing the parent over a child that now covers it — every number in the response
/// correct on its own, and the wrong shape drawn.
#[test]
fn a_publication_that_adds_an_edge_is_in_the_next_requests_cut() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree")).unwrap();
    publish(
        &engine,
        "clusters/tree",
        vec![node(&fx, "root", None, 0..300)],
    );

    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("root".to_string(), 300)],
        "a root on its own is the whole cut"
    );
    let warm = engine.artifact_cache_builds().1;

    publish(
        &engine,
        "clusters/tree",
        vec![node(&fx, "child", Some("root"), 0..300)],
    );

    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("child".to_string(), 300)],
        "the new edge is seen: the child covers the root and replaces it"
    );
    assert!(
        engine.artifact_cache_builds().1 > warm,
        "which it could only do by rederiving the lineage the publication invalidated"
    );
}

/// **A fold rebuilds every row form and only the lineages it moved**, which are two different
/// answers to two different questions.
///
/// Row space renumbers globally at a fold, so every projection built over the old one names other
/// people's documents and all of them go. A lineage holds *ordinals*, which a fold preserves — it
/// writes a hole where it retired an artifact rather than closing the gap — so only a level this
/// fold actually changed needs its lineage again.
///
/// The fold does both itself, on its own thread, rather than leaving the first request after the
/// flip to absorb them.
#[test]
fn a_fold_rebuilds_every_row_form_and_only_the_lineages_it_moved() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(flat("clusters/a")).unwrap();
    engine.register_layer(treed("clusters/tree")).unwrap();
    publish(
        &engine,
        "clusters/a",
        vec![IncomingArtifact::from_entities(
            Some("a0".into()),
            fx.members(0..100),
        )],
    );
    publish(
        &engine,
        "clusters/tree",
        vec![
            node(&fx, "root", None, 500..800),
            node(&fx, "left", Some("root"), 500..600),
            node(&fx, "right", Some("root"), 600..700),
        ],
    );
    let before_tree = served(&engine, "clusters/tree");
    let (warm_rows, warm_lineages) = engine.artifact_cache_builds();

    // A member of `clusters/a` alone: the tree's own levels are untouched by what this fold
    // executes, which is what makes the two counters diverge.
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);

    let (folded_rows, folded_lineages) = engine.artifact_cache_builds();
    assert!(
        folded_rows >= warm_rows + 2,
        "every level's row form is rebuilt at the flip: row space renumbered under all of them"
    );
    assert_eq!(
        folded_lineages,
        warm_lineages + 1,
        "one lineage — the level the fold changed. The tree's ordinals did not move, so neither \
         did the lineage over them"
    );

    assert_eq!(
        served(&engine, "clusters/a"),
        vec![("a0".to_string(), 99)],
        "the deleted member is gone from the count the fold executed it in"
    );
    assert_eq!(
        served(&engine, "clusters/tree"),
        before_tree,
        "and the layer the fold did not touch serves exactly what it served"
    );
    assert_eq!(
        engine.artifact_cache_builds(),
        (folded_rows, folded_lineages),
        "the fold warmed both, so the first request after the flip derives nothing — which is the \
         stall it exists to keep off a request"
    );
}
