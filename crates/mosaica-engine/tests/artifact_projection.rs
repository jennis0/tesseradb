//! **The rung is the number a client draws by**: the declared level on a levelled layer and the
//! response-local parent-chain depth on a treed one — the depth in the forest the response's own
//! `parent_ids` links form, after the cut, so a re-rooted subtree starts at 0. A rung read from the
//! stored tree instead would draw a pruned leaf at depth 2 of a response whose links say it is a
//! root.
//!
//! Engine-level rather than over HTTP, for `artifact_filter_bit.rs`'s reason: the subject is what
//! is computed inside the trust boundary; the wire's column shapes are `mosaica-server`'s tests.

mod common;

use std::collections::BTreeMap;

use common::*;
use mosaica_corpus::Corpus;
use mosaica_engine::{ArtifactOut, Engine, LayerSelection, ViewportArtifactsRequest};
use mosaica_lifecycle::IncomingArtifact;
use mosaica_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, LevelDeclaration,
    MembershipSource,
};
use mosaica_types::EntityId;

const N: u64 = 3_000;
const SEED: u64 = 0x5EED;

const TREED: &str = "clusters/tree";
const PRUNED: &str = "clusters/pruned";
const TIERED: &str = "admin/tiers";

fn declaration(name: &str, kind: HierarchyKind, prune_children: bool) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind,
            prune_children,
        },
        // Derived content declared on purpose: the identity projection's saving is skipping this,
        // and the full projection carrying it is half of what the payload assertions check.
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

fn tiered_declaration(name: &str, levels: u32) -> LayerDeclaration {
    let mut d = declaration(name, HierarchyKind::Tiered, false);
    d.levels = (0..levels)
        .map(|level| LevelDeclaration {
            level,
            title: Some(format!("level {level}")),
            zoom: None,
        })
        .collect();
    d
}

struct Fixture {
    _tmp: tempfile::TempDir,
    engine: Engine,
    root: std::path::PathBuf,
}

impl Fixture {
    fn members(&self, sources: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        sources.map(|s| EntityId::new(map[&s])).collect()
    }

    fn node(
        &self,
        key: &str,
        parent: Option<&str>,
        sources: impl Iterator<Item = u64>,
    ) -> IncomingArtifact {
        let mut artifact =
            IncomingArtifact::from_entities(Some(key.into()), self.members(sources));
        artifact.parent_keys = parent.into_iter().map(str::to_string).collect();
        artifact
    }
}

/// The generator's corpus, plus three layers of this file's own:
///
/// - [`TREED`]: nested, unpruned — `root → {a, b}`, `a → {a1, a2}`, `b → {b1, b2}`;
/// - [`PRUNED`]: the same tree under `prune_children` — served as its leaves, every one of them a
///   response-local root;
/// - [`TIERED`]: two declared levels, one country over two states.
fn fixture() -> Fixture {
    let corpus =
        Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent");
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    build_corpus_fixture(&root, &points, &pairs, &corpus);

    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.set_background_refresh_for_test(false);
    let fx = Fixture {
        _tmp: tmp,
        engine,
        root,
    };

    for (name, prune) in [(TREED, false), (PRUNED, true)] {
        fx.engine
            .register_layer(declaration(name, HierarchyKind::Nested, prune))
            .unwrap();
        fx.engine
            .publish_artifacts(
                name.into(),
                0,
                vec![
                    fx.node("root", None, 0..600),
                    fx.node("a", Some("root"), 0..300),
                    fx.node("b", Some("root"), 300..600),
                    fx.node("a1", Some("a"), 0..150),
                    fx.node("a2", Some("a"), 150..300),
                    fx.node("b1", Some("b"), 300..450),
                    fx.node("b2", Some("b"), 450..600),
                ],
            )
            .unwrap();
    }
    fx.engine
        .register_layer(tiered_declaration(TIERED, 2))
        .unwrap();
    fx.engine
        .publish_artifacts(TIERED.into(), 0, vec![fx.node("country", None, 0..600)])
        .unwrap();
    fx.engine
        .publish_artifacts(
            TIERED.into(),
            1,
            vec![
                fx.node("state-a", Some("country"), 0..300),
                fx.node("state-b", Some("country"), 300..600),
            ],
        )
        .unwrap();
    fx
}

fn artifacts(
    engine: &Engine,
    grant: &str,
    layers: &[&str],
    budget: Option<u32>,
) -> Vec<ArtifactOut> {
    let session = engine.authorise(&grant_credential(grant)).unwrap();
    let request = ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX)
        .layers(LayerSelection::Named(layers))
        .budget(budget);
    engine
        .viewport_artifacts(&session, request)
        .expect("a viewport over the fixture")
        .artifacts()
}

/// The per-key rungs of one layer, from a full unfiltered whole-map request.
fn rungs_of(engine: &Engine, layer: &str, budget: Option<u32>) -> BTreeMap<String, u32> {
    artifacts(engine, "0,1", &[layer], budget)
        .into_iter()
        .map(|a| (a.key.expect("published with a key"), a.rung))
        .collect()
}

/// **Levelled rungs are the declared levels; treed rungs are chain depths.** The two disagree on
/// this very fixture — every treed artifact is stored at level 0 — which is what the column
/// exists to absorb.
#[test]
fn rungs_are_declared_levels_on_levelled_layers_and_chain_depths_on_treed_ones() {
    let fx = fixture();
    assert_eq!(
        rungs_of(&fx.engine, TIERED, None),
        BTreeMap::from([
            ("country".into(), 0),
            ("state-a".into(), 1),
            ("state-b".into(), 1),
        ]),
        "a levelled layer's rung is its declared level"
    );
    assert_eq!(
        rungs_of(&fx.engine, TREED, None),
        BTreeMap::from([
            ("root".into(), 0),
            ("a".into(), 1),
            ("b".into(), 1),
            ("a1".into(), 2),
            ("a2".into(), 2),
            ("b1".into(), 2),
            ("b2".into(), 2),
        ]),
        "a treed layer's rung is its depth in the response's own forest"
    );
}

/// **A cut that re-roots a subtree restarts its rungs at zero.** Two cuts, two shapes:
///
/// - the budget cut serves ancestors in place of descendants, so the served forest is the tree's
///   top and the rungs are the depths of what remains;
/// - `prune_children` drops every covered ancestor, so the served forest is the leaves — seven
///   stored depths collapsed to a response of roots, every `parent_ids` empty and every rung 0.
///
/// A rung read from the stored lineage instead of the response's own links fails the second case
/// with a plausible-looking 2.
#[test]
fn a_cut_that_reroots_a_subtree_restarts_its_rungs_at_zero() {
    let fx = fixture();
    assert_eq!(
        rungs_of(&fx.engine, TREED, Some(3)),
        BTreeMap::from([("root".into(), 0), ("a".into(), 1), ("b".into(), 1)]),
        "the budget climbs, and what survives keeps its depth in what survives"
    );
    let pruned = artifacts(&fx.engine, "0,1", &[PRUNED], None);
    assert_eq!(
        pruned
            .iter()
            .map(|a| a.key.clone().expect("published with a key"))
            .collect::<std::collections::BTreeSet<_>>(),
        ["a1", "a2", "b1", "b2"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        "pruning serves the leaves"
    );
    for leaf in &pruned {
        assert_eq!(
            (leaf.parent_ids.as_slice(), leaf.rung),
            (&[][..], 0),
            "{:?}: a leaf served without its ancestors is a root of the response's forest, \
             whatever its depth in the stored tree",
            leaf.key
        );
    }
}
