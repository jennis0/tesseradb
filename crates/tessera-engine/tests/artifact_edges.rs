//! **A label does not outlive what it labels — on every route, including the ones that traverse
//! nothing.**
//!
//! The model's conjunctive rule covers edge *traversal*. Search, a held identifier and a filter
//! reach a label **directly**, so without the extra term the predicate carries, suppressing a
//! cluster would hide the cluster while every label naming and describing it went on serving — and
//! those labels are exactly the description of the thing that was just hidden
//! (`annotation-representation.md` §4). Every test here therefore asserts the *two* serving routes
//! together: a viewport that drops the label while drill-down still answers on the identifier it
//! issued a moment ago is the whole failure.

mod common;

use common::*;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::membership::{IncomingAttachment, IncomingContent};
use tessera_lifecycle::{wal::ChangeOp, IncomingArtifact};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent,
};
use tessera_types::{EntityId, TesseraId};

const CLUSTERS: &str = "clusters/a";
const LABELS: &str = "topics/x";

/// The cluster layer. `visibility` is the access label a viewer must hold to reach it at all.
fn clusters(visibility: Option<&str>) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: CLUSTERS.into(),
        title: Some("clusters".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: visibility.map(str::to_string),
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// The label layer: `public`, corpus-derived text, and declaring the cluster layer it edges into.
///
/// **`public` on purpose.** Every withholding below has to come from the attachment term rather than
/// from the label layer's own gate, or the test would pass with the term deleted.
fn labels() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LABELS.into(),
        title: Some("topics".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "topic".into(),
                ty: "text".into(),
                require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
            }],
        },
        depends_on: vec![CLUSTERS.into()],
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

/// One cluster over `0..150`, and one label attached to it, generated from the same documents.
fn publish_a_cluster_and_its_label(engine: &Engine, fx: &Fixture) {
    engine
        .publish_artifacts(
            CLUSTERS.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..150),
            )],
        )
        .unwrap();
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::attached(
                Some("l0".into()),
                fx.members(0..150),
                // Two ranked contents, as a real label layer publishes them: one generated from
                // the whole cluster, one from the third of it a narrower principal can see. Which
                // one a viewer gets is containment's business and not this file's — what matters
                // here is that a principal who is served *some* content is still refused the
                // whole artifact once its cluster goes.
                vec![
                    IncomingContent::new(vec!["shipping and logistics".into()], fx.members(0..150)),
                    IncomingContent::new(
                        vec!["logistics".into()],
                        fx.members((0..150).filter(|s| terms_of(*s).contains(&SUBSET_TERM))),
                    ),
                ],
                IncomingAttachment {
                    layer: CLUSTERS.into(),
                    level: 0,
                    key: "c0".into(),
                },
            )],
        )
        .unwrap();
}

fn labels_in(served: &[ArtifactOut]) -> Vec<&ArtifactOut> {
    served.iter().filter(|a| a.layer == LABELS).collect()
}

/// Whether the identifier route still answers — the route that traverses no edge, and the one the
/// extra predicate term exists for.
fn reachable_by_identifier(engine: &Engine, credential: &[u8], id: TesseraId) -> bool {
    let session = engine.authorise(credential).unwrap();
    engine.artifact(&session, id, None, "s0", None).unwrap().is_some()
}

/// **The headline.** Suppress the cluster and its label stops serving — in the viewport *and* on the
/// identifier a viewer is already holding. Without the attachment term the second half goes on
/// answering, with the label's text, describing the cluster that was just hidden.
#[test]
fn suppressing_a_cluster_stops_its_labels_serving_on_a_held_identifier_too() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(clusters(None)).unwrap();
    engine.register_layer(labels()).unwrap();
    publish_a_cluster_and_its_label(&engine, &fx);

    let served = artifacts_of(&engine, &full_coverage_credential());
    let label = labels_in(&served);
    assert_eq!(label.len(), 1, "the label serves while its cluster does");
    assert_eq!(label[0].content, vec!["shipping and logistics".to_string()]);
    let label_id = label[0].tessera_id;
    let cluster_id = served
        .iter()
        .find(|a| a.layer == CLUSTERS)
        .expect("the cluster serves too")
        .tessera_id;
    // The identifier is taken *before* the suppression, which is the case that matters: a viewer
    // who was shown the label a moment ago is exactly who would go on reading it.
    assert!(reachable_by_identifier(
        &engine,
        &full_coverage_credential(),
        label_id
    ));

    engine
        .accept_change(artifact_entity(&engine, cluster_id), ChangeOp::Suppress)
        .unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert!(
        after.iter().all(|a| a.layer != CLUSTERS),
        "the cluster itself is suppressed at the ack"
    );
    assert!(
        labels_in(&after).is_empty(),
        "the label goes with it — a description of a hidden cluster is the disclosure"
    );
    assert!(
        !reachable_by_identifier(&engine, &full_coverage_credential(), label_id),
        "and the route that traverses no edge agrees, which is the whole of the extra term"
    );

    // The label's own entity was never touched, so lifting the cluster's suppression restores both.
    engine
        .accept_change(artifact_entity(&engine, cluster_id), ChangeOp::Unsuppress)
        .unwrap();
    assert_eq!(
        labels_in(&artifacts_of(&engine, &full_coverage_credential())).len(),
        1
    );
    assert!(reachable_by_identifier(
        &engine,
        &full_coverage_credential(),
        label_id
    ));
}

/// **The gate half, which is not optional.** A viewer who cannot reach the cluster layer is served
/// no labels out of a layer they *can* reach — otherwise the label layer would report what the
/// clusters of a gated layer are called.
#[test]
fn a_viewer_who_cannot_reach_the_cluster_layer_is_served_none_of_its_labels() {
    let fx = fixture();
    let engine = fx.open();
    // The cluster layer is gated on the subset term; the label layer is not gated at all.
    engine.register_layer(clusters(Some("1"))).unwrap();
    engine.register_layer(labels()).unwrap();
    publish_a_cluster_and_its_label(&engine, &fx);

    // The gated principal reaches both, and holds an identifier for the label.
    let gated = artifacts_of(&engine, &subset_credential());
    let label = labels_in(&gated);
    assert_eq!(label.len(), 1);
    assert_eq!(label[0].content, vec!["logistics".to_string()]);
    let label_id = label[0].tessera_id;

    // The principal holding `public` — term 0, which every document carries, so nothing here turns on the
    // mask — reaches the label layer and none of its labels.
    let outsider = artifacts_of(&engine, &full_coverage_credential());
    assert!(
        outsider.iter().all(|a| a.layer != CLUSTERS),
        "the cluster layer's gate is what this test rests on"
    );
    assert!(
        labels_in(&outsider).is_empty(),
        "a label must not outlive the reachability of what it labels"
    );
    assert!(
        !reachable_by_identifier(&engine, &full_coverage_credential(), label_id),
        "and an identifier handed to them by anyone else is the same answer"
    );
}

/// Suppressing the cluster **layer** takes the labels attached into it with it — the live half of
/// the gate, which a session's cached reachability must never outlive.
#[test]
fn suppressing_the_cluster_layer_stops_its_labels_serving() {
    let fx = fixture();
    let engine = fx.open();
    // Registration answers with the layer's own identifier — where a layer suppression lands.
    let cluster_layer = engine.register_layer(clusters(None)).unwrap();
    engine.register_layer(labels()).unwrap();
    publish_a_cluster_and_its_label(&engine, &fx);

    let served = artifacts_of(&engine, &full_coverage_credential());
    let label_id = labels_in(&served)[0].tessera_id;

    engine
        .accept_change(artifact_entity(&engine, cluster_layer), ChangeOp::Suppress)
        .unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert!(labels_in(&after).is_empty());
    assert!(!reachable_by_identifier(
        &engine,
        &full_coverage_credential(),
        label_id
    ));
}

/// **An attachment that did not survive a restart is a label serving over a suppressed cluster.**
/// The edge is a visibility term, so losing it on the way back is not a missing navigation aid — it
/// is the fail-open, reappearing at the one moment nothing is watching.
#[test]
fn an_attachment_survives_a_restart_and_still_withholds() {
    let fx = fixture();
    let cluster_id = {
        let engine = fx.open();
        engine.register_layer(clusters(None)).unwrap();
        engine.register_layer(labels()).unwrap();
        publish_a_cluster_and_its_label(&engine, &fx);
        let served = artifacts_of(&engine, &full_coverage_credential());
        served
            .iter()
            .find(|a| a.layer == CLUSTERS)
            .expect("the cluster serves")
            .tessera_id
    };

    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(labels_in(&served).len(), 1, "the label comes back");

    engine
        .accept_change(artifact_entity(&engine, cluster_id), ChangeOp::Suppress)
        .unwrap();
    let after = artifacts_of(&engine, &full_coverage_credential());
    assert!(
        labels_in(&after).is_empty(),
        "and it is still withheld with its cluster — the edge came back with it"
    );
}

/// An edge is refused unless its target already exists, and unless the layer declared that it edges
/// there at all — the ordering §5.0.4 turns on, and the declaration that makes a dangling
/// replacement refusable.
#[test]
fn an_edge_needs_a_target_that_exists_and_a_dependency_that_was_declared() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(clusters(None)).unwrap();
    engine.register_layer(labels()).unwrap();

    let label = |target: &str| {
        vec![IncomingArtifact::attached(
            Some("l0".into()),
            fx.members(0..150),
            vec![IncomingContent::new(
                vec!["shipping and logistics".into()],
                fx.members(0..150),
            )],
            IncomingAttachment {
                layer: CLUSTERS.into(),
                level: 0,
                key: target.into(),
            },
        )]
    };

    // No cluster yet: refused, rather than stored to name whatever later lands at that ordinal.
    let err = engine
        .publish_artifacts(LABELS.into(), 0, label("c0"))
        .expect_err("a target exists before the edge into it");
    assert!(format!("{err}").contains("holds no such artifact"), "{err}");

    engine
        .publish_artifacts(
            CLUSTERS.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..150),
            )],
        )
        .unwrap();
    engine
        .publish_artifacts(LABELS.into(), 0, label("c0"))
        .expect("and it is accepted once the cluster is there");
}

// ---- the dependency prerequisite ---------------------------------------------------------------
//
// A dependency edge carries visibility as well as ordering
// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)):
// a label is served only where the cluster it attaches to is served, **per artifact**. The cases
// above are the ones a disposition could answer — a suppression, a layer gate, a fold. These are
// the one it cannot: a cluster that is alive, reachable and simply not shown to *this* principal,
// because its own masked count is below its own layer's bar.

/// A cluster layer that announces a grouping only to a principal who can see `n` of its members.
fn clusters_with_bar(n: u64) -> LayerDeclaration {
    let mut d = clusters(None);
    d.require_member_visibility = Some(tessera_types::layer::ExistenceCriterion::Count(n));
    d
}

/// The label layer with no bar of its own, and content every principal here contains — so the only
/// thing that can withhold a label below is the prerequisite.
fn labels_containing_nothing() -> LayerDeclaration {
    let mut d = labels();
    d.content.supplied[0].require_member_visibility =
        tessera_types::layer::SuppliedRequirement::Inherited;
    d
}

/// Publish one cluster over `sources` and one label attached to it, both keyed on `key`.
fn cluster_and_label(engine: &Engine, fx: &Fixture, key: &str, sources: std::ops::Range<u64>) {
    engine
        .publish_artifacts(
            CLUSTERS.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some(key.into()),
                fx.members(sources.clone()),
            )],
        )
        .unwrap();
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::attached(
                Some(format!("l-{key}")),
                fx.members(sources),
                vec![IncomingContent::new(
                    vec![format!("the {key} topic")],
                    Vec::new(),
                )],
                IncomingAttachment {
                    layer: CLUSTERS.into(),
                    level: 0,
                    key: key.into(),
                },
            )],
        )
        .unwrap();
}

/// **The case a disposition cannot answer.** The cluster is alive, unsuppressed and in a layer
/// everyone reaches; it is simply not announced to a principal who can see too few of its members.
/// Its label must not announce it instead — and it must be *absent*, contributing to no count in
/// the response, rather than served empty.
#[test]
fn a_label_is_absent_where_its_cluster_is_below_its_own_bar_for_this_principal() {
    let fx = fixture();
    let engine = fx.open();
    // 150 documents, of which the subset principal sees every third: 50. The bar sits between.
    engine.register_layer(clusters_with_bar(100)).unwrap();
    engine.register_layer(labels_containing_nothing()).unwrap();
    cluster_and_label(&engine, &fx, "c0", 0..150);

    let broad = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(
        labels_in(&broad).len(),
        1,
        "the label serves where its cluster does"
    );
    let label_id = labels_in(&broad)[0].tessera_id;
    assert_eq!(
        broad.len(),
        2,
        "the cluster and its label, and nothing else"
    );

    let narrow = artifacts_of(&engine, &subset_credential());
    assert!(
        narrow.iter().all(|a| a.layer != CLUSTERS),
        "the cluster is below its bar for this principal — the premise of the test"
    );
    assert!(
        labels_in(&narrow).is_empty(),
        "and its label goes with it: a label is served only where its cluster is"
    );
    assert!(
        narrow.is_empty(),
        "absent, not empty — it contributes to no count this principal is shown"
    );
    assert!(
        !reachable_by_identifier(&engine, &subset_credential(), label_id),
        "including on the route that traverses no edge"
    );
}

/// **Per artifact, not per layer.** Two clusters under one declaration and two labels under
/// another: one cluster clears its bar for this principal and one does not, and exactly the label
/// of the first is served. A prerequisite asked at the layer grain — *does this principal see
/// anything in the parent layer?* — passes both.
#[test]
fn the_prerequisite_is_per_artifact_and_not_per_layer() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(clusters_with_bar(20)).unwrap();
    engine.register_layer(labels_containing_nothing()).unwrap();
    // `visible` holds 90 documents the subset principal can see a third of — 30, over the bar.
    // `hidden` holds 30, of which they see 10, under it.
    cluster_and_label(&engine, &fx, "visible", 0..90);
    cluster_and_label(&engine, &fx, "hidden", 90..120);

    let broad = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(
        labels_in(&broad).len(),
        2,
        "both labels serve to a principal who sees both clusters"
    );

    let narrow = artifacts_of(&engine, &subset_credential());
    let served: Vec<&str> = labels_in(&narrow)
        .iter()
        .map(|a| a.content[0].as_str())
        .collect();
    assert_eq!(
        served,
        vec!["the visible topic"],
        "one label, and it is the one whose own cluster this principal is shown"
    );
}

/// A layer that declares a dependency publishes dependents, and the control plane refuses anything
/// else — the ingest half of the refusal the build makes over a file. An artifact with no
/// attachment has nothing for the prerequisite to gate on, so admitting it would make the
/// prerequisite optional for whoever forgot the column.
#[test]
fn a_layer_that_declares_a_dependency_refuses_an_artifact_that_declares_none() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(clusters(None)).unwrap();
    engine.register_layer(labels()).unwrap();
    engine
        .publish_artifacts(
            CLUSTERS.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..150),
            )],
        )
        .unwrap();

    let err = engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("l0".into()),
                fx.members(0..150),
                vec![IncomingContent::new(
                    vec!["shipping and logistics".into()],
                    fx.members(0..150),
                )],
            )],
        )
        .expect_err("a label layer's artifacts attach to something");
    assert!(format!("{err}").contains("attaches to nothing"), "{err}");
}
