//! Stage 2's write half: artifacts get in, land on entities of their own, and come back after a
//! restart on the same entities the caller was handed identifiers for.
//!
//! The read half — masked counts against a tile — is `artifacts.rs`'s predicate, tested there, and
//! reaches the viewport with the row-space projection. What is pinned here is everything between a
//! caller's batch and durable state, because each of these fails in a way that looks like success:
//! an artifact republished onto an ordinal that already existed still serves *an* artifact; a
//! membership lost at a restart still serves a cluster, just an empty one; and an entity reissued
//! after a rotation still resolves a `tessera_id`, just to the wrong thing.

mod common;

use common::*;
use tessera_engine::Engine;
use tessera_lifecycle::{wal::ChangeOp, IncomingArtifact};
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerAccess, LayerDeclaration,
    MembershipSource,
};
use tessera_types::EntityId;

fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: format!("{name} (title)"),
        slices: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        access: LayerAccess {
            label: None,
            artifacts_carry_own: false,
        },
        visible_when: Some(ExistenceCriterion::MinVisible(2)),
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

    /// The corpus entities behind a run of source ids — what a clustering pipeline would resolve
    /// its members to.
    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids
            .map(|s| EntityId::new(map[&s]))
            .collect()
    }
}

fn artifact(key: &str, members: Vec<EntityId>) -> IncomingArtifact {
    IncomingArtifact::from_entities(Some(key.into()), members)
}

/// Invert an artifact's `tessera_id` through the admin plane's own resolver, the way
/// `/control/changes` does — so this exercises the misdirection guard rather than going round it.
fn artifact_entity(engine: &Engine, id: tessera_types::TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0]
        .expect("an artifact identifier names the entity this deployment issued for it")
}

/// The batch is the commit unit, and each artifact gets an entity of its own — the address by which
/// it can later be suppressed, and the only one that crosses the wire.
#[test]
fn a_published_batch_takes_one_entity_per_artifact_and_none_of_them_is_the_layers() {
    let fx = fixture();
    let engine = fx.open();
    let layer_id = engine.register_layer(declaration("clusters/a")).unwrap();

    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![
                artifact("c0", fx.members(0..40)),
                artifact("c1", fx.members(40..90)),
                artifact("c2", fx.members(90..95)),
            ],
        )
        .expect("a batch into a registered enumerated layer publishes");

    assert_eq!(ids.len(), 3);
    assert_eq!(engine.published_artifacts(), 3);

    // Distinct identifiers, distinct entities, and each resolves to the ordinal it was published
    // at — in the caller's submitted order.
    let entities: Vec<EntityId> = ids.iter().map(|id| artifact_entity(&engine, *id)).collect();
    let distinct: std::collections::BTreeSet<_> = entities.iter().collect();
    assert_eq!(distinct.len(), 3, "one entity per artifact");

    for (i, entity) in entities.iter().enumerate() {
        let at = engine.locate_artifact(*entity).expect("it is an artifact");
        assert_eq!(at.layer, "clusters/a");
        assert_eq!(at.level, 0);
        assert_eq!(at.ordinal, i as u32);
        assert_eq!(at.stable_key.as_deref(), Some(["c0", "c1", "c2"][i]));
    }

    // The layer's own entity is not one of them, which is what keeps suppressing the layer from
    // suppressing artifact ordinal zero.
    let layer_entity = artifact_entity(&engine, layer_id);
    assert!(
        engine.locate_artifact(layer_entity).is_none(),
        "a layer sits outside every level's run"
    );
    assert!(!entities.contains(&layer_entity));
}

/// A second batch continues the first's numbering. Restarting it would overwrite the first batch's
/// artifacts in place — same ordinals, same entities, different members — and every `tessera_id`
/// the first batch handed out would silently name the second's clusters.
#[test]
fn a_second_batch_continues_the_numbering_rather_than_restarting_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();

    let first = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![artifact("c0", fx.members(0..10))],
        )
        .unwrap();
    let second = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![artifact("c1", fx.members(10..20))],
        )
        .unwrap();

    assert_ne!(first[0], second[0]);
    assert_eq!(
        engine
            .locate_artifact(artifact_entity(&engine, second[0]))
            .unwrap()
            .ordinal,
        1
    );
    assert_eq!(engine.published_artifacts(), 2);

    // And a key already in the level is refused, naming the reason: publication is append-only,
    // and an edit is a delete plus a re-publish.
    let refused = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![artifact("c0", fx.members(20..30))],
        )
        .expect_err("the key is taken");
    assert!(
        refused.to_string().contains("append-only"),
        "the refusal says why, since it is the caller's to fix: {refused}"
    );
    assert_eq!(engine.published_artifacts(), 2, "and nothing was published");
}

/// A member with no row would count towards the artifact's declared size — the denominator the
/// proportional criterion divides by — while being visible to nobody, so an artifact could be
/// pushed below its own existence threshold by members that can never be seen.
#[test]
fn a_membership_naming_something_other_than_a_point_is_refused() {
    let fx = fixture();
    let engine = fx.open();
    let layer_id = engine.register_layer(declaration("clusters/a")).unwrap();
    let layer_entity = artifact_entity(&engine, layer_id);

    let mut members = fx.members(0..10);
    members.push(layer_entity);
    let refused = engine
        .publish_artifacts("clusters/a".into(), 0, vec![artifact("c0", members)])
        .expect_err("a layer is not a document");
    assert!(
        refused.to_string().contains("no point"),
        "the refusal names what is wrong with it: {refused}"
    );
    assert_eq!(engine.published_artifacts(), 0);
}

/// **The durability assertion.** The WAL is the only place a membership lives, so a restart is the
/// whole test: the artifacts must come back at the same ordinals, on the same entities, with the
/// same keys — and the row-less mark must have moved far enough that nothing reissues them.
#[test]
fn a_publication_survives_a_restart_and_its_entities_are_not_reissued() {
    let fx = fixture();
    let (ids, low_water) = {
        let engine = fx.open();
        engine.register_layer(declaration("clusters/a")).unwrap();
        let ids = engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![
                    artifact("c0", fx.members(0..40)),
                    artifact("c1", fx.members(40..90)),
                ],
            )
            .unwrap();
        (ids, engine.allocator_low_water())
    };

    let engine = fx.open();
    assert_eq!(engine.published_artifacts(), 2);
    assert!(
        engine.allocator_low_water() <= low_water,
        "the row-less mark comes back at least as far along as it was; a mark that regressed \
         would reissue these artifacts' entities to the next layer registered"
    );

    for (i, id) in ids.iter().enumerate() {
        let at = engine
            .locate_artifact(artifact_entity(&engine, *id))
            .expect("the identifier the caller holds still names this artifact");
        assert_eq!(at.ordinal, i as u32);
        assert_eq!(at.stable_key.as_deref(), Some(["c0", "c1"][i]));
    }

    // A fresh registration after the restart must not land on an entity an artifact holds.
    engine.register_layer(declaration("clusters/b")).unwrap();
    let published: Vec<EntityId> = ids.iter().map(|id| artifact_entity(&engine, *id)).collect();
    engine
        .publish_artifacts(
            "clusters/b".into(),
            0,
            vec![artifact("d0", fx.members(0..10))],
        )
        .unwrap();
    for entity in &published {
        let at = engine.locate_artifact(*entity).unwrap();
        assert_eq!(
            at.layer, "clusters/a",
            "an entity issued before the restart still names the artifact it was issued for"
        );
    }
}

/// An artifact takes an entity precisely so `/control/changes` works on it unchanged — the same
/// route, the same two removal rules, no second mechanism.
#[test]
fn an_artifacts_entity_takes_a_suppression_like_any_other() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![artifact("c0", fx.members(0..40))],
        )
        .unwrap();

    let entity = artifact_entity(&engine, ids[0]);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("an artifact suppression is an ordinary change");
    assert!(engine.generation().overlay.is_suppressed(entity));

    // And it comes back — a suppression is the reversible one of the two removal rules.
    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert!(!engine.generation().overlay.is_suppressed(entity));
}
