//! **Stage 5's headline: a viewport returns a cut through the tree, not the tree.**
//!
//! Three things are checked here and each fails in a way that looks like success. A cut that
//! served every passing node would draw parents over their own children and put a count beside a
//! count it contains. A cut that required an ancestor would blank the region under every hole a
//! proportional criterion opens — and open them it does, which is the second test. And a budget
//! that met itself by dropping nodes rather than by climbing would return the right *number* of
//! artifacts while claiming the dropped regions are empty.
//!
//! The lineage here is planted rather than drawn from the real condensed tree, which
//! `artifact_census` is the home for; what these want is the exact criterion arithmetic, which a
//! real clustering cannot be asked to produce on demand.

mod common;

use common::*;
use tessera_engine::{ArtifactOut, Engine, LayerSelection, LevelSelection, ViewportRequest};
use tessera_lifecycle::membership::IncomingAttachment;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// A treed layer: it declares no levels and its lineage is entirely in its edges
/// (decision 0082).
///
/// **Pruned by default here, which is not the declaration's own default.** Most of these cases are
/// about the frontier — which of two visible artifacts is the one drawn — and that only exists when
/// `prune_children` is on. `treed_whole` is the other half.
fn treed(name: &str, criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
    declaration(name, criterion, true)
}

/// The same, serving every passing artifact rather than the frontier — the declaration's own
/// default, and what a client that wants to nest or filter a subtree asks for.
fn treed_whole(name: &str, criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
    declaration(name, criterion, false)
}

fn declaration(
    name: &str,
    criterion: Option<ExistenceCriterion>,
    prune_children: bool,
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
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
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

/// One node of a planted tree: a key, its parent's key, and the source ids it holds.
fn node(
    fx: &Fixture,
    key: &str,
    parent: Option<&str>,
    sources: impl Iterator<Item = u64>,
) -> IncomingArtifact {
    let mut artifact = IncomingArtifact::from_entities(Some(key.into()), fx.members(sources));
    artifact.parent_keys = parent.into_iter().map(str::to_string).collect();
    artifact
}

fn artifacts_of(engine: &Engine, credential: &[u8], budget: Option<u32>) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).artifact_budget(budget),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

fn keys(artifacts: &[ArtifactOut]) -> Vec<String> {
    let mut names: Vec<String> = artifacts.iter().filter_map(|a| a.key.clone()).collect();
    names.sort();
    names
}

/// **The headline: where a parent and a child both pass, the child is what is drawn.**
///
/// Serving both would put the parent's shape over its own child's and a masked count beside a
/// count it contains — the failure that looks most like success, since every number in it is
/// correct on its own.
#[test]
fn a_passing_child_replaces_its_passing_parent() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "left", Some("root"), 0..100),
                node(&fx, "right", Some("root"), 100..200),
            ],
        )
        .unwrap();

    let served = artifacts_of(&engine, &full_coverage_credential(), None);
    assert_eq!(
        keys(&served),
        vec!["left", "right"],
        "the root is covered by its own children and is not drawn over them"
    );
    // And entities 200..300 are the root's alone — the non-covering case. Their region is dark
    // under this cut, which is what a frontier means: it is the finest statement available, not
    // a partition of the corpus.
}

/// **A hole in the lineage does not stop the child being served**, and the proportional criterion
/// is what opens one.
///
/// A parent at a small fraction of a large membership fails a `{ fraction = p }` requirement while its child
/// at a large fraction of a small one passes it — the child a strict subset of the parent
/// throughout. **A run that does not reproduce this gap has not exercised the proportional form at
/// all**, which is why the numbers here are chosen against the independently computed intersection
/// rather than against anything the engine said.
///
/// No disclosure follows either way: each node passed its own test. What a caller must expect is
/// that a layer declaring the proportional form has holes in its lineage.
#[test]
fn under_a_proportional_criterion_a_passing_child_sits_beneath_a_failing_parent() {
    let fx = fixture();
    let engine = fx.open();
    // The subset credential sees every third document, so a masked count is about a third of a
    // membership — and a *fraction* is about a third whatever the membership's size. The gap
    // therefore has to come from the memberships being different sizes relative to what the
    // viewer can see, which is what the child's narrow, fully visible membership supplies.
    let parent_sources: Vec<u64> = (0..300).collect();
    let child_sources: Vec<u64> = (0..300)
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .take(20)
        .collect();

    let parent_visible = parent_sources
        .iter()
        .filter(|s| terms_of(**s).contains(&SUBSET_TERM))
        .count() as f64;
    let parent_fraction = parent_visible / parent_sources.len() as f64;
    // Every one of the child's members is visible to this principal, so its fraction is 1.0.
    let bar = (parent_fraction + 1.0) / 2.0;
    assert!(
        parent_fraction < bar && bar < 1.0,
        "the fixture must put the bar between the two fractions, or the gap is untested \
         (parent {parent_fraction}, bar {bar})"
    );

    engine
        .register_layer(treed(
            "clusters/tree",
            Some(ExistenceCriterion::Fraction(bar)),
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "parent", None, parent_sources.iter().copied()),
                node(&fx, "child", Some("parent"), child_sources.iter().copied()),
            ],
        )
        .unwrap();

    // **The parent must fail its criterion, not merely be covered by the frontier**, and the two
    // are indistinguishable from the served set alone — which is how this test passes without
    // exercising the proportional form at all. So the same parent is published into a layer of its
    // own, with no child to cover it and the same bar, and asked separately.
    engine
        .register_layer(treed(
            "clusters/alone",
            Some(ExistenceCriterion::Fraction(bar)),
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/alone".into(),
            0,
            vec![node(
                &fx,
                "parent-alone",
                None,
                parent_sources.iter().copied(),
            )],
        )
        .unwrap();

    let served = artifacts_of(&engine, &subset_credential(), None);
    assert!(
        !keys(&served).contains(&"parent-alone".to_string()),
        "the parent fails the bar on its own account — without this the test passes whenever the \
         frontier hides the parent, which it does whatever the criterion said: {served:?}"
    );
    assert_eq!(
        keys(&served),
        vec!["child"],
        "the child clears the bar on its own members and the parent does not — the lineage has a \
         hole and the child is served through it"
    );

    // The broad principal sees both fractions at 1.0, so the parent now passes — and is dropped
    // from the tree layer by the frontier rather than by the criterion, while the same artifact
    // in the layer where nothing covers it is served. That pair is what separates the two
    // reasons an artifact can be absent, which no served set shows on its own.
    let broad = keys(&artifacts_of(&engine, &full_coverage_credential(), None));
    assert_eq!(broad, vec!["child", "parent-alone"]);
}

/// **A budget is met by serving ancestors, never by dropping nodes.** Dropping would return the
/// right number of artifacts while claiming the dropped regions hold nothing.
#[test]
fn a_budget_climbs_the_tree_rather_than_sampling_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..400),
                node(&fx, "a", Some("root"), 0..200),
                node(&fx, "b", Some("root"), 200..400),
                node(&fx, "a1", Some("a"), 0..100),
                node(&fx, "a2", Some("a"), 100..200),
                node(&fx, "b1", Some("b"), 200..300),
                node(&fx, "b2", Some("b"), 300..400),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["a1", "a2", "b1", "b2"],
        "with no budget the frontier is the leaves"
    );
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, Some(3))),
        vec!["a", "b"],
        "four leaves do not fit in three, so the cut climbs — and serves two, because a depth is \
         what it trades and not a count"
    );
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, Some(1))),
        vec!["root"]
    );
}

/// **Two budgets agree on every artifact both return.** A client widening its budget mid-pan sees
/// nodes appear beside the ones it had; it never sees one it kept change what it says.
#[test]
fn two_budgets_agree_on_every_artifact_both_return() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..400),
                node(&fx, "a", Some("root"), 0..200),
                node(&fx, "b", Some("root"), 200..400),
                node(&fx, "a1", Some("a"), 0..100),
                node(&fx, "b1", Some("b"), 200..300),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    let cuts: Vec<Vec<ArtifactOut>> = (1..=6)
        .map(|budget| artifacts_of(&engine, &credential, Some(budget)))
        .collect();
    for shallow in &cuts {
        for deep in &cuts {
            for a in shallow {
                let Some(b) = deep.iter().find(|other| other.tessera_id == a.tessera_id) else {
                    continue;
                };
                assert_eq!(
                    a.masked_count, b.masked_count,
                    "an artifact two cuts both return says the same thing in both"
                );
                assert_eq!(a.key, b.key);
            }
        }
    }
}

/// A layer with no edges is served exactly as it was before this stage — the flat case is a short
/// circuit and not a degenerate tree, so a budget has nothing to trade and takes nothing.
#[test]
fn a_flat_layer_is_untouched_by_a_budget() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/flat", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/flat".into(),
            0,
            vec![
                node(&fx, "c0", None, 0..100),
                node(&fx, "c1", None, 100..200),
                node(&fx, "c2", None, 200..300),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, Some(1))),
        vec!["c0", "c1", "c2"],
        "there is no lineage to climb, so every artifact that passed is served"
    );
}

// ---------------------------------------------------------------------------------------------
// What a lineage survives
// ---------------------------------------------------------------------------------------------

/// **A suppressed parent withholds itself and nothing else.** Its child is served on its own
/// account, because a node's verdict has no lineage input (decision 0080).
///
/// The alternative — suppressing a parent taking its subtree with it — reads as the cautious
/// choice and is a different product: it makes a hierarchy's edges into visibility terms, which is
/// exactly what an attachment is and a parent edge is not. It would also blank a region the viewer
/// is entitled to see, with nothing reporting why.
#[test]
fn a_suppressed_parent_does_not_take_its_child_with_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "leaf", Some("root"), 0..100),
                node(&fx, "other", Some("root"), 100..200),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["leaf", "other"]
    );

    // The root's own entity, which is what a suppression addresses. Two leaves do not fit in a
    // budget of one, so this cut is the root alone — which is how the test names it without an
    // ordinal, none of which crosses the boundary.
    let served = artifacts_of(&engine, &credential, Some(1));
    assert_eq!(keys(&served), vec!["root"]);
    let idset = engine.generation().bundle.manifest.identity.idset;
    let root_entity = engine
        .resolve_tessera_ids(&[served[0].tessera_id], idset)
        .unwrap()[0]
        .expect("it names what was issued");
    engine
        .accept_change(root_entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("the suppress is accepted");

    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["leaf", "other"],
        "the leaves are unmoved: their verdicts never asked their parent anything"
    );
    // And with the budget that climbed to the root, the root is now absent and the climb has
    // nowhere to go — so both leaves are served rather than nothing. Serving *more* than the
    // budget asked is the sound direction: a blank map is the failure, an overfull one is a
    // client's problem.
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, Some(1))),
        vec!["leaf", "other"]
    );
}

/// **A deleted parent leaves its child a root, and the fold rewrites nothing to make that true.**
///
/// This is the claim that lets the parent direction be the only durable one. An edge into a deleted
/// artifact resolves to a hole, and a hole is a lineage that ends — so the child is served on its
/// own account, which is correct because it passed its own test. Nothing has to find the child and
/// rewrite it, which is the bookkeeping that would have to be right at every fold and is where the
/// last two stages each found a defect.
///
/// Note what this does *not* share with an attachment. A label whose cluster is deleted is
/// **withheld** — its edge is a visibility term, and dropping it would serve the label of a hidden
/// cluster. A parent edge is not a visibility term, so the opposite answer is the right one, and
/// the two rules have to be kept apart by hand.
#[test]
fn a_deleted_parent_leaves_its_child_a_root() {
    let fx = fixture();
    let engine = fx.open();
    engine.set_background_refresh_for_test(false);
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "leaf", Some("root"), 0..100),
                node(&fx, "other", Some("root"), 100..200),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    let served = artifacts_of(&engine, &credential, Some(1));
    assert_eq!(keys(&served), vec!["root"]);
    let idset = engine.generation().bundle.manifest.identity.idset;
    let root_entity = engine
        .resolve_tessera_ids(&[served[0].tessera_id], idset)
        .unwrap()[0]
        .expect("it names what was issued");

    engine
        .accept_change(root_entity, tessera_lifecycle::wal::ChangeOp::Delete)
        .expect("the delete is accepted");
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["leaf", "other"],
        "the children stand on their own the moment the deletion is acked"
    );

    // And across the fold that executes it, where the root's ordinal becomes a hole for good.
    let before = engine.write_executor_stats();
    engine.request_fold();
    for _ in 0..2_000 {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["leaf", "other"],
        "the fold executed the deletion and the children are unmoved — their parent pointer names \
         a hole, which is a lineage that ends rather than one that is broken"
    );
}

/// **The whole visible tree, when the layer asks for it.** `prune_children` is a layer's rendering
/// choice, not a disclosure control (decision 0082 says so in as many words), and this is the half
/// of it a frontier cannot give: the client receives the ancestors as well as the leaves, which is
/// what lets it nest what it draws or filter to one subtree while still drawing the rest of the
/// map.
///
/// It was silently unavailable until 2026-08-18: the cut pruned unconditionally and never read the
/// declaration, so a layer asking for its whole tree was served a frontier and had no way to tell.
#[test]
fn a_layer_that_declines_pruning_is_served_its_whole_visible_tree() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(treed_whole("clusters/tree", None))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "left", Some("root"), 0..100),
                node(&fx, "right", Some("root"), 100..200),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, None)),
        vec!["left", "right", "root"],
        "the ancestor is served beside its children, which is what a client needs to nest them"
    );

    // **A budget still bites, and it climbs the same way.** What `prune_children` decides is which
    // artifacts are candidates to be moved, never what a depth means.
    assert_eq!(
        keys(&artifacts_of(&engine, &credential, Some(1))),
        vec!["root"]
    );
}

/// And the counts are the viewer's own on every artifact of that tree, ancestors included — a
/// parent's count is not the sum of its children's, because the members it keeps away from both
/// are counted in it and in neither of them.
#[test]
fn an_ancestors_count_is_its_own_and_not_the_sum_of_its_children() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(treed_whole("clusters/tree", None))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                // 200..300 are the root's and no child's — the non-covering case.
                node(&fx, "root", None, 0..300),
                node(&fx, "left", Some("root"), 0..100),
                node(&fx, "right", Some("root"), 100..200),
            ],
        )
        .unwrap();

    let served = artifacts_of(&engine, &full_coverage_credential(), None);
    let count = |key: &str| {
        served
            .iter()
            .find(|a| a.key.as_deref() == Some(key))
            .expect("served")
            .masked_count
    };
    assert_eq!(count("left"), 100);
    assert_eq!(count("right"), 100);
    assert_eq!(
        count("root"),
        300,
        "the root holds 100 members neither child does, so its count exceeds their sum"
    );
}

// ---------------------------------------------------------------------------------------------
// The tiered shape: levels carry the resolution, edges carry the structure
// ---------------------------------------------------------------------------------------------

/// A tiered layer: levels, with edges running between them — countries, states, counties.
fn tiered(name: &str, levels: u32) -> LayerDeclaration {
    let mut d = declaration(name, None, false);
    d.hierarchy.kind = HierarchyKind::Tiered;
    d.levels = (0..levels)
        .map(|level| tessera_types::layer::LevelDeclaration {
            level,
            title: Some(format!("level {level}")),
            zoom: None,
        })
        .collect();
    d
}

fn levelled_artifacts_of(
    engine: &Engine,
    credential: &[u8],
    budget: Option<u32>,
    layer: &str,
) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                .artifact_budget(budget)
                .layers(LayerSelection::Named(&[layer])),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// **A budget is inert on a tiered layer, and that is the ruling rather than an
/// oversight** (2026-08-18). Its edges are information about what contains what, not a ladder to
/// coarsen along: climbing them would substitute a state for its counties and draw one large
/// polygon across a region whose neighbours are still counties. Resolution is the client choosing
/// a level.
///
/// **What bounds an over-large response is the level, not a ceiling** (owner ruling 2026-08-28;
/// an earlier revision of this comment said a ceiling refuses, and none exists or will). A request
/// names the levels it wants, or names none and is answered at the levels the layer's own zoom
/// ranges declare for the depth asked at — see `a_request_naming_no_levels_follows_the_declared_map`.
/// The cut must never start sampling to reach a number, which is what this test is for.
#[test]
fn a_budget_is_inert_on_a_tiered_layer() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered("admin/boundaries", 2))
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![
                node(&fx, "state-a", Some("country"), 0..100),
                node(&fx, "state-b", Some("country"), 100..200),
            ],
        )
        .unwrap();

    let credential = full_coverage_credential();
    let whole = keys(&levelled_artifacts_of(
        &engine,
        &credential,
        None,
        "admin/boundaries",
    ));
    assert_eq!(whole, vec!["country", "state-a", "state-b"]);

    // A budget of one would have climbed a tree to its root. Here there is nothing to climb: the
    // levels are the resolution, and the response is unchanged.
    assert_eq!(
        keys(&levelled_artifacts_of(
            &engine,
            &credential,
            Some(1),
            "admin/boundaries"
        )),
        whole,
        "a budget has no depth to trade on a levelled layer, so it takes nothing"
    );
}

/// **The counts are per artifact on every level**, and a parent's is its own rather than the sum
/// of its children's — 200..300 belong to the country and to neither state.
#[test]
fn a_tiered_parents_count_is_its_own() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered("admin/boundaries", 2))
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![node(&fx, "state-a", Some("country"), 0..100)],
        )
        .unwrap();

    let served = levelled_artifacts_of(
        &engine,
        &full_coverage_credential(),
        None,
        "admin/boundaries",
    );
    let count = |key: &str| {
        served
            .iter()
            .find(|a| a.key.as_deref() == Some(key))
            .expect("served")
            .masked_count
    };
    assert_eq!(count("state-a"), 100);
    assert_eq!(count("country"), 300);
}

/// An edge running within one level is not a tiered edge, and the publish refuses it —
/// the layer's guarantee is that lineage never runs against the levels.
#[test]
fn a_tiered_edge_within_one_level_is_refused_at_publish() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered("admin/boundaries", 2))
        .unwrap();
    let err = engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![
                node(&fx, "state-a", None, 0..100),
                node(&fx, "state-b", Some("state-a"), 100..200),
            ],
        )
        .expect_err("a same-level parent is not a tiered edge");
    assert!(format!("{err}").contains("coarser"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The parent identifier on the wire, and the one rule that governs it
// ---------------------------------------------------------------------------------------------

/// **A client is given the structure of what it was served, and nothing else.**
///
/// A tiered layer's whole purpose is this: the client receives countries and states and
/// can tell which states are in which country, so it can nest what it draws or filter to one
/// subtree while still drawing the rest of the map.
#[test]
fn a_served_artifact_names_its_parent_when_the_parent_is_also_served() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered("admin/boundaries", 2))
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![
                node(&fx, "state-a", Some("country"), 0..100),
                node(&fx, "state-b", Some("country"), 100..200),
            ],
        )
        .unwrap();

    let served = levelled_artifacts_of(
        &engine,
        &full_coverage_credential(),
        None,
        "admin/boundaries",
    );
    let by_key = |key: &str| {
        served
            .iter()
            .find(|a| a.key.as_deref() == Some(key))
            .expect("served")
    };
    let country = by_key("country");
    assert!(country.parent_ids.is_empty(), "a root names no parent");
    assert_eq!(by_key("state-a").parent_ids, vec![country.tessera_id],
        "a state names the country it is in, by the identifier that country was served under"
    );
    assert_eq!(by_key("state-b").parent_ids, vec![country.tessera_id]);
}

/// **A parent that exists and was withheld is null, identically to a root.** That is the whole
/// disclosure rule for the field: naming it would tell this viewer that a coarser artifact exists
/// which they are not cleared to see, and the register's standing rule is that a withheld artifact
/// is indistinguishable from one that was never published.
#[test]
fn a_withheld_parent_is_named_no_differently_from_a_root() {
    let fx = fixture();
    let engine = fx.open();
    // A bar the country cannot clear for the narrow principal while its state can — the same
    // proportional gap the layer's own criterion opens, used here to withhold exactly one artifact.
    let parent_sources: Vec<u64> = (0..300).collect();
    let child_sources: Vec<u64> = (0..300)
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .take(20)
        .collect();
    let visible = parent_sources
        .iter()
        .filter(|s| terms_of(**s).contains(&SUBSET_TERM))
        .count() as f64;
    let bar = (visible / parent_sources.len() as f64 + 1.0) / 2.0;

    let mut declaration = tiered("admin/boundaries", 2);
    declaration.require_member_visibility = Some(ExistenceCriterion::Fraction(bar));
    engine.register_layer(declaration).unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, parent_sources.iter().copied())],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![node(
                &fx,
                "state",
                Some("country"),
                child_sources.iter().copied(),
            )],
        )
        .unwrap();

    let narrow = levelled_artifacts_of(&engine, &subset_credential(), None, "admin/boundaries");
    assert_eq!(
        keys(&narrow),
        vec!["state"],
        "the country is below the bar for this principal and the state is not"
    );
    assert!(narrow[0].parent_ids.is_empty(),
        "the state's parent exists and was withheld, so it reads exactly as a root does — the \
         alternative discloses that a coarser artifact is there"
    );

    // And the broad principal, who is served both, gets the link.
    let broad = levelled_artifacts_of(
        &engine,
        &full_coverage_credential(),
        None,
        "admin/boundaries",
    );
    let state = broad
        .iter()
        .find(|a| a.key.as_deref() == Some("state"))
        .expect("served");
    assert!(
        !state.parent_ids.is_empty(),
        "the same edge is named for a principal served both endpoints"
    );
}

/// The frontier drops ancestors, so a pruned layer carries no links — correct, and worth pinning:
/// the field's presence follows the response's own membership rather than the stored lineage.
#[test]
fn a_pruned_response_carries_no_parent_links() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "leaf", Some("root"), 0..100),
            ],
        )
        .unwrap();

    let served = artifacts_of(&engine, &full_coverage_credential(), None);
    assert_eq!(keys(&served), vec!["leaf"]);
    assert!(served[0].parent_ids.is_empty(),
        "the root was dropped by the frontier, so there is nothing in this response to name"
    );
}

/// **The same key at two levels is a taxonomy, not a cycle.** A key is unique per
/// `(layer, level)`, so an arXiv archive with no subject class is `hep-ph` at level 0 and `hep-ph`
/// at level 1, the second naming the first as its parent. Refusing that would make a caller rename
/// half their taxonomy to satisfy a check written for a tree, where an artifact naming its own key
/// really is naming itself.
#[test]
fn a_levelled_layer_may_carry_one_key_at_two_levels() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(tiered("taxonomy/arxiv", 2)).unwrap();
    engine
        .publish_artifacts(
            "taxonomy/arxiv".into(),
            0,
            vec![node(&fx, "hep-ph", None, 0..200)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "taxonomy/arxiv".into(),
            1,
            // The archive has no subclass, so the class carries the archive's own name.
            vec![node(&fx, "hep-ph", Some("hep-ph"), 0..200)],
        )
        .expect("a level-1 artifact may name the level-0 artifact of the same key");

    let served =
        levelled_artifacts_of(&engine, &full_coverage_credential(), None, "taxonomy/arxiv");
    assert_eq!(served.len(), 2, "both levels are served");
    let child = served
        .iter()
        .find(|a| !a.parent_ids.is_empty())
        .expect("the level-1 artifact names its parent");
    let parent = served
        .iter()
        .find(|a| a.tessera_id == child.parent_ids[0])
        .expect("and the parent is in the response");
    assert_ne!(
        child.tessera_id, parent.tessera_id,
        "two artifacts, one name"
    );
}

// ---------------------------------------------------------------------------------------------
// What the cut owes a dependent
// ---------------------------------------------------------------------------------------------

/// A flat layer of labels, each one hanging from an artifact of `target`.
///
/// It declares no content of its own: every withholding below has to come from the dependency,
/// or the test would pass with the drop deleted.
fn labels_on(name: &str, target: &str) -> LayerDeclaration {
    let mut d = declaration(name, None, false);
    d.hierarchy.kind = HierarchyKind::Flat;
    d.content.computed = Vec::new();
    d.depends_on = vec![target.into()];
    d
}

/// One label, over the same documents as the artifact it describes.
fn label(
    fx: &Fixture,
    key: &str,
    target_layer: &str,
    target_key: &str,
    sources: impl Iterator<Item = u64>,
) -> IncomingArtifact {
    IncomingArtifact::attached(
        Some(key.into()),
        fx.members(sources),
        Vec::new(),
        IncomingAttachment {
            layer: target_layer.into(),
            level: 0,
            key: target_key.into(),
        },
    )
}

fn keys_in(served: &[ArtifactOut], layer: &str) -> Vec<String> {
    let mut names: Vec<String> = served
        .iter()
        .filter(|a| a.layer == layer)
        .filter_map(|a| a.key.clone())
        .collect();
    names.sort();
    names
}

/// The tree of `a_budget_climbs_the_tree_rather_than_sampling_it`, with one label hanging from
/// each of three nodes at three different depths — so that whatever the budget resolves to, some
/// label's subject is in the response and the other two labels' subjects are not.
fn a_tree_and_labels_at_three_depths(engine: &Engine, fx: &Fixture) {
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .register_layer(labels_on("topics/x", "clusters/tree"))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
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
    engine
        .publish_artifacts(
            "topics/x".into(),
            0,
            vec![
                label(fx, "on-root", "clusters/tree", "root", 0..400),
                label(fx, "on-a", "clusters/tree", "a", 0..200),
                label(fx, "on-a1", "clusters/tree", "a1", 0..100),
            ],
        )
        .unwrap();
}

/// **The headline: one response never describes a cluster it does not contain.**
///
/// Every one of these three labels passes its own test at every budget — the cut runs *after* the
/// verdicts, so it serves fewer artifacts and never evaluates fewer, and the dependency
/// prerequisite of [decision 0089](../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)
/// was satisfied by a cluster the same response then removed. Without the drop, a client asking
/// for a budget of one is answered with a root and three labels, two of them naming clusters that
/// are not there and cannot be asked for.
///
/// The budgets are the same three the cut's own test uses, so the cluster half of each assertion
/// is that test's expected frontier, unchanged.
#[test]
fn a_label_goes_when_the_cut_removes_what_it_describes() {
    let fx = fixture();
    let engine = fx.open();
    a_tree_and_labels_at_three_depths(&engine, &fx);
    let credential = full_coverage_credential();

    for (budget, clusters, labels) in [
        (None, vec!["a1", "a2", "b1", "b2"], vec!["on-a1"]),
        (Some(3), vec!["a", "b"], vec!["on-a"]),
        (Some(1), vec!["root"], vec!["on-root"]),
    ] {
        let served = artifacts_of(&engine, &credential, budget);
        assert_eq!(
            keys_in(&served, "clusters/tree"),
            clusters,
            "the frontier at {budget:?}"
        );
        assert_eq!(
            keys_in(&served, "topics/x"),
            labels,
            "only the label whose subject this cut serves is drawn, at {budget:?}"
        );
    }
}

/// **A request naming the label layer alone is answered exactly as it was before the drop
/// existed.** The response looked at no cluster, so it removed none, and a lookup that could not
/// tell those two apart would blank every label a client asked for on its own.
///
/// This is a legitimate call and refusing it is outside the disclosure surface. Nothing is
/// disclosed by answering it: each label was gated on its own target's `verdict`, which is the
/// whole of what 0089 requires and is unchanged by which layers a request happens to name.
#[test]
fn a_request_for_the_labels_alone_keeps_every_label_it_would_have_had() {
    let fx = fixture();
    let engine = fx.open();
    a_tree_and_labels_at_three_depths(&engine, &fx);

    let served = levelled_artifacts_of(&engine, &full_coverage_credential(), Some(1), "topics/x");
    assert_eq!(
        keys_in(&served, "topics/x"),
        vec!["on-a", "on-a1", "on-root"],
        "a budget of one takes nothing from a flat layer, and no cluster was cut from a response \
         that walked no clusters"
    );
}

/// **A chain cascades.** A note on a label goes when the label goes, which goes when the cluster
/// it describes is cut — and the note never named the label on the wire, so it cannot be asked to
/// filter for itself.
#[test]
fn a_dependent_of_a_dropped_dependent_goes_with_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(treed("clusters/tree", None)).unwrap();
    engine
        .register_layer(labels_on("topics/x", "clusters/tree"))
        .unwrap();
    engine
        .register_layer(labels_on("notes/y", "topics/x"))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "leaf", Some("root"), 0..100),
                node(&fx, "other", Some("root"), 100..200),
            ],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "topics/x".into(),
            0,
            vec![label(&fx, "on-leaf", "clusters/tree", "leaf", 0..100)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "notes/y".into(),
            0,
            vec![label(&fx, "note", "topics/x", "on-leaf", 0..100)],
        )
        .unwrap();

    let credential = full_coverage_credential();
    let whole = artifacts_of(&engine, &credential, None);
    assert_eq!(keys(&whole), vec!["leaf", "note", "on-leaf", "other"]);

    // Two leaves do not fit in one, so the cut climbs to the root — which the label does not
    // describe.
    let cut = artifacts_of(&engine, &credential, Some(1));
    assert_eq!(
        keys(&cut),
        vec!["root"],
        "the label goes with its cluster and the note goes with the label"
    );
}

// ---------------------------------------------------------------------------------------------
// The level on the wire, and the level as a request bound (2026-08-28).
//
// Two things were missing at once and each hid the other. A response said nothing about which
// level an artifact sat at, so a client counted `parent_id` links — which answers a different
// question, and disagrees wherever a layer's edges skip a level or its roots have no parent. And a
// request could not name a level, so every level was served on every request and a client following
// the published zoom→level map paid for five and drew one.
// ---------------------------------------------------------------------------------------------

/// A tiered layer whose levels declare zoom ranges, as a real geography does — GeoNames' own
/// country/admin1/admin2 ladder is `[0,4] [3,7] [6,10]`, overlapping at the seams so a scale change
/// is a fade rather than a jump.
fn tiered_zoomed(name: &str, ranges: &[(u32, u32)]) -> LayerDeclaration {
    let mut d = declaration(name, None, false);
    d.hierarchy.kind = HierarchyKind::Tiered;
    d.levels = ranges
        .iter()
        .enumerate()
        .map(
            |(level, &(lo, hi))| tessera_types::layer::LevelDeclaration {
                level: level as u32,
                title: Some(format!("level {level}")),
                zoom: Some((lo, hi)),
            },
        )
        .collect();
    d
}

/// Three levels of one tiered layer, planted so each level's membership is disjoint from its
/// siblings' and every artifact clears any criterion.
fn plant_three_levels(engine: &Engine, fx: &Fixture, layer: &str) {
    engine
        .publish_artifacts(layer.into(), 0, vec![node(fx, "country", None, 0..300)])
        .unwrap();
    engine
        .publish_artifacts(
            layer.into(),
            1,
            vec![
                node(fx, "state-a", Some("country"), 0..150),
                node(fx, "state-b", Some("country"), 150..300),
            ],
        )
        .unwrap();
    engine
        .publish_artifacts(
            layer.into(),
            2,
            vec![
                node(fx, "county-a", Some("state-a"), 0..75),
                node(fx, "county-b", Some("state-b"), 150..225),
            ],
        )
        .unwrap();
}

fn at_zoom(
    engine: &Engine,
    credential: &[u8],
    zoom: u8,
    layer: &str,
    levels: LevelSelection<'_>,
) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", zoom, WHOLE_MAP, N_ITEMS as usize)
                .layers(LayerSelection::Named(&[layer]))
                .levels(levels),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

fn levels_of(served: &[ArtifactOut]) -> Vec<u32> {
    // `rung` is the declared level on these layers, every one of them levelled.
    let mut out: Vec<u32> = served.iter().map(|a| a.rung).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// **A request naming no levels is answered at the levels the layer declares for that depth.**
///
/// This is the whole of the change on the serving side. The declaration carries a zoom range per
/// level and `/v1/meta` publishes it; the request carries the depth it is asking at; until now
/// nothing joined them, so an overview over a five-level administrative hierarchy was served all
/// five and the client drew one. The ranges overlap at their seams, so a depth inside two of them
/// is answered at both — a scale change is a fade, and dropping one of the pair to make the answer
/// tidy would blank a level mid-transition.
#[test]
fn a_request_naming_no_levels_follows_the_declared_map() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered_zoomed(
            "admin/boundaries",
            &[(0, 4), (3, 7), (6, 10)],
        ))
        .unwrap();
    plant_three_levels(&engine, &fx, "admin/boundaries");

    // Depth 0: only the coarsest range contains it.
    let overview = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::Declared,
    );
    assert_eq!(
        levels_of(&overview),
        vec![0],
        "depth 0 is inside [0,4] alone"
    );

    // Depth 3: the seam of the first two ranges, so both answer.
    let seam = at_zoom(
        &engine,
        &full_coverage_credential(),
        3,
        "admin/boundaries",
        LevelSelection::Declared,
    );
    assert_eq!(
        levels_of(&seam),
        vec![0, 1],
        "depth 3 is inside [0,4] and [3,7]"
    );

    // Depth 8: past the first two entirely.
    let deep = at_zoom(
        &engine,
        &full_coverage_credential(),
        8,
        "admin/boundaries",
        LevelSelection::Declared,
    );
    assert_eq!(levels_of(&deep), vec![2], "depth 8 is inside [6,10] alone");
}

/// **Naming levels serves exactly those, and `All` serves every one** — the override that keeps
/// the zoom map advisory in the sense that matters: a client may always ask for what the map does
/// not offer it, and pay for it.
#[test]
fn naming_levels_overrides_the_declared_map() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered_zoomed(
            "admin/boundaries",
            &[(0, 4), (3, 7), (6, 10)],
        ))
        .unwrap();
    plant_three_levels(&engine, &fx, "admin/boundaries");

    // At depth 0 the map offers level 0 alone; asking for 2 gets 2 and nothing else.
    let named = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::Named(&[2]),
    );
    assert_eq!(levels_of(&named), vec![2]);

    let both = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::Named(&[0, 2]),
    );
    assert_eq!(levels_of(&both), vec![0, 2]);

    let all = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::All,
    );
    assert_eq!(
        levels_of(&all),
        vec![0, 1, 2],
        "`All` ignores the map entirely"
    );

    // A level the layer does not hold is absent rather than a refusal — the route an unreachable
    // layer name takes, and for the same reason: asking is not a way to learn what exists.
    let beyond = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::Named(&[0, 9]),
    );
    assert_eq!(levels_of(&beyond), vec![0]);
}

/// **A layer that declares no zoom range is unaffected**, in all three selections' absent case.
/// This is what keeps the default inert on every layer built before the ranges existed and on
/// every treed layer, which declares no levels at all (decision 0082).
#[test]
fn a_layer_declaring_no_zoom_range_serves_every_level() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered("admin/boundaries", 3))
        .unwrap();
    plant_three_levels(&engine, &fx, "admin/boundaries");

    // Depth 8 rather than deeper: a whole-map request at depth 9 or below is already refused on
    // `max_tiles_per_request`, which is worth knowing — the tile cap bounds the widest, deepest
    // request before any of this runs.
    for zoom in [0u8, 5, 8] {
        let served = at_zoom(
            &engine,
            &full_coverage_credential(),
            zoom,
            "admin/boundaries",
            LevelSelection::Declared,
        );
        assert_eq!(
            levels_of(&served),
            vec![0, 1, 2],
            "no range is declared, so there is no map to follow at depth {zoom}"
        );
    }
}

/// **A treed layer's rungs are its response-local chain depths, and the `levels` selection stays
/// inert on it.** A treed layer declares no levels, so a level number names nothing about it and
/// naming one must not blank its clusterings; what its `rung` column carries is the number
/// walking the response's own `parent_id` links yields — root 0, child 1 — computed server-side
/// (contracts §3.2 r43; until then the column was the constant stored level, 0). Walking parents
/// was always the correct reading of a treed layer, whose lineage is its structure; the column
/// now does the walk so no client picks the wrong derivation.
#[test]
fn a_treed_layers_rungs_are_chain_depths_and_the_levels_selection_is_inert() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(treed_whole("clusters/hdbscan", None))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/hdbscan".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "child", Some("root"), 0..150),
            ],
        )
        .unwrap();

    let served = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "clusters/hdbscan",
        LevelSelection::Declared,
    );
    assert_eq!(
        levels_of(&served),
        vec![0, 1],
        "root at rung 0, its child at 1"
    );
    assert!(served.len() >= 2, "the whole visible tree, unpruned");

    // Naming levels for the tiered layer beside it must not blank a clustering: the layer
    // declares none, so the selection is inert in every form and the same tree comes back.
    let named = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "clusters/hdbscan",
        LevelSelection::Named(&[1]),
    );
    assert_eq!(keys(&named), keys(&served));
}

/// **The level is the declaration's, not the depth of the chain that reached it.**
///
/// The bug this closes, in the shape it was found in: `clusters/toponymy` put 490 of 797 artifacts
/// at the wrong level because the client counted parent links, and a tiered layer's edges may skip
/// a level. Here the county's parent is the **country**, two levels up — a city directly under a
/// country because that country has no states, which is a fact about the data and not a gap in the
/// ladder. Counting links puts it at depth 1; it is declared at level 2, and the column says so.
#[test]
fn the_level_is_declared_not_counted_from_parent_links() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered_zoomed(
            "admin/boundaries",
            &[(0, 4), (3, 7), (6, 10)],
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![node(&fx, "state", Some("country"), 0..150)],
        )
        .unwrap();
    // The edge skips level 1 entirely: this county's parent is the country.
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            2,
            vec![node(&fx, "county", Some("country"), 200..300)],
        )
        .unwrap();

    let served = at_zoom(
        &engine,
        &full_coverage_credential(),
        0,
        "admin/boundaries",
        LevelSelection::All,
    );
    let county = served
        .iter()
        .find(|a| a.key.as_deref() == Some("county"))
        .expect("the county is served");
    let country = served
        .iter()
        .find(|a| a.key.as_deref() == Some("country"))
        .expect("the country is served");

    assert_eq!(
        county.rung, 2,
        "the declared level — a levelled layer's rung"
    );
    assert_eq!(county.parent_ids, vec![country.tessera_id],
        "and its parent is the country, one link up — which is the count that would say 1"
    );
}

/// **A zoom range that contains no depth is refused at the declaration.**
///
/// It was harmless while the range was advisory. Now it decides what a request naming no `levels`
/// is answered at, so an inverted or out-of-grid range means the level is served at no depth at all
/// — and the operator's only symptom would be a layer silently absent from every zoom.
#[test]
fn a_zoom_range_containing_no_depth_is_refused() {
    let fx = fixture();
    let engine = fx.open();

    let mut inverted = tiered_zoomed("admin/a", &[(0, 4), (7, 3)]);
    inverted.name = "admin/inverted".into();
    assert!(
        engine.register_layer(inverted).is_err(),
        "[7, 3] contains no depth"
    );

    let mut past_grid = tiered_zoomed("admin/b", &[(0, 4), (17, 20)]);
    past_grid.name = "admin/past-grid".into();
    assert!(
        engine.register_layer(past_grid).is_err(),
        "a range starting past the grid's depth of 16 contains no depth a request can ask at"
    );

    // **A single-depth range is legal** — `[4, 4]` is one band, not an empty one — and so is an
    // absent range, which means *served at every depth*.
    let mut single = tiered_zoomed("admin/c", &[(0, 0), (4, 4)]);
    single.name = "admin/single".into();
    assert!(engine.register_layer(single).is_ok());
    assert!(engine.register_layer(tiered("admin/none", 2)).is_ok());
}

/// **A level selection is inert on a layer that declares no levels.**
///
/// The uniform reading — the selection applies to every layer named — would otherwise let a client
/// asking for level 1 of its boundaries blank every clustering in the same response, a treed layer
/// sitting entirely at level 0 (decision 0082). A level number names nothing about such a layer, so
/// it does not select against it.
#[test]
fn naming_a_level_does_not_blank_a_treed_layer_beside_it() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered_zoomed("admin/boundaries", &[(0, 4), (3, 7)]))
        .unwrap();
    engine
        .register_layer(treed_whole("clusters/hdbscan", None))
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![node(&fx, "state", Some("country"), 0..150)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/hdbscan".into(),
            0,
            vec![node(&fx, "cluster", None, 0..300)],
        )
        .unwrap();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let served = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                .layers(LayerSelection::Named(&[
                    "admin/boundaries",
                    "clusters/hdbscan",
                ]))
                .levels(LevelSelection::Named(&[1])),
        )
        .expect("a viewport over the whole map")
        .artifacts;

    assert_eq!(
        keys_in(&served, "admin/boundaries"),
        vec!["state"],
        "the levelled layer answers at the level asked for"
    );
    assert_eq!(
        keys_in(&served, "clusters/hdbscan"),
        vec!["cluster"],
        "and the treed layer beside it is untouched, having no level the number could name"
    );
}

/// **A level a request did not ask for takes its dependents with it.**
///
/// The trap this closes: the names of a boundary set often live in a dependent layer, so a level
/// selection that dropped the boundaries and kept their labels would leave a map annotated with
/// names for regions it is not drawing. The rule already exists — a dependent whose target is
/// withheld is absent entire — and this is what proves a level filter is not a hole in it.
///
/// **The label layer declares its own levels, and that is what makes the test bite.** A flat label
/// layer sits at level 0, so asking for level 1 alone excludes the *labels* too and the orphan path
/// is never reached — the test then passes with the whole drop deleted, which is what an earlier
/// revision of it did. Here the label is at level 1 and its target at level 0, so a request for
/// level 1 selects the label and not its subject: the only thing that can remove it is the orphan
/// rule.
#[test]
fn a_dependent_goes_when_its_targets_level_is_not_asked_for() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(tiered_zoomed("admin/boundaries", &[(0, 4), (3, 7)]))
        .unwrap();
    let mut names = labels_on("admin/names", "admin/boundaries");
    names.hierarchy.kind = HierarchyKind::Stacked;
    names.levels = (0..2)
        .map(|level| tessera_types::layer::LevelDeclaration {
            level,
            title: Some(format!("names {level}")),
            zoom: None,
        })
        .collect();
    engine.register_layer(names).unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            0,
            vec![node(&fx, "country", None, 0..300)],
        )
        .unwrap();
    engine
        .publish_artifacts(
            "admin/boundaries".into(),
            1,
            vec![node(&fx, "state", Some("country"), 0..150)],
        )
        .unwrap();
    // The label sits at level **1** and describes the country, which is at level 0.
    engine
        .publish_artifacts(
            "admin/names".into(),
            1,
            vec![label(
                &fx,
                "country-name",
                "admin/boundaries",
                "country",
                0..300,
            )],
        )
        .unwrap();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let both = |zoom: u8, levels: LevelSelection<'_>| -> Vec<ArtifactOut> {
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", zoom, WHOLE_MAP, N_ITEMS as usize)
                    .layers(LayerSelection::Named(&["admin/boundaries", "admin/names"]))
                    .levels(levels),
            )
            .expect("a viewport over the whole map")
            .artifacts
    };

    // Both levels asked for: the country is served and so is the name at level 1 above it.
    let served = both(0, LevelSelection::Named(&[0, 1]));
    assert_eq!(
        keys_in(&served, "admin/boundaries"),
        vec!["country", "state"]
    );
    assert_eq!(keys_in(&served, "admin/names"), vec!["country-name"]);

    // **Level 1 alone.** The label's own level *is* selected — so nothing about the selection
    // removes it — and its subject's is not. It must go with its subject rather than float free
    // over a boundary the response does not carry.
    let served = both(0, LevelSelection::Named(&[1]));
    assert_eq!(keys_in(&served, "admin/boundaries"), vec!["state"]);
    assert!(
        keys_in(&served, "admin/names").is_empty(),
        "a label whose subject was not served is absent, whatever withheld the subject"
    );
}

// ---------------------------------------------------------------------------------------------
// The dag shape: a child under several parents (`dag-hierarchies.md`, decision 0117)
// ---------------------------------------------------------------------------------------------

/// A `dag` layer: `treed_whole` at the kind that records a second parent, named on the artifact
/// row, rather than refusing it. Open, so an ingest batch's keys mint.
fn dag(name: &str) -> LayerDeclaration {
    let mut d = declaration(name, None, false);
    d.hierarchy.kind = HierarchyKind::Dag;
    d.value_set = tessera_types::layer::ValueSet::Open;
    d
}

/// The same at `nested`, for the refusals that must not move.
fn tree_open(name: &str) -> LayerDeclaration {
    let mut d = declaration(name, None, false);
    d.value_set = tessera_types::layer::ValueSet::Open;
    d
}

/// One node of a planted graph: a key, its parents' keys, and the source ids it holds.
fn node_under(
    fx: &Fixture,
    key: &str,
    parents: &[&str],
    sources: impl Iterator<Item = u64>,
) -> IncomingArtifact {
    let mut artifact = IncomingArtifact::from_entities(Some(key.into()), fx.members(sources));
    artifact.parent_keys = parents.iter().map(|p| p.to_string()).collect();
    artifact
}

/// The parents the **bundle** holds for the artifact under `key`, read out of the live prefix's
/// record packs by the same decoder the engine opens them with — the durable form
/// (`dag-hierarchies.md` §7), not the served one.
fn parents_in_bundle(
    fx: &Fixture,
    engine: &Engine,
    key: &str,
) -> Vec<tessera_lifecycle::wal::ParentRef> {
    let dir = fx
        .root
        .join(&engine.generation().prefix)
        .join("partitions")
        .join("default")
        .join("members");
    let mut found = None;
    for entry in std::fs::read_dir(&dir)
        .expect("the fold wrote record packs")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "tsmb") {
            continue;
        }
        let pack = tessera_store::membership::MembershipPack::open(&path).expect("a pack opens");
        for (_, blob) in pack.iter() {
            if blob.is_empty() {
                continue;
            }
            let (record, _) = tessera_lifecycle::membership::decode_record(EntityId::new(1), blob)
                .expect("a record the fold wrote decodes");
            if record.key.as_deref() == Some(key) {
                assert!(found.is_none(), "one artifact under {key}");
                found = Some(record.parents);
            }
        }
    }
    found.unwrap_or_else(|| panic!("no record under {key} in the bundle"))
}

/// **A child under two parents publishes, is served, and comes back from the bundle with both**
/// (`dag-hierarchies.md` §4, §7). The record is the guard `BUNDLE_FORMAT` 5 exists for: the fold
/// writes the parent list, and a reopen with the log deleted reads it back from the bundle alone.
///
/// The response names both parents in `parent_ids`, ascending by identifier (contracts §3.2 r71),
/// since both are served.
#[test]
fn a_dag_child_under_two_parents_is_published_served_and_folded_whole() {
    let fx = fixture();
    let served_ids = {
        let engine = fx.open();
        engine.register_layer(dag("mesh/d")).unwrap();
        engine
            .publish_artifacts(
                "mesh/d".into(),
                0,
                vec![
                    node_under(&fx, "p0", &[], 0..150),
                    node_under(&fx, "p1", &[], 50..200),
                    // Named out of order and one of them twice: the record is ascending and
                    // holds each parent once.
                    node_under(&fx, "c", &["p1", "p0", "p1"], 50..150),
                ],
            )
            .expect("a child under two parents is what a dag layer declares");

        let served = artifacts_of(&engine, &full_coverage_credential(), None);
        assert_eq!(keys(&served), vec!["c", "p0", "p1"]);
        let id_of = |key: &str| {
            served
                .iter()
                .find(|a| a.key.as_deref() == Some(key))
                .unwrap()
                .tessera_id
        };
        let child = served
            .iter()
            .find(|a| a.key.as_deref() == Some("c"))
            .unwrap();
        let mut both = vec![id_of("p0"), id_of("p1")];
        both.sort_unstable();
        assert_eq!(
            child.parent_ids, both,
            "the child names both served parents, ascending by identifier"
        );

        // No point was ingested, so there is nothing to flush; the fold rewrites the level whole.
        fold(&engine);
        let parents = parents_in_bundle(&fx, &engine, "c");
        assert_eq!(
            parents,
            vec![
                tessera_lifecycle::wal::ParentRef {
                    level: 0,
                    ordinal: 0
                },
                tessera_lifecycle::wal::ParentRef {
                    level: 0,
                    ordinal: 1
                }
            ],
            "the bundle holds both parents, ascending, each once"
        );
        let mut ids: Vec<_> = served.iter().map(|a| a.tessera_id).collect();
        ids.sort();
        ids
    };
    remove_the_whole_log(&fx.wal);

    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential(), None);
    assert_eq!(keys(&served), vec!["c", "p0", "p1"]);
    let mut ids: Vec<_> = served.iter().map(|a| a.tessera_id).collect();
    ids.sort();
    assert_eq!(
        ids, served_ids,
        "the same three artifacts, from the bundle alone"
    );
    let child = served
        .iter()
        .find(|a| a.key.as_deref() == Some("c"))
        .unwrap();
    assert!(!child.parent_ids.is_empty(), "the child still names a parent");
}

/// One ingest batch of one point naming `keys` on `layer`, with the edges its list column would
/// have declared — what `/control/ingest` decodes to, taken at the engine boundary. Only a
/// `nested` or `tiered` column declares edges; a `dag` column's keys are memberships alone
/// (decision 0125), so a dag batch here carries none.
fn ingest_edges(
    engine: &Engine,
    batch: &str,
    layer: &str,
    keys: &[&str],
    edges: &[(&str, &str)],
) -> Result<u64, String> {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(batch.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest_joining(
            vec![row],
            batch.to_string(),
            hash,
            tessera_lifecycle::BatchArtifacts {
                memberships: keys
                    .iter()
                    .map(|key| tessera_lifecycle::BatchMembership {
                        layer: layer.to_string(),
                        level: 0,
                        key: key.to_string(),
                        rows: vec![0],
                    })
                    .collect(),
                edges: edges
                    .iter()
                    .map(|(child, parent)| tessera_lifecycle::BatchEdge {
                        layer: layer.to_string(),
                        level: 0,
                        child: child.to_string(),
                        parent: parent.to_string(),
                    })
                    .collect(),
            },
        )
        .map(|(_, minted)| minted)
        .map_err(|e| e.to_string())
}

/// The masked count served under `key`, for full coverage — retried past the bounded
/// `FragmentBuilding` a fresh publication answers an authorisation with.
fn count_of(engine: &Engine, key: &str) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let served = loop {
        match engine.authorise(&full_coverage_credential()).and_then(|session| {
            engine.viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
        }) {
            Ok(out) => break out.artifacts,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out retrying a viewport: {e}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    };
    served
        .iter()
        .find(|a| a.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{key} is served"))
        .masked_count
}

/// **A child published under two parents takes an ingested point into all three and folds whole**
/// (`dag-hierarchies.md` §4, decision 0125): the edges are spelled on the artifact rows at the
/// publication, both parents resolved among the batch's own siblings before the edge into them;
/// the ingest batch's dag column names the three keys as memberships and no edge, so it mints
/// nothing and grows each; and the record the fold writes carries the pair.
#[test]
fn a_dag_child_published_under_two_parents_grows_by_a_batch_naming_all_three() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(dag("mesh/d")).unwrap();
    engine
        .publish_artifacts(
            "mesh/d".into(),
            0,
            vec![
                node_under(&fx, "c", &["p0", "p1"], 0..10),
                node_under(&fx, "p0", &[], 0..20),
                node_under(&fx, "p1", &[], 0..30),
            ],
        )
        .expect("a child under two parents, named before either, is one publication");
    let before: Vec<u64> = ["c", "p0", "p1"]
        .iter()
        .map(|k| count_of(&engine, k))
        .collect();
    assert_eq!(
        ingest_edges(&engine, "b1", "mesh/d", &["c", "p0", "p1"], &[])
            .expect("three memberships on a dag layer"),
        0,
        "every key exists, so nothing mints"
    );
    // A membership is projected through base rows, so the counts are readable once the fold has
    // given the ingested point one.
    flush(&engine);
    fold(&engine);
    for (key, was) in ["c", "p0", "p1"].iter().zip(before) {
        assert_eq!(count_of(&engine, key), was + 1, "{key} grew by the point");
    }
    let parents = parents_in_bundle(&fx, &engine, "c");
    assert_eq!(parents.len(), 2, "both edges, each resolved: {parents:?}");
    for key in ["p0", "p1"] {
        assert!(
            parents_in_bundle(&fx, &engine, key).is_empty(),
            "{key} is a root"
        );
    }
}

/// **A cycle refuses at ingest, at every kind, naming the cycle** (`dag-hierarchies.md` §4). On a
/// `nested` layer the edges arrive by the list column and the check is over the window's minted
/// edges, where the artifacts are created; on a `dag` layer they arrive by the publish route,
/// whose `prepare_publish` is the same check. A self-edge is the cycle of length one. Nothing is
/// published.
#[test]
fn a_cycle_refuses_an_ingest_batch_at_every_kind() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(dag("mesh/d")).unwrap();
    engine.register_layer(tree_open("clusters/t")).unwrap();

    let refused = ingest_edges(
        &engine,
        "three-tree",
        "clusters/t",
        &["a", "b", "c"],
        &[("a", "b"), ("b", "c"), ("c", "a")],
    )
    .expect_err("a 3-cycle has no root");
    assert!(
        refused.contains("cycle — a → b → c → a"),
        "the cycle is named, child → parent: {refused}"
    );
    let refused = ingest_edges(&engine, "self-tree", "clusters/t", &["s"], &[("s", "s")])
        .expect_err("a self-edge is a cycle of length one");
    assert!(refused.contains("s → s"), "{refused}");

    let refused = engine
        .publish_artifacts(
            "mesh/d".into(),
            0,
            vec![
                node_under(&fx, "a", &["b"], 0..10),
                node_under(&fx, "b", &["c"], 0..10),
                node_under(&fx, "c", &["a"], 0..10),
            ],
        )
        .expect_err("a 3-cycle has no root");
    assert!(
        refused.to_string().contains("cycle — a → b → c → a"),
        "the cycle is named, child → parent: {refused}"
    );
    let refused = engine
        .publish_artifacts("mesh/d".into(), 0, vec![node_under(&fx, "s", &["s"], 0..10)])
        .expect_err("a self-edge is a cycle of length one");
    assert!(refused.to_string().contains("s → s"), "{refused}");
    assert_eq!(
        engine.published_artifacts(),
        0,
        "a refusal publishes nothing"
    );

    // A diamond published in one batch is not a cycle.
    engine
        .publish_artifacts(
            "mesh/d".into(),
            0,
            vec![
                node_under(&fx, "r", &[], 0..40),
                node_under(&fx, "l", &["r"], 0..20),
                node_under(&fx, "m", &["r"], 20..40),
                node_under(&fx, "c", &["l", "m"], 10..30),
            ],
        )
        .expect("two paths to one root, every edge descending");
    assert_eq!(engine.published_artifacts(), 4);
}

/// **A `nested` layer still refuses a child named under two parents, in the words it always
/// used**, and so does a `dag` layer's *list* column now that it declares no edges at all
/// (decision 0125): two edges for one child can only reach the engine from a list column, and
/// no kind's list column may name two parents. A dag child's several parents are the publish
/// route's, above.
#[test]
fn a_list_column_naming_two_parents_refuses_at_every_kind() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(dag("mesh/d")).unwrap();
    engine.register_layer(tree_open("clusters/t")).unwrap();
    for layer in ["clusters/t", "mesh/d"] {
        let refused = ingest_edges(
            &engine,
            &format!("two-{layer}"),
            layer,
            &["c", "p0", "p1"],
            &[("c", "p0"), ("c", "p1")],
        )
        .expect_err("a list column's child has one parent");
        assert!(
            refused.contains(&format!(
                "c in level 0 of {layer} is named as a child of both p0 and p1"
            )),
            "{refused}"
        );
    }
    assert_eq!(engine.published_artifacts(), 0);
}
