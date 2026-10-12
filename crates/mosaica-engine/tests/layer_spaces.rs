//! Each layer has a space, which its own identifier and its artifacts' carry under the kind bit.
//! An identifier routes by its kind, a layer is removed by dropping it and never by deleting its
//! identifier, and a name registered again after a drop is a new layer in a new space, none of
//! whose identifiers the dropped layer issued.

mod common;

use common::*;
use mosaica_engine::{AcceptError, AddressTable, Engine};
use mosaica_lifecycle::resolve::Refusal;
use mosaica_lifecycle::wal::{ChangeOp, WalRecord};
use mosaica_lifecycle::membership::{IncomingAttachment, IncomingGrowth};
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

fn count(engine: &Engine, id: MosaicaId) -> Option<u64> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine.artifact(&session, id, "s0", None).unwrap()?;
    Some(out.masked_count)
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
/// different things, and so do an item's and the artifact identifier over its number. The layer's
/// own entity is 0 of its space, so the item identifier over its bits names item 0.
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
    for id in [artifact, artifact_as_item, item_as_artifact, layer] {
        assert!(!item_card(&engine, id), "{id:?} names no item");
    }
    assert!(item_card(&engine, layer_as_item), "item 0 shares the layer's entity number");
    assert_eq!(
        engine
            .resolve_targets(&[artifact, artifact_as_item, layer, layer_as_item, item])
            .unwrap(),
        vec![
            Some(Target::Artifact(space, entity)),
            None,
            Some(Target::Artifact(space, layer_entity)),
            Some(Target::Item(ShardId::ZERO, layer_entity)),
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

/// The deny state the side-manifest records for `space`: `(deleted, suppressed)` entity ids.
fn space_deny(fx: &Fixture, space: LayerSpace) -> (Vec<u32>, Vec<u32>) {
    let manifest = side_manifest(&fx.root);
    let Some(record) = mosaica_store::manifest::layer_space(&manifest.layer_spaces, space) else {
        return (Vec::new(), Vec::new());
    };
    let ids = |set: &mosaica_store::manifest::EntitySet| {
        set.entities().expect("decodes").iter().collect::<Vec<u32>>()
    };
    (ids(&record.tombstones), ids(&record.deny))
}

/// An artifact's and a layer's deny is kept in its layer space's overlay, through a publication, a
/// WAL rotation that reclaims the change records and a restart: the shard's deny state never holds
/// them, and the restart opens with both still denied.
#[test]
fn an_artifacts_and_a_layers_deny_survive_a_rotation_and_a_restart() {
    let fx = fixture();
    let (artifact, kept, suppressed_layer) = {
        let engine = fx.open();
        let (_, artifact) = register_and_publish(&fx, &engine, "clusters/a");
        let (layer_b, _) = register_and_publish(&fx, &engine, "clusters/b");
        let target = artifact_entity(&engine, artifact);
        let layer_target = artifact_entity(&engine, layer_b);
        engine
            .accept_changes(vec![
                (target, ChangeOp::Suppress),
                (layer_target, ChangeOp::Suppress),
            ])
            .unwrap();
        let Target::Artifact(space, entity) = target else {
            panic!("an artifact's target")
        };
        tick_until(&engine, "the spaces' deny state's publication", WAIT, || {
            space_deny(&fx, space).1 == vec![entity.raw() as u32]
        });
        assert!(
            side_manifest(&fx.root).only_shard().deny.entities().unwrap().is_empty(),
            "the shard's deny state holds no artifact or layer"
        );
        // Twice: the first rotation seals the member holding the changes, the second reclaims it.
        tick(&engine);
        tick(&engine);
        (artifact, target, space_of(layer_b))
    };
    let changes = mosaica_lifecycle::Wal::open(&fx.wal)
        .unwrap()
        .records()
        .filter(|r| matches!(r, Ok((_, WalRecord::ChangeBatch { .. }))))
        .count();
    assert_eq!(changes, 0, "only the rotation's snapshot carries the denies");

    let engine = fx.open();
    assert!(!card(&engine, artifact), "the artifact is still suppressed");
    assert_eq!(engine.resolve_targets(&[artifact]).unwrap(), vec![Some(kept)]);
    assert_eq!(
        reachable(&engine),
        vec!["clusters/a".to_string()],
        "the layer in space {suppressed_layer} is still suppressed"
    );
    let status = engine.generation_status();
    assert_eq!((status.overlay_depth, status.layer_spaces.depth), (0, 2));
}

/// A fold removes a deleted artifact's record from its layer space and its deletion from the
/// space's deny state in the same publication, keeps every other artifact at its ordinal, and a
/// restart serves what the fold left.
#[test]
fn a_fold_removes_a_deleted_artifacts_record_from_its_space() {
    let fx = fixture();
    let (deleted, kept) = {
        let engine = fx.open();
        engine.register_layer(declaration("clusters/a")).unwrap();
        let ids = engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![
                    IncomingArtifact::from_entities(Some("c0".into()), fx.members(0..40)),
                    IncomingArtifact::from_entities(Some("c1".into()), fx.members(40..80)),
                ],
            )
            .unwrap();
        let target = artifact_entity(&engine, ids[0]);
        engine
            .accept_changes(vec![(target, ChangeOp::Delete)])
            .unwrap();
        let status = engine.generation_status();
        assert_eq!(
            (status.retirable_deletions, status.layer_spaces.retirable),
            (0, 1)
        );
        fold(&engine);
        let status = engine.generation_status();
        assert_eq!(status.layer_spaces.retirable, 0);
        assert_eq!(engine.published_artifacts(), 1);
        assert_eq!(space_deny(&fx, space_of(ids[0])), (Vec::new(), Vec::new()));
        (ids[0], ids[1])
    };

    let engine = fx.open();
    assert!(!card(&engine, deleted));
    assert!(card(&engine, kept));
    assert_eq!(engine.published_artifacts(), 1);
    let at = engine
        .locate_artifact(artifact_entity(&engine, kept))
        .unwrap();
    assert_eq!((at.ordinal, at.key.as_deref()), (1, Some("c1")));
}

/// A dropped layer's space leaves the next publication: no layer space record names it, and no
/// member pack of the shard does.
#[test]
fn a_dropped_layers_space_leaves_the_next_publication() {
    let fx = fixture();
    let engine = fx.open();
    let (dropped, _) = register_and_publish(&fx, &engine, "clusters/a");
    let (kept, _) = register_and_publish(&fx, &engine, "clusters/b");
    tick_until(&engine, "both spaces' publication", WAIT, || {
        side_manifest(&fx.root).layer_spaces.len() == 2
    });
    engine.drop_layer("clusters/a".into()).unwrap();
    tick_until(&engine, "the drop's publication", WAIT, || {
        side_manifest(&fx.root).layer_spaces.len() == 1
    });
    let manifest = side_manifest(&fx.root);
    assert_eq!(manifest.layer_spaces[0].space, space_of(kept));
    assert!(!manifest.layer_spaces[0].record_packs.is_empty());
    assert!(manifest
        .only_shard()
        .membership_extents
        .iter()
        .all(|extent| extent.space == space_of(kept)));
    assert!(manifest.partition.next_layer_space > space_of(dropped).raw());
}

/// Every file under `dir`, relative to `prefix`.
fn files_under(prefix: &std::path::Path, dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(prefix).unwrap();
                found.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}

/// A dropped layer leaves nothing behind. Of a layer holding a published, a suppressed and a
/// deleted artifact, nothing is served after the drop; its space leaves the next side-manifest,
/// and after the next fold the served prefix holds none of its member slices, row forms or
/// records. A restart brings none of it back, and the name registered again is a new layer whose
/// identifiers the dropped one never issued.
#[test]
fn a_dropped_layer_is_neither_served_nor_stored() {
    let fx = fixture();
    // Read before a fold removes the built prefix the map is read from.
    let members = fx.members(0..120);
    let (dropped, layer_a, kept) = {
        let engine = fx.open();
        let layer_a = engine.register_layer(declaration("clusters/a")).unwrap();
        let mut ids = engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![
                    IncomingArtifact::from_entities(Some("c0".into()), members[..40].to_vec()),
                    IncomingArtifact::from_entities(Some("c1".into()), members[40..80].to_vec()),
                    IncomingArtifact::from_entities(Some("c2".into()), members[80..].to_vec()),
                ],
            )
            .unwrap();
        let kept = register_and_publish(&fx, &engine, "clusters/b");
        engine
            .accept_changes(vec![
                (artifact_entity(&engine, ids[1]), ChangeOp::Suppress),
                (artifact_entity(&engine, ids[2]), ChangeOp::Delete),
            ])
            .unwrap();
        // A row form for each level, so the drop has derived files of its own to leave.
        fold(&engine);
        let space_a = space_of(layer_a);
        let manifest = side_manifest(&fx.root);
        assert!(manifest.layer_spaces.iter().any(|r| r.space == space_a));
        assert!(manifest
            .only_shard()
            .membership_extents
            .iter()
            .any(|extent| extent.space == space_a));

        engine.drop_layer("clusters/a".into()).unwrap();
        ids.push(layer_a);
        let served = |engine: &Engine| {
            assert_eq!(reachable(engine), vec!["clusters/b".to_string()]);
            assert!(ids.iter().all(|id| !card(engine, *id)));
            assert!(engine.resolve_targets(&ids).unwrap().iter().all(Option::is_none));
            assert_eq!(engine.published_artifacts(), 1);
            assert!(card(engine, kept.1));
        };
        served(&engine);
        tick_until(&engine, "the drop's publication", WAIT, || {
            side_manifest(&fx.root).layer_spaces.iter().all(|r| r.space != space_a)
        });

        fold(&engine);
        served(&engine);
        let manifest = side_manifest(&fx.root);
        let shard = manifest.only_shard();
        assert!(manifest.layer_spaces.iter().all(|r| r.space != space_a));
        assert!(shard.membership_extents.iter().all(|e| e.space != space_a));
        assert!(shard.derived_extents.iter().all(|e| e.space != space_a));
        assert!(
            shard.derived_extents.iter().any(|e| e.space == space_of(kept.0)),
            "the kept layer's row forms are where the dropped one's would be"
        );
        let prefix = fx.root.join(&engine.generation().prefix);
        let space_dir = mosaica_store::LayerSpaceLayout::new(PARTITION, space_a).dir(&prefix);
        assert!(!space_dir.exists(), "no file of the dropped space is in the new prefix");
        let mut packs: Vec<String> =
            shard.membership_extents.iter().map(|e| e.path.clone()).collect();
        packs.sort();
        assert_eq!(
            files_under(&prefix, &prefix.join(SHARD.members_rel())),
            packs,
            "every member pack in the prefix is a kept layer's"
        );
        (ids, layer_a, kept)
    };

    let engine = fx.open();
    assert_eq!(reachable(&engine), vec!["clusters/b".to_string()]);
    assert!(dropped.iter().all(|id| !card(&engine, *id)));
    assert!(engine.resolve_targets(&dropped).unwrap().iter().all(Option::is_none));
    assert_eq!(engine.published_artifacts(), 1);
    assert!(card(&engine, kept.1));

    let again_layer = engine.register_layer(declaration("clusters/a")).unwrap();
    let again_artifact = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(Some("c0".into()), members[..40].to_vec())],
        )
        .unwrap()[0];
    let again = (again_layer, again_artifact);
    assert!(space_of(again.0) > space_of(kept.0));
    assert!(space_of(again.0) > space_of(layer_a));
    assert!(!dropped.contains(&again.0) && !dropped.contains(&again.1));
    assert!(card(&engine, again.1));
    assert!(dropped.iter().all(|id| !card(&engine, *id)));
}

/// An item and an artifact at the same entity number, each in its own space, are two things: each
/// identifier reaches its own, a deny of one leaves the other, and a fold that removes one keeps
/// the other, across a restart.
#[test]
fn an_item_and_an_artifact_at_one_entity_number_are_two_things() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    // Enough items that one holds the entity the first artifact of space 0 takes.
    let first = mosaica_types::layer::RESERVED_BLOCK;
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        first + 2,
    );
    let (cache, wal) = (tmp.path().join("cache"), tmp.path().join("wal.log"));
    let open = || open_engine_publishing(&root, &cache, &wal);
    let point = EntityId::new(first);
    let other = EntityId::new(first + 1);
    let (item_id, other_id, shared, kept) = {
        let engine = open();
        engine.register_layer(declaration("clusters/a")).unwrap();
        let members = |range: std::ops::Range<u64>| range.map(EntityId::new).collect::<Vec<_>>();
        let (below, across) = (members(first - 10..first), members(first - 10..first + 2));
        let ids = engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![
                    IncomingArtifact::from_entities(Some("c0".into()), below),
                    IncomingArtifact::from_entities(Some("c1".into()), across),
                ],
            )
            .unwrap();
        let shared = ids[0];
        assert_eq!(
            artifact_entity(&engine, shared),
            Target::Artifact(LayerSpace::ZERO, point),
            "the artifact sits at the item's number"
        );
        let item_id = engine.mosaica_id_of(point).unwrap();
        let other_id = engine.mosaica_id_of(other).unwrap();
        assert!(card(&engine, shared) && item_card(&engine, item_id));

        // A deny of the item leaves the artifact, and the reverse.
        engine.accept_changes(vec![(item(point), ChangeOp::Suppress)]).unwrap();
        assert!(card(&engine, shared) && !item_card(&engine, item_id));
        engine.accept_changes(vec![(item(point), ChangeOp::Unsuppress)]).unwrap();
        engine
            .accept_changes(vec![(artifact_entity(&engine, shared), ChangeOp::Delete)])
            .unwrap();
        assert!(!card(&engine, shared) && item_card(&engine, item_id));
        engine.accept_changes(vec![(item(other), ChangeOp::Delete)]).unwrap();
        assert!(card(&engine, ids[1]), "an item's deletion deletes no artifact at its number");
        assert_eq!(count(&engine, ids[1]), Some(11));

        fold(&engine);
        assert!(!card(&engine, shared));
        assert!(item_card(&engine, item_id), "the artifact's fold kept the item");
        assert!(!item_card(&engine, other_id));
        assert_eq!(
            count(&engine, ids[1]),
            Some(11),
            "the item's fold kept the artifact, and the artifact's kept its member at that number"
        );
        assert_eq!(engine.resolve_mosaica_ids(&[item_id]).unwrap(), vec![Some(point)]);
        (item_id, other_id, shared, ids[1])
    };

    let engine = open();
    assert!(!card(&engine, shared));
    assert_eq!(count(&engine, kept), Some(11));
    assert!(item_card(&engine, item_id));
    assert!(!item_card(&engine, other_id));
    assert_eq!(
        engine.resolve_targets(&[shared, item_id]).unwrap(),
        vec![None, Some(item(point))]
    );
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A layer attached to a dropped one is not attached to the layer registered again under its name:
/// its artifacts are neither served under the new layer's artifacts nor deleted with them, before
/// a restart or after one.
#[test]
fn a_dependant_of_a_dropped_layer_does_not_attach_to_its_successor() {
    let fx = fixture();
    let label = {
        let engine = fx.open();
        register_and_publish(&fx, &engine, "clusters/a");
        let mut labels = declaration("labels/a");
        labels.depends_on = vec!["clusters/a".into()];
        engine.register_layer(labels).unwrap();
        let label = engine
            .publish_artifacts(
                "labels/a".into(),
                0,
                vec![IncomingArtifact::attached(
                    Some("l0".into()),
                    fx.members(0..40),
                    Vec::new(),
                    IncomingAttachment {
                        layer: "clusters/a".into(),
                        level: 0,
                        key: "c0".into(),
                    },
                )],
            )
            .unwrap()[0];
        assert!(card(&engine, label));
        engine.drop_layer("clusters/a".into()).unwrap();
        assert!(!card(&engine, label), "its target is gone");

        let (_, again) = register_and_publish(&fx, &engine, "clusters/a");
        assert!(!card(&engine, label), "the new layer's c0 is not its target");
        engine
            .accept_changes(vec![(artifact_entity(&engine, again), ChangeOp::Delete)])
            .unwrap();
        assert_eq!(
            engine.generation_status().layer_spaces.retirable,
            1,
            "the deletion took nothing with it"
        );
        label
    };

    let engine = fx.open();
    assert!(!card(&engine, label));
    assert_eq!(engine.generation_status().layer_spaces.retirable, 1);
}

/// A publication that grew a dropped layer's reservation is not replayed onto the layer registered
/// again under its name, when the log holding both is replayed over a side-manifest holding the
/// new one: the new layer comes back with its own runs.
#[test]
fn a_replayed_publication_into_a_dropped_layer_leaves_its_successors_runs() {
    let fx = fixture();
    let (runs, again) = {
        let engine = fx.open();
        // A growth holds the log from here until a fold, so the publication below keeps it.
        register_and_publish(&fx, &engine, "clusters/b");
        engine
            .grow_memberships(
                "clusters/b".into(),
                0,
                vec![IncomingGrowth::from_entities("c0".into(), fx.members(40..50))],
            )
            .unwrap();
        engine.register_layer(declaration("clusters/a")).unwrap();
        let past_a_block = (0..=mosaica_types::layer::RESERVED_BLOCK)
            .map(|i| IncomingArtifact::from_entities(Some(format!("c{i}")), Vec::new()))
            .collect();
        engine
            .publish_artifacts("clusters/a".into(), 0, past_a_block)
            .unwrap();
        engine.drop_layer("clusters/a".into()).unwrap();
        let (layer, again) = register_and_publish(&fx, &engine, "clusters/a");
        tick_until(&engine, "the new layer's publication", WAIT, || {
            side_manifest(&fx.root)
                .partition
                .layers
                .iter()
                .any(|l| l.space == space_of(layer))
        });
        let runs = engine.registered_layer("clusters/a").unwrap().runs;
        assert_eq!(runs[0].capacity(), mosaica_types::layer::RESERVED_BLOCK);
        (runs, again)
    };
    let creates = mosaica_lifecycle::Wal::open(&fx.wal)
        .unwrap()
        .records()
        .filter(|r| {
            matches!(r, Ok((_, WalRecord::LayerCreate { declaration, .. }))
                if declaration.name == "clusters/a")
        })
        .count();
    assert_eq!(creates, 2, "the log still holds the dropped registration");

    let engine = fx.open();
    assert_eq!(engine.registered_layer("clusters/a").unwrap().runs, runs);
    assert!(card(&engine, again));
}
