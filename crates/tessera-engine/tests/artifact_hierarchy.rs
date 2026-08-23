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
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
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
        layout: None,
        shape: None,
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

/// One node of a planted tree: a key, its parent's key, and the source ids it holds.
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
                .layers(Some(&[layer])),
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
/// An over-large response is the artifact ceiling's business, which refuses rather than
/// truncating. The cut must never start sampling to reach a number.
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
    assert_eq!(country.parent_id, None, "a root names no parent");
    assert_eq!(
        by_key("state-a").parent_id,
        Some(country.tessera_id),
        "a state names the country it is in, by the identifier that country was served under"
    );
    assert_eq!(by_key("state-b").parent_id, Some(country.tessera_id));
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
    assert_eq!(
        narrow[0].parent_id, None,
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
        state.parent_id.is_some(),
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
    assert_eq!(
        served[0].parent_id, None,
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
        .find(|a| a.parent_id.is_some())
        .expect("the level-1 artifact names its parent");
    let parent = served
        .iter()
        .find(|a| a.tessera_id == child.parent_id.unwrap())
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
