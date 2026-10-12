//! Each layer has a space, which its own identifier and its artifacts' carry under the kind bit.
//! An identifier routes by its kind, a layer is removed by dropping it and never by deleting its
//! identifier, and a name registered again after a drop is a new layer in a new space, none of
//! whose identifiers the dropped layer issued.

mod common;

use common::*;
use mosaica_engine::{AcceptError, AddressTable, Engine};
use mosaica_lifecycle::resolve::Refusal;
use mosaica_lifecycle::wal::{ChangeOp, WalRecord};
use mosaica_lifecycle::{ExecError, IncomingArtifact};
use mosaica_types::layer::{
    ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
};
use mosaica_types::{EntityId, LayerSpace, MosaicaId, ShardId, Target};

fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: Some(ExistenceCriterion::Count(2)),
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: Default::default(),
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

    fn members(&self, sources: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        sources.map(|s| EntityId::new(map[&s])).collect()
    }
}

/// Register `name` and publish one artifact `c0` into it: the layer's identifier and the
/// artifact's.
fn register_and_publish(fx: &Fixture, engine: &Engine, name: &str) -> (MosaicaId, MosaicaId) {
    let layer = engine.register_layer(declaration(name)).unwrap();
    let ids = engine
        .publish_artifacts(
            name.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..40),
            )],
        )
        .unwrap();
    (layer, ids[0])
}

fn card(engine: &Engine, id: MosaicaId) -> bool {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine.artifact(&session, id, "s0", None).unwrap().is_some()
}

fn item_card(engine: &Engine, id: MosaicaId) -> bool {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine.item(&session, id).unwrap().is_some()
}

fn reachable(engine: &Engine) -> Vec<String> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .visible_layers(&session)
        .into_iter()
        .map(|l| l.declaration.name)
        .collect()
}

fn space_of(id: MosaicaId) -> LayerSpace {
    test_key().invert_artifact(id).expect("an artifact's identifier").0
}

/// An artifact's identifier and the item identifier over the same 32 bits but the kind bit name
/// different things, and so do an item's and the artifact identifier over its number.
#[test]
fn an_identifier_routes_by_its_kind() {
    let fx = fixture();
    let engine = fx.open();
    let (layer, artifact) = register_and_publish(&fx, &engine, "clusters/a");
    let key = test_key();
    let (space, entity) = key.invert_artifact(artifact).unwrap();
    assert_eq!(space, LayerSpace::ZERO, "the first layer takes the first space");
    assert_eq!(key.invert(artifact), None);
    let (_, layer_entity) = key.invert_artifact(layer).unwrap();
    // Space 0 under kind 0 is shard 0 at tenancy 0: these differ from the two above in the kind
    // bit alone.
    let artifact_as_item = mosaica_id_at(entity, 0);
    let layer_as_item = mosaica_id_at(layer_entity, 0);

    let point = fx.members(0..1)[0];
    let item = engine.mosaica_id_of(point).unwrap();
    let (high, number) = key.invert(item).unwrap();
    assert_eq!(high.shard, ShardId::ZERO);
    let item_as_artifact = key.forward_artifact(space, number).unwrap();

    assert!(card(&engine, artifact));
    assert!(item_card(&engine, item));
    for id in [artifact_as_item, item, item_as_artifact, layer, layer_as_item] {
        assert!(!card(&engine, id), "{id:?} names no artifact");
    }
    for id in [artifact, artifact_as_item, item_as_artifact, layer, layer_as_item] {
        assert!(!item_card(&engine, id), "{id:?} names no item");
    }
    assert_eq!(
        engine
            .resolve_targets(&[artifact, artifact_as_item, layer, layer_as_item, item])
            .unwrap(),
        vec![
            Some(Target::Artifact(space, entity)),
            None,
            Some(Target::Artifact(space, layer_entity)),
            None,
            Some(Target::Item(ShardId::ZERO, point)),
        ]
    );

    // The changes route names each by its kind, and refuses the twins as naming nothing.
    let table = AddressTable {
        rows: 4,
        mosaica_id: Some(vec![
            Some(artifact),
            Some(artifact_as_item),
            Some(item),
            Some(item_as_artifact),
        ]),
        columns: Vec::new(),
    };
    let named = engine.name_targets(&table).unwrap();
    assert_eq!(named.verdicts[0], Ok(Target::Artifact(space, entity)));
    assert_eq!(
        named.verdicts[1],
        Err(Refusal::UnknownMosaicaId { row: 1 })
    );
    assert_eq!(named.verdicts[2], Ok(Target::Item(ShardId::ZERO, point)));
    assert_eq!(
        named.verdicts[3],
        Err(Refusal::UnknownMosaicaId { row: 3 })
    );

    // Suppressing the artifact by its target withholds it and leaves the item it shares bits with
    // served.
    engine
        .accept_changes(vec![(Target::Artifact(space, entity), ChangeOp::Suppress)])
        .unwrap();
    assert!(!card(&engine, artifact));
    assert!(item_card(&engine, item));
}

/// A layer is removed by dropping it: a deletion naming its own identifier is refused, with
/// everything else in the request, and a suppression of it is not.
#[test]
fn a_layer_is_not_deleted_by_its_own_identifier_and_is_suppressed_by_it() {
    let fx = fixture();
    let engine = fx.open();
    let (layer, _) = register_and_publish(&fx, &engine, "clusters/a");
    let target = engine.resolve_targets(&[layer]).unwrap()[0].unwrap();
    let point = fx.members(50..51)[0];

    let refused = engine
        .accept_changes(vec![(item(point), ChangeOp::Delete), (target, ChangeOp::Delete)])
        .expect_err("a layer is dropped, not deleted");
    assert!(matches!(
        refused,
        AcceptError::Exec(ExecError::LayerRefused { .. })
    ));
    assert_eq!(reachable(&engine), vec!["clusters/a".to_string()]);
    assert_eq!(
        engine.resolve_mosaica_ids(&[engine.mosaica_id_of(point).unwrap()]).unwrap(),
        vec![Some(point)],
        "nothing in the refused request was applied"
    );

    engine
        .accept_changes(vec![(target, ChangeOp::Suppress)])
        .expect("a layer is suppressed by its identifier");
    assert!(reachable(&engine).is_empty());
    engine
        .accept_changes(vec![(target, ChangeOp::Unsuppress)])
        .unwrap();
    assert_eq!(reachable(&engine), vec!["clusters/a".to_string()]);
}

/// A dropped layer's name registers again, as a layer in a new space: no identifier the dropped
/// layer issued names the new layer or its artifacts, before a restart or after one.
#[test]
fn a_name_registered_again_after_a_drop_shares_no_identifier_with_the_dropped_layer() {
    let fx = fixture();
    let (old, new) = {
        let engine = fx.open();
        let old = register_and_publish(&fx, &engine, "clusters/a");
        engine.drop_layer("clusters/a".into()).unwrap();
        assert_eq!(engine.resolve_targets(&[old.0, old.1]).unwrap(), vec![None, None]);
        assert!(!card(&engine, old.1));

        let new = register_and_publish(&fx, &engine, "clusters/a");
        assert!(space_of(new.0) > space_of(old.0));
        assert_eq!(space_of(new.1), space_of(new.0));
        assert_ne!(new.1, old.1, "the same ordinal in another space");
        assert!(card(&engine, new.1));
        assert!(!card(&engine, old.1));
        assert_eq!(engine.resolve_targets(&[old.0, old.1]).unwrap(), vec![None, None]);
        assert_eq!(engine.published_artifacts(), 1, "the dropped layer's artifact is gone");
        (old, new)
    };

    let engine = fx.open();
    assert_eq!(engine.resolve_targets(&[old.0, old.1]).unwrap(), vec![None, None]);
    assert!(!card(&engine, old.1));
    assert!(card(&engine, new.1));
    let at = engine
        .locate_artifact(engine.resolve_targets(&[new.1]).unwrap()[0].unwrap())
        .unwrap();
    assert_eq!((at.layer.as_str(), at.ordinal), ("clusters/a", 0));
    assert_eq!(engine.published_artifacts(), 1);
    assert_eq!(
        engine.registered_layer("clusters/a").unwrap().space,
        space_of(new.0)
    );
}

/// The space counter is in the side-manifest, so a dropped layer's space is not given again once
/// the log no longer holds its registration.
#[test]
fn a_dropped_layers_space_is_not_given_again_after_a_rotation_and_a_restart() {
    let fx = fixture();
    {
        let engine = fx.open();
        for name in ["clusters/a", "clusters/b"] {
            engine.register_layer(declaration(name)).unwrap();
            engine.drop_layer(name.into()).unwrap();
        }
        tick_until(&engine, "the counter's publication", WAIT, || {
            side_manifest(&fx.root).partition.next_layer_space == 2
        });
        // Twice: the first rotation seals the member holding the registrations, the second
        // reclaims it.
        tick(&engine);
        tick(&engine);
    }
    let creates = mosaica_lifecycle::Wal::open(&fx.wal)
        .unwrap()
        .records()
        .filter(|r| matches!(r, Ok((_, WalRecord::LayerCreate { .. }))))
        .count();
    assert_eq!(creates, 0, "the log no longer holds the registrations");

    let engine = fx.open();
    let id = engine.register_layer(declaration("clusters/a")).unwrap();
    assert_eq!(space_of(id).raw(), 2);
    tick_until(&engine, "the counter's publication", WAIT, || {
        side_manifest(&fx.root).partition.next_layer_space == 3
    });
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(30);
