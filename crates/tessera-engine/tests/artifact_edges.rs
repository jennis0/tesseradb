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
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
use tessera_lifecycle::membership::{IncomingAttachment, IncomingContent};
use tessera_lifecycle::{wal::ChangeOp, IncomingArtifact};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent,
};
use tessera_types::{EntityId, TesseraId};

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const CLUSTERS: &str = "clusters/a";
const LABELS: &str = "topics/x";

/// The cluster layer. `gate` is the access label a viewer must hold to reach it at all.
fn clusters(gate: Option<&str>) -> LayerDeclaration {
    LayerDeclaration {
        name: CLUSTERS.into(),
        title: Some("clusters".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        visibility: gate.map(str::to_string),
            artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
    }
}

/// The label layer: ungated, corpus-derived text, and declaring the cluster layer it edges into.
///
/// **Ungated on purpose.** Every withholding below has to come from the attachment term rather than
/// from the label layer's own gate, or the test would pass with the term deleted.
fn labels() -> LayerDeclaration {
    LayerDeclaration {
        name: LABELS.into(),
        title: Some("topics".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        visibility: None,
            artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent { name: "topic".into(), ty: "text".into(), require_member_visibility: tessera_types::layer::SuppliedRequirement::All }],
            withdraw_on_member_deletion: true,
        },
        depends_on: vec![CLUSTERS.into()],
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
                    IncomingContent::new(
                        vec!["shipping and logistics".into()],
                        fx.members(0..150),
                    ),
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

fn labels_in(served: &[ArtifactOut]) -> Vec<&ArtifactOut> {
    served.iter().filter(|a| a.layer == LABELS).collect()
}

fn artifact_entity(engine: &Engine, id: TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// Whether the identifier route still answers — the route that traverses no edge, and the one the
/// extra predicate term exists for.
fn reachable_by_identifier(engine: &Engine, credential: &[u8], id: TesseraId) -> bool {
    let session = engine.authorise(credential).unwrap();
    engine.artifact(&session, id, None, "s0").unwrap().is_some()
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
    assert_eq!(labels_in(&artifacts_of(&engine, &full_coverage_credential())).len(), 1);
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

    // The ungated principal — term 0, which every document carries, so nothing here turns on the
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
    assert!(
        format!("{err}").contains("holds no such artifact"),
        "{err}"
    );

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
