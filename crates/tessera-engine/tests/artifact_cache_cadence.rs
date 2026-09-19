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
use rustc_hash::FxHashSet;
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
        scope: Default::default(),
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
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
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
    artifact.parent_keys = parent.into_iter().map(str::to_string).collect();
    artifact
}

/// A publication and the tick that publishes its delta into the level's row forms — two moments,
/// the acknowledgement meaning durable and the tick meaning served (`ingest.md` §1.3).
fn publish(engine: &Engine, layer: &str, artifacts: Vec<IncomingArtifact>) {
    engine
        .publish_artifacts(layer.into(), 0, artifacts)
        .expect("the publication is accepted");
    tick(engine);
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
    tick(&engine);

    assert_eq!(
        served(&engine, "clusters/a"),
        vec![("a0".to_string(), 150)],
        "the growth is in the first request after the tick that published it — a form the tick \
         had not reached would go on serving the count it had before, which nothing distinguishes \
         from a smaller membership"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "no form was rebuilt at all: the write applied its own delta to the form it moved, so \
         neither the level that was written to nor any other layer's was projected again"
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

/// **A dropped layer's derived structures leave both caches, and nobody else's do.**
///
/// Retention rather than correctness — a tombstoned name never resolves through the registry
/// again, so a form left behind could not be served to anyone — but it is retention that does not
/// come back: neither cache had a removal path, so each was bounded by the `(view, layer, level)`
/// triples the process had ever seen rather than the ones it holds, and at the campaign's target a
/// level's row form is gigabytes.
///
/// The layer beside it is the half that makes the assertion mean anything: `forget` has to be a
/// scalpel, and a cache cleared wholesale would pass a test that only counted the drop.
#[test]
fn dropping_a_layer_takes_its_row_form_and_its_lineage_with_it() {
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
        ],
    );
    let before_a = served(&engine, "clusters/a");
    let (held_rows, held_lineages) = engine.artifact_cache_held();
    assert!(
        held_rows > 0 && held_lineages > 0,
        "the request has to have left something held for the drop to remove"
    );

    engine
        .drop_layer("clusters/tree".into())
        .expect("a layer is droppable");

    let (after_rows, after_lineages) = engine.artifact_cache_held();
    assert!(
        after_rows < held_rows && after_lineages < held_lineages,
        "the dropped layer's entries are gone from both caches, not left pinned for the life of \
         the process"
    );
    assert_eq!(
        served(&engine, "clusters/tree"),
        Vec::new(),
        "and the layer is gone from the response, which is the registry's doing and not the \
         cache's"
    );
    assert_eq!(
        served(&engine, "clusters/a"),
        before_a,
        "the layer beside it kept its own form: `forget` names one layer, it does not clear"
    );
    assert_eq!(
        engine.artifact_cache_held(),
        (after_rows, after_lineages),
        "and serving `clusters/a` rebuilt nothing, so what survived the drop is what was held"
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

// ---------------------------------------------------------------------------------------------
// Derived geometry: the third structure held between requests, and the one that is per principal.
// ---------------------------------------------------------------------------------------------

/// A layer that declares the expensive property, so its artifacts have geometry worth holding.
fn hulled(name: &str) -> LayerDeclaration {
    let mut declaration = flat(name);
    declaration.content.computed = vec!["centroid".into(), "box".into(), "hull".into()];
    declaration
}

/// A pan: overlapping viewports that walk across the map, as a viewer dragging it produces.
///
/// Each step is a real `/v1/viewport` through the serving path, so what is being counted is the
/// derivations a *sequence of requests* makes rather than a rate computed from a key's shape.
fn pan(engine: &Engine, session: &tessera_engine::Session, steps: usize) {
    for step in 0..steps {
        let x = 40.0 * step as f64;
        engine
            .viewport(
                session,
                ViewportRequest::new("s0", 0, [x, 0.0, x + 700.0, 1000.0], N_ITEMS as usize),
            )
            .expect("a viewport");
    }
}

/// **The headline for derived geometry: a pan re-serves the same artifacts and derives them once.**
///
/// A hull is the most expensive thing a response does per artifact — a *measured* p90 of 14 ms and
/// 84 ms on the corpus root — and before this cache the identical request three times running cost
/// 2.4 s, 2.9 s and 2.8 s. The freshness half is the test below; neither is worth writing alone,
/// which is this file's own rule.
#[test]
fn a_pan_derives_each_shape_once_and_re_serves_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(hulled("clusters/a")).unwrap();
    publish(
        &engine,
        "clusters/a",
        (0..8)
            .map(|k| node(&fx, &format!("c{k}"), None, k * 400..(k + 1) * 400))
            .collect(),
    );

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    pan(&engine, &session, 8);

    let stats = engine.derived_cache_stats();
    assert!(
        stats.misses <= 8,
        "{} derivations for 8 artifacts — a pan is re-deriving",
        stats.misses
    );
    assert!(
        stats.hits > stats.misses,
        "{} hits against {} misses over a pan",
        stats.hits,
        stats.misses
    );
    println!(
        "pan of 8 viewports over 8 artifacts: {} hits, {} misses, hit rate {:.2}",
        stats.hits,
        stats.misses,
        stats.hit_rate().expect("the pan made lookups")
    );
}

/// The freshness half: a membership that moved is a shape that moved, on the very next request.
///
/// The key carries the level's own write counter, so a publication rotates it rather than editing
/// anything — the same rule the row form and the masked-count histogram beside it follow.
#[test]
fn a_write_re_derives_the_shape_it_moved() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(hulled("clusters/a")).unwrap();
    publish(&engine, "clusters/a", vec![node(&fx, "c0", None, 0..200)]);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let hull_of = |engine: &Engine| -> Vec<Vec<Vec<[u32; 2]>>> {
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .expect("a viewport")
            .artifacts
            .into_iter()
            .find(|a| a.layer == "clusters/a")
            .expect("the artifact is served")
            .derived
            .shape
            .expect("a declared hull")
    };
    let before = hull_of(&engine);
    assert_eq!(before, hull_of(&engine), "the second request re-derived");

    // Members join the artifact that exists — publication is append-only, so growth is how a
    // membership widens, and it is the ordinary write this cache has to notice.
    engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities(
                "c0".into(),
                fx.members(200..2_000),
            )],
        )
        .expect("points joining an artifact that exists is an ordinary write");
    tick(&engine);
    let after = hull_of(&engine);
    assert_ne!(
        before, after,
        "the shape from before the publication was served after it"
    );
}

/// **A shape is never shared across principals**, which is the term the whole safety of holding one
/// rests on: a hull is derived from `membership ∩ M_auth`, so one principal's is not an answer to
/// another's request for the same artifact.
#[test]
fn two_principals_over_one_artifact_get_two_shapes() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(hulled("clusters/a")).unwrap();
    publish(&engine, "clusters/a", vec![node(&fx, "c0", None, 0..600)]);

    let hull_for = |credential: &[u8]| -> Vec<Vec<Vec<[u32; 2]>>> {
        let session = engine.authorise(credential).unwrap();
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .expect("a viewport")
            .artifacts
            .into_iter()
            .find(|a| a.layer == "clusters/a")
            .expect("the artifact is served")
            .derived
            .shape
            .expect("a declared hull")
    };
    let broad = hull_for(&full_coverage_credential());
    let narrow = hull_for(&subset_credential());
    assert_ne!(
        broad, narrow,
        "the broad principal's shape was served to the narrow one"
    );
    assert_eq!(engine.derived_cache_stats().misses, 2);
}

// ---------------------------------------------------------------------------------------------
// The pruners: what a revoked session takes with it.
// ---------------------------------------------------------------------------------------------

/// The three per-session gauges a prune must move, read together so one assertion names all of
/// them: masked counts, the occupancy ladder and derived geometry.
fn per_session_entries(engine: &Engine) -> [usize; 3] {
    [
        engine.masked_count_cache_stats().entries,
        engine.occupancy_cache_stats().entries,
        engine.derived_cache_stats().entries,
    ]
}

/// A level served row-major, which is the only shape a masked-count histogram is built for: the
/// column has no per-artifact route to a count, so the histogram is what answers.
fn row_major(name: &str) -> LayerDeclaration {
    let mut declaration = flat(name);
    declaration.layout = Some(tessera_types::layer::ServingLayout::RowMajorLabel);
    declaration
}

/// Two layers, because the two caches want different ones: a masked-count histogram is built only
/// for a level served column-only, and a hull is what a shape is held for. The background fill is
/// off so every entry below is one a request made.
fn pruning_fixture(fx: &Fixture) -> Engine {
    let engine = fx.open();
    engine.set_occupancy_stage_for_test(false);
    engine.register_layer(row_major("clusters/counts")).unwrap();
    engine.register_layer(hulled("clusters/shapes")).unwrap();
    for layer in ["clusters/counts", "clusters/shapes"] {
        publish(
            &engine,
            layer,
            (0..4)
                .map(|k| node(fx, &format!("c{k}"), None, k * 2_000..(k + 1) * 2_000))
                .collect(),
        );
    }
    engine
}

/// One request that warms all three: artifacts give the masked counts and the hulls, the request
/// itself takes the occupancy ladder.
fn warm(engine: &Engine, session: &tessera_engine::Session) {
    let out = engine
        .viewport(
            session,
            ViewportRequest::new("s0", 4, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport");
    for layer in ["clusters/counts", "clusters/shapes"] {
        assert!(
            out.artifacts.iter().any(|a| a.layer == layer),
            "the request must serve {layer}'s artifacts, or it warms nothing"
        );
    }
}

/// **A prune drops every cache the session warmed, not only its row projections.**
///
/// The revoked session's masked-count histograms, occupancy rungs and derived shapes are each the
/// largest per-session structure in their own right — a masked-count histogram alone is ~4 B per
/// artifact — and nothing else holds them once the session is gone. The surviving session keeps
/// all of its own and is still served.
#[test]
fn a_prune_drops_the_tokens_masked_counts_occupancy_and_shapes() {
    let fx = fixture();
    let engine = pruning_fixture(&fx);

    let doomed = engine.authorise(&full_coverage_credential()).unwrap();
    warm(&engine, &doomed);
    let doomed_entries = per_session_entries(&engine);
    assert!(
        doomed_entries.iter().all(|&n| n > 0),
        "every cache under test must hold something for the doomed session: {doomed_entries:?}"
    );

    let survivor = engine.authorise(&subset_credential()).unwrap();
    warm(&engine, &survivor);
    let both = per_session_entries(&engine);
    for (cache, (&both, &doomed)) in both.iter().zip(doomed_entries.iter()).enumerate() {
        assert!(
            both > doomed,
            "cache {cache}: the second session must add entries of its own, {both} against \
             {doomed}"
        );
    }

    engine.prune_token(doomed.token_id());

    let after = per_session_entries(&engine);
    for (cache, ((&after, &both), &doomed)) in after
        .iter()
        .zip(both.iter())
        .zip(doomed_entries.iter())
        .enumerate()
    {
        assert_eq!(
            after,
            both - doomed,
            "cache {cache}: the prune must remove exactly the pruned session's entries"
        );
    }

    // And the survivor is still served, from what it still holds.
    warm(&engine, &survivor);
    assert_eq!(
        per_session_entries(&engine),
        after,
        "the surviving session re-reads from its own entries rather than rebuilding them"
    );
}

/// [`a_prune_drops_the_tokens_masked_counts_occupancy_and_shapes`] through the sweep's batch form,
/// which walks each cache once with a set membership test rather than once per victim.
#[test]
fn a_batch_prune_drops_the_same_caches_as_a_single_one() {
    let fx = fixture();
    let engine = pruning_fixture(&fx);

    let doomed = engine.authorise(&full_coverage_credential()).unwrap();
    warm(&engine, &doomed);
    let doomed_entries = per_session_entries(&engine);
    assert!(doomed_entries.iter().all(|&n| n > 0), "{doomed_entries:?}");

    let survivor = engine.authorise(&subset_credential()).unwrap();
    warm(&engine, &survivor);
    let both = per_session_entries(&engine);

    engine.prune_tokens(&FxHashSet::from_iter([doomed.token_id()]));

    let after = per_session_entries(&engine);
    for (cache, ((&after, &both), &doomed)) in after
        .iter()
        .zip(both.iter())
        .zip(doomed_entries.iter())
        .enumerate()
    {
        assert_eq!(
            after,
            both - doomed,
            "cache {cache}: a batch of one removes exactly what the single prune removes"
        );
    }
    warm(&engine, &survivor);
    assert_eq!(per_session_entries(&engine), after);
}
