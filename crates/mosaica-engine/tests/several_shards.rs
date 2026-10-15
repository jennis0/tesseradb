//! A bundle a build wrote past its `[shards] size`, opened by a running service: it answers every
//! read as the same items built as one shard answer it, names each item in its own shard, and
//! refuses every write unless the engine is opened to write several shards.

mod common;

use arrow::array::UInt64Array;
use arrow::record_batch::RecordBatch;

use common::*;
use mosaica_engine::{
    AcceptError, AggregateRequest, Engine, EngineError, ExecutorPosture, Grouping, RecordsOrder,
    ScalarOut, Session, ViewportRequest,
};
use mosaica_lifecycle::resolve::Refusal;
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::{DeclaredValue, IncomingArtifact, IncomingGrowth, VocabularyRequest};
use mosaica_store::manifest::EntitySet;
use mosaica_types::{EntityId, ItemAt, MosaicaId, ShardId, Target};

/// Items the fixture builds.
const N: u64 = 40;
/// The size the several-shard build is declared at: shards of 25 and 15 items.
const SIZE: u64 = 25;

fn u64s(batch: &RecordBatch, name: &str) -> Vec<u64> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("the page carries `{name}`"))
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap()
        .values()
        .to_vec()
}

/// Each tile's visible count at `depth` over the whole map.
fn tile_counts(engine: &Engine, session: &Session, depth: u8) -> Vec<(u64, u64)> {
    let out = engine
        .viewport(
            session,
            ViewportRequest::new("s0", depth, WHOLE_MAP, N as usize),
        )
        .expect("the viewport answers");
    let mut counts: Vec<(u64, u64)> = out.tiles.iter().map(|t| (t.tile, t.visible)).collect();
    counts.sort_unstable();
    counts
}

/// Each cell's count at `depth`, from the aggregate.
fn cell_counts(engine: &Engine, session: &Session, depth: u8) -> Vec<(u64, u64)> {
    let groupings = [Grouping {
        by: None,
        cells: Some(depth),
        area: None,
    }];
    let mut pages = Pages::default();
    engine
        .aggregate_stream(
            session,
            AggregateRequest {
                view: "s0",
                filter: None,
                reference: None,
                groupings: &groupings,
                page_rows: None,
                pages: None,
                cursor: None,
                limits: limits(),
                caps: caps(),
                cancel: None,
            },
            &mut pages,
        )
        .expect("the aggregate answers");
    let mut counts: Vec<(u64, u64)> = pages
        .batches
        .iter()
        .flat_map(|batch| u64s(batch, "cell").into_iter().zip(u64s(batch, "count")))
        .collect();
    counts.sort_unstable();
    counts
}

/// The `id` the item card of `mosaica_id` carries, or `None` where the viewer is served no card.
fn card_id(engine: &Engine, session: &Session, mosaica_id: u64) -> Option<u64> {
    let card = engine
        .item(session, mosaica_types::MosaicaId::new(mosaica_id))
        .expect("the item card answers")?;
    card.fields
        .iter()
        .find(|field| field.name == "id")
        .map(|field| match field.value {
            ScalarOut::U64(id) => id,
            ref other => panic!("`id` is a u64, not {other:?}"),
        })
}

/// What a viewer is served, by the fixture's own `id`, which both bundles share.
#[derive(Debug, PartialEq)]
struct Served {
    /// The items a bulk read answers, each checked against its card.
    ids: Vec<u64>,
    /// Each tile's visible count.
    tiles: Vec<(u64, u64)>,
    /// Each cell's count, from the aggregate.
    cells: Vec<(u64, u64)>,
}

fn served(engine: &Engine, credential: &[u8]) -> Served {
    let session = engine.authorise(credential).unwrap();
    let read = read_items(engine, &session, &Items::new(RecordsOrder::Stored)).rows_with_ids;
    for &(mosaica_id, id) in &read {
        assert_eq!(
            card_id(engine, &session, mosaica_id),
            Some(id),
            "item {id}'s card"
        );
    }
    let mut ids: Vec<u64> = read.iter().map(|&(_, id)| id).collect();
    ids.sort_unstable();
    Served {
        ids,
        tiles: tile_counts(engine, &session, 3),
        cells: cell_counts(engine, &session, 4),
    }
}

#[test]
fn a_bundle_of_several_shards_answers_every_read_as_one_shard_does() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    assert_eq!(
        shards_of(&several.root).1.len(),
        2,
        "the build wrote two shards"
    );
    let (one, several) = (one.open(), several.open());

    for credential in [full_coverage_credential(), subset_credential()] {
        let answered = served(&several, &credential);
        assert_eq!(answered, served(&one, &credential));
        let ids = answered.ids;
        let want: Vec<u64> = (0..N)
            .filter(|&e| credential == full_coverage_credential() || subset_sees(e))
            .collect();
        assert_eq!(ids, want, "every item the viewer may see, from both shards");
    }

    // A card the viewer may not see is not served, from either shard.
    let full = several.authorise(&full_coverage_credential()).unwrap();
    let subset = several.authorise(&subset_credential()).unwrap();
    let read = read_items(&several, &full, &Items::new(RecordsOrder::Stored));
    for (mosaica_id, id) in read.rows_with_ids {
        let card = card_id(&several, &subset, mosaica_id);
        assert_eq!(
            card,
            subset_sees(id).then_some(id),
            "item {id}'s card to the subset"
        );
    }
}

/// The entities `ids`, as a side-manifest records a deny state.
fn entities(ids: &[u32]) -> EntitySet {
    EntitySet::of(&ids.iter().copied().collect())
}

/// Each shard opens with its own record's deletions and suppressions: what the same denies leave
/// served in a bundle of one shard, every shard's counted in the overlay figures.
#[test]
fn each_shard_opens_with_its_own_denies() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let before = {
        let engine = several.open();
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        read_items(&engine, &session, &Items::new(RecordsOrder::Stored)).rows_with_ids
    };
    // Shard 0 suppresses its entity 2; shard 1 suppresses its entity 3 and deletes its entity 4,
    // which are entities 25 + 3 and 25 + 4 of the bundle of one shard.
    edit_side_manifest(&several.root, |manifest| {
        manifest.shards.records[0].deny = entities(&[2]);
        manifest.shards.records[1].deny = entities(&[3]);
        manifest.shards.records[1].tombstones = entities(&[4]);
    });
    edit_side_manifest(&one.root, |manifest| {
        let base = SIZE as u32;
        manifest.shards.records[0].deny = entities(&[2, base + 3]);
        manifest.shards.records[0].tombstones = entities(&[base + 4]);
    });
    let (one, several) = (one.open(), several.open());

    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(served(&several, &credential), served(&one, &credential));
    }
    let full = several.authorise(&full_coverage_credential()).unwrap();
    let after = served(&several, &full_coverage_credential()).ids;
    let denied: Vec<(u64, u64)> = before
        .into_iter()
        .filter(|(_, id)| !after.contains(id))
        .collect();
    assert_eq!(denied.len(), 3, "both shards' denies are in force");
    for (mosaica_id, id) in denied {
        assert_eq!(
            card_id(&several, &full, mosaica_id),
            None,
            "item {id}'s card"
        );
    }
    assert_eq!(several.overlay_depth(), 3);
    assert_eq!(several.retirable_deletions(), 1);
    let (status, one_status) = (several.generation_status(), one.generation_status());
    assert_eq!(
        (status.overlay_depth, status.retirable_deletions),
        (one_status.overlay_depth, one_status.retirable_deletions)
    );
}

#[test]
fn status_sums_the_shards() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), several.open());
    assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);

    assert_eq!(engine.allocator_high_water(), N);
    assert_eq!(
        engine.allocator_high_water(),
        one_engine.allocator_high_water()
    );
    assert_eq!(engine.shard_size(), SIZE);
    let (status, one_status) = (engine.generation_status(), one_engine.generation_status());
    assert_eq!(status.live_rows, one_status.live_rows);
    assert_eq!(status.overlay_depth, one_status.overlay_depth);
    assert_eq!(status.retirable_deletions, one_status.retirable_deletions);
    let watermarks = |s: &mosaica_engine::GenerationStatus| -> Vec<u64> {
        s.partitions.iter().map(|p| p.watermark).collect()
    };
    assert_eq!(watermarks(&status), vec![N]);
    assert_eq!(watermarks(&status), watermarks(&one_status));
    let manifest = side_manifest(&several.root);
    let held: usize = manifest
        .shards
        .records
        .iter()
        .map(|r| r.segments.len())
        .sum();
    assert_eq!(status.segments.len(), 1);
    assert_eq!(
        status.segments[0].segments, held,
        "every shard's segments of the view"
    );
}

/// Every write kind, through the engine.
fn writes(engine: &Engine) -> Vec<(&'static str, Result<(), AcceptError>)> {
    let shard = ShardId::new(1).unwrap();
    let item = Target::Item(shard, EntityId::new(0));
    let frame = mosaica_engine::DeclaredFrame {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    };
    vec![
        (
            "ingest",
            engine
                .ingest_rows(vec![keyed_row("new")], "b".into(), [1; 32])
                .map(drop),
        ),
        (
            "change",
            engine.accept_change(EntityId::new(0), ChangeOp::Suppress),
        ),
        (
            "changes",
            engine.accept_changes(vec![(item, ChangeOp::Delete)]),
        ),
        (
            "submitted changes",
            engine
                .submit_changes(vec![(item, ChangeOp::Suppress)])
                .map(drop),
        ),
        (
            "layer",
            engine.register_layer(flat_declaration("l")).map(drop),
        ),
        ("layer drop", engine.drop_layer("l".into())),
        (
            "attribute",
            engine
                .declare_attribute(mosaica_engine::AttributeRequest {
                    name: "a".into(),
                    title: None,
                    ty: "u64".into(),
                    vocabulary: None,
                    analyser: None,
                    index: false,
                    render: false,
                    scope: mosaica_types::layer::LayerScope::Entity,
                    unique: false,
                })
                .map(drop),
        ),
        ("shards", engine.declare_shards(SIZE + 1).map(drop)),
        (
            "vocabulary",
            engine
                .declare_vocabulary(VocabularyRequest {
                    name: "v".into(),
                    title: None,
                    kind: mosaica_types::vocabulary::VocabularyKind::Declared,
                    visibility: mosaica_types::vocabulary::Visibility::Public,
                    width: "u8".into(),
                    values: Vec::new(),
                    reserved: Vec::new(),
                })
                .map(drop),
        ),
        (
            "values",
            engine
                .mint_vocabulary_values(
                    "v".into(),
                    vec![DeclaredValue {
                        key: "k".into(),
                        title: None,
                    }],
                )
                .map(drop),
        ),
        (
            "view group",
            engine
                .create_view_group(mosaica_lifecycle::wal::ViewGroupDeclaration {
                    name: "g".into(),
                    title: None,
                    projection: "none".into(),
                    frame,
                    visibility: None,
                    point_default: None,
                    members: None,
                    metadata: Vec::new(),
                })
                .map(drop),
        ),
        (
            "plain view",
            engine
                .create_plain_view(mosaica_engine::PlainViewDeclaration {
                    name: "p".into(),
                    title: None,
                    projection: "none".into(),
                    frame,
                    visibility: None,
                    point_default: None,
                })
                .map(drop),
        ),
        (
            "view",
            engine.create_view("g".into(), "k".into(), None, Default::default()),
        ),
        (
            "view drop",
            engine.drop_view("g".into(), "k".into()).map(drop),
        ),
        (
            "artifacts",
            engine
                .put_artifacts(
                    "l".into(),
                    0,
                    vec![IncomingArtifact::from_entities(
                        Some("a".into()),
                        [EntityId::new(0)],
                    )],
                )
                .map(drop),
        ),
        (
            "growth",
            engine
                .grow_memberships(
                    "l".into(),
                    0,
                    vec![IncomingGrowth {
                        key: "a".into(),
                        view: None,
                        joining: mosaica_lifecycle::ShardSets::in_shard(
                            ShardId::ZERO,
                            croaring::Bitmap::of(&[0]),
                        ),
                        leaving: Default::default(),
                        rank: None,
                        parts: Default::default(),
                    }],
                )
                .map(drop),
        ),
    ]
}

#[test]
fn every_write_is_refused_and_a_restart_reopens_the_bundle() {
    let built = fixture_sized(N, Some(SIZE));
    let engine = built.open();
    let before = served(&engine, &full_coverage_credential());

    assert!(matches!(
        engine.writable(),
        Err(AcceptError::SeveralShards { shards: 2 })
    ));
    for (kind, answered) in writes(&engine) {
        assert!(
            matches!(answered, Err(AcceptError::SeveralShards { shards: 2 })),
            "{kind}: {answered:?}"
        );
    }
    assert_eq!(engine.allocator_high_water(), N, "nothing was allocated");
    assert_eq!(
        served(&engine, &full_coverage_credential()),
        before,
        "nothing changed"
    );

    drop(engine);
    let engine = built.open();
    assert_eq!(served(&engine, &full_coverage_credential()), before);
}

#[test]
fn a_bundle_of_one_shard_takes_every_write() {
    let built = fixture_sized(N, None);
    let engine = built.open();
    assert!(engine.writable().is_ok());
    let refused: Vec<&str> = writes(&engine)
        .into_iter()
        .filter(|(_, answered)| matches!(answered, Err(AcceptError::SeveralShards { .. })))
        .map(|(kind, _)| kind)
        .collect();
    assert!(refused.is_empty(), "{refused:?}");
}

/// No write to a bundle of several shards can be in a log, so a log beside one that holds a
/// record is another bundle's, and the open refuses it.
#[test]
fn a_log_holding_records_is_refused_beside_several_shards() {
    let one = fixture_sized(N, None);
    let engine = one.open();
    ingest(&engine, "a");
    drop(engine);

    let several = fixture_sized(N, Some(SIZE));
    let opened = Engine::open(&several.root, &several.cache, &one.wal, config());
    assert!(
        matches!(opened, Err(EngineError::SeveralShardsLogged { shards: 2 })),
        "{:?}",
        opened.err()
    );
}

/// A table naming each of `read`'s items by its `mosaica_id` and by its unique `id`.
fn by_identifier_and_id(read: &[(u64, u64)]) -> mosaica_engine::AddressTable {
    mosaica_engine::AddressTable {
        rows: read.len(),
        mosaica_id: Some(
            read.iter()
                .map(|&(mosaica_id, _)| Some(MosaicaId::new(mosaica_id)))
                .collect(),
        ),
        columns: vec![(
            "id".to_string(),
            read.iter()
                .map(|&(_, id)| Some(mosaica_engine::AddressValue::Text(id.to_string())))
                .collect(),
        )],
    }
}

/// Each item is named in its own shard: its `mosaica_id` and its unique value name the one item,
/// the items of both shards are distinct where their entity numbers repeat, and a row naming an
/// item of each shard that holds one entity number names two items.
#[test]
fn each_item_is_named_in_its_own_shard() {
    let built = fixture_sized(N, Some(SIZE));
    let engine = built.open();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let read = read_items(&engine, &session, &Items::new(RecordsOrder::Stored)).rows_with_ids;
    assert_eq!(read.len(), N as usize);

    let named = engine.name_items(&by_identifier_and_id(&read)).unwrap();
    let items: Vec<ItemAt> = named
        .verdicts
        .iter()
        .map(|verdict| verdict.clone().expect("each row names its item"))
        .collect();
    let distinct: std::collections::BTreeSet<ItemAt> = items.iter().copied().collect();
    assert_eq!(distinct.len(), N as usize, "no two items are one");
    let in_shard = |shard: u32| -> std::collections::BTreeSet<EntityId> {
        let shard = ShardId::new(shard).unwrap();
        items
            .iter()
            .filter(|item| item.shard == shard)
            .map(|item| item.entity)
            .collect()
    };
    let (zero, one) = (in_shard(0), in_shard(1));
    assert_eq!(
        (zero.len(), one.len()),
        (SIZE as usize, (N - SIZE) as usize)
    );
    let shared: Vec<EntityId> = zero.intersection(&one).copied().collect();
    assert!(!shared.is_empty(), "the shards repeat entity numbers");

    let ids: Vec<MosaicaId> = read.iter().map(|&(id, _)| MosaicaId::new(id)).collect();
    assert_eq!(
        engine.resolve_targets(&ids).unwrap(),
        items
            .iter()
            .map(|item| Some(item.target()))
            .collect::<Vec<_>>()
    );
    let by_id = |shard: u32| {
        let at = items
            .iter()
            .position(|item| *item == ItemAt::new(ShardId::new(shard).unwrap(), shared[0]))
            .unwrap();
        read[at]
    };
    let (in_zero, in_one) = (by_id(0), by_id(1));
    let crossed = engine
        .name_items(&by_identifier_and_id(&[(in_one.0, in_zero.1)]))
        .unwrap();
    assert!(
        matches!(&crossed.verdicts[0], Err(Refusal::NamesTwo { .. })),
        "{:?}",
        crossed.verdicts
    );
}

/// An engine opened to write several shards admits writes and starts its executor, and opens a
/// log holding a record beside the shards, which [`Engine::open`] refuses.
#[cfg(feature = "several-shard-writes")]
#[test]
fn an_engine_writing_several_shards_admits_writes_and_a_log() {
    let several = fixture_sized(N, Some(SIZE));
    let mut engine =
        Engine::open_writing_several_shards(&several.root, &several.cache, &several.wal, config())
            .unwrap();
    engine.start_write_executor(8).expect("the executor starts");
    assert!(engine.writable().is_ok());
    assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);
    let before = served(&engine, &full_coverage_credential());
    drop(engine);

    // A log naming no shard: a layer's registration.
    let one = fixture_sized(N, None);
    let engine = one.open();
    engine.register_layer(flat_declaration("l")).unwrap();
    drop(engine);

    let refused = Engine::open(&several.root, &several.cache, &one.wal, config());
    assert!(
        matches!(refused, Err(EngineError::SeveralShardsLogged { shards: 2 })),
        "{:?}",
        refused.err()
    );
    let engine =
        Engine::open_writing_several_shards(&several.root, &several.cache, &one.wal, config())
            .expect("a log beside several shards opens");
    assert!(
        engine.registered_layer("l").is_some(),
        "the log is replayed"
    );
    assert_eq!(served(&engine, &full_coverage_credential()), before);
}
