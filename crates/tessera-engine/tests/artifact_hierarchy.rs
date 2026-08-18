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
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerAccess, LayerDeclaration,
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
        title: format!("{name} (title)"),
        slices: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        access: LayerAccess {
            label: None,
            artifacts_carry_own: false,
        },
        visible_when: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Nested,
            prune_children,
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
    let mut names: Vec<String> = artifacts
        .iter()
        .filter_map(|a| a.stable_key.clone())
        .collect();
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
/// A parent at a small fraction of a large membership fails a `min_fraction` rule while its child
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
    let child_sources: Vec<u64> = (0..300).filter(|s| terms_of(*s).contains(&SUBSET_TERM)).take(20).collect();

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
            Some(ExistenceCriterion::MinFraction(bar)),
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
            Some(ExistenceCriterion::MinFraction(bar)),
        ))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/alone".into(),
            0,
            vec![node(&fx, "parent-alone", None, parent_sources.iter().copied())],
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
                let Some(b) = deep
                    .iter()
                    .find(|other| other.tessera_id == a.tessera_id)
                else {
                    continue;
                };
                assert_eq!(
                    a.masked_count, b.masked_count,
                    "an artifact two cuts both return says the same thing in both"
                );
                assert_eq!(a.stable_key, b.stable_key);
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
            .find(|a| a.stable_key.as_deref() == Some(key))
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
