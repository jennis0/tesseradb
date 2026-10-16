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
#[test]
fn an_engine_writing_several_shards_admits_writes_and_a_log() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
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

/// One new item holding the `id` of `key` and every term the full credential holds, and its
/// `mosaica_id`.
fn insert(engine: &Engine, key: &str) -> MosaicaId {
    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: key.to_string(),
            body_hash: [7; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec()]),
                position: Some((5.0, 5.0)),
                scalars: keyed(key),
                scoped: Vec::new(),
                omitted: Vec::new(),
            }],
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the insert is accepted");
    receipt.mosaica_ids[0].expect("the row created an item")
}

/// An engine over `built` that writes several shards, its executor running.
fn writing(built: &Fixture) -> Engine {
    let mut engine =
        Engine::open_writing_several_shards(&built.root, &built.cache, &built.wal, config())
            .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

/// An insert into a bundle built as two shards lands in the open one, shard 1, at its high
/// water: its `mosaica_id` and its unique value name it there, and a restart replays it there.
#[test]
fn an_insert_lands_in_the_open_shard_and_replays_there() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    let before = served(&engine, &full_coverage_credential());
    let inserted = insert(&engine, "new");
    let landed = ItemAt::new(ShardId::new(1).unwrap(), EntityId::new(N - SIZE));
    let named = |engine: &Engine| {
        let table = by_identifier_and_id(&[(inserted.raw(), key_id("new"))]);
        let verdicts = engine.name_items(&table).unwrap().verdicts;
        (engine.resolve_targets(&[inserted]).unwrap(), verdicts)
    };
    assert_eq!(
        named(&engine),
        (vec![Some(landed.target())], vec![Ok(landed)])
    );
    assert_eq!(engine.allocator_high_water(), N + 1);
    drop(engine);

    let engine = writing(&several);
    assert_eq!(
        named(&engine),
        (vec![Some(landed.target())], vec![Ok(landed)]),
        "the log replays the row into shard 1"
    );
    assert_eq!(engine.allocator_high_water(), N + 1);
    assert_eq!(served(&engine, &full_coverage_credential()), before);
    let next = insert(&engine, "next");
    assert_eq!(
        engine.resolve_targets(&[next]).unwrap(),
        vec![Some(Target::Item(
            ShardId::new(1).unwrap(),
            EntityId::new(N - SIZE + 1)
        ))],
        "shard 1's high water came back from its own rows"
    );
}

/// A bundle of two shards whose log holds deletions and suppressions in both reopens with each
/// shard's denies: what the same denies leave served in a bundle of one shard.
#[test]
fn a_log_denying_in_both_shards_reopens_with_each_shards_denies() {
    use mosaica_lifecycle::wal::{Wal, WalRecord};
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let log = |path: &std::path::Path, changes: Vec<(Target, ChangeOp)>| {
        let mut wal = Wal::open(path).expect("the log opens");
        wal.append(&WalRecord::ChangeBatch { changes })
            .expect("an append");
        wal.fsync().expect("an fsync");
    };
    let item =
        |shard: u32, entity: u64| Target::Item(ShardId::new(shard).unwrap(), EntityId::new(entity));
    // Shard 0's entity 2 and shard 1's entities 3 and 4 are entities 2, 25 + 3 and 25 + 4 of the
    // bundle of one shard.
    log(
        &several.wal,
        vec![
            (item(0, 2), ChangeOp::Suppress),
            (item(1, 3), ChangeOp::Suppress),
            (item(1, 4), ChangeOp::Delete),
        ],
    );
    log(
        &one.wal,
        vec![
            (item(0, 2), ChangeOp::Suppress),
            (item(0, SIZE + 3), ChangeOp::Suppress),
            (item(0, SIZE + 4), ChangeOp::Delete),
        ],
    );
    let (one, several) = (one.open(), writing(&several));
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(served(&several, &credential), served(&one, &credential));
    }
    assert_eq!(several.overlay_depth(), 3);
    assert_eq!(several.retirable_deletions(), 1);
}

/// The publication `root` serves, as its partition file names it.
fn served_publication(root: &std::path::Path) -> mosaica_store::read::PublishedManifest {
    let n = mosaica_store::open_bundle(root)
        .expect("the bundle opens")
        .partitions[PARTITION]
        .segments_n;
    mosaica_store::read_publication(&root.join(current_prefix(root)), PARTITION, n)
        .expect("the served publication reads")
}

/// Each shard file the publication names, by shard.
fn shard_refs(
    published: &mosaica_store::read::PublishedManifest,
) -> Vec<(ShardId, mosaica_store::manifest::ShardRef)> {
    published
        .refs
        .shards
        .iter()
        .map(|named| (named.id, named.clone()))
        .collect()
}

/// Rows inserted into shard 1 flush into shard 1 alone: the publication writes shard 1's file
/// and keeps shard 0's, the segment is under shard 1's directory, and the items are served as the
/// same inserts into one shard serve them, before and after a restart.
#[test]
fn rows_flush_into_their_own_shard_and_leave_the_others_file() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let before = served_publication(&several.root);
    for key in ["a", "b", "c"] {
        insert(&one_engine, key);
        insert(&engine, key);
    }
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    assert_eq!(engine.buffered_items(), 0, "every row flushed");

    let after = served_publication(&several.root);
    assert!(after.n > before.n);
    let (refs_before, refs_after) = (shard_refs(&before), shard_refs(&after));
    let [zero, first] = [0, 1].map(|id| ShardId::new(id).unwrap());
    assert_eq!(refs_after[0], refs_before[0], "shard 0's file is kept");
    assert_eq!(refs_after[0].0, zero);
    assert_eq!(
        (refs_after[1].0, refs_after[1].1.n),
        (first, after.n),
        "shard 1's file is the publication's own"
    );
    let segments = |published: &mosaica_store::read::PublishedManifest, shard: ShardId| {
        published
            .manifest
            .shard(shard)
            .expect("the shard is listed")
            .segments
            .iter()
            .map(|segment| segment.seg_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        segments(&after, zero),
        segments(&before, zero),
        "shard 0 took no segment"
    );
    let flushed: Vec<String> = segments(&after, first)
        .into_iter()
        .filter(|seg_id| !segments(&before, first).contains(seg_id))
        .collect();
    assert_eq!(flushed.len(), 1);
    let layout = mosaica_store::ShardLayout::new(PARTITION, first);
    assert!(several
        .root
        .join(current_prefix(&several.root))
        .join(layout.segment_rel("s0", &flushed[0]))
        .is_dir());

    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served(&engine, &credential),
            served(&one_engine, &credential)
        );
    }
    let expected = served(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = writing(&several);
    assert_eq!(
        engine.buffered_items(),
        0,
        "the restart replays no flushed row"
    );
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
}

/// Four flushes into shard 1 merge there and nowhere else: shard 1's segments come down, shard
/// 0's file and segments are as they were, and every viewer is served what the same writes into
/// one shard serve, before and after a restart.
#[test]
fn a_merge_in_one_shard_leaves_the_others_segments_and_counts() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let engines = [one.open(), writing(&several)];
    let before = served_publication(&several.root);
    for engine in &engines {
        engine.set_merge_for_test(false);
    }
    for round in 0..4 {
        for engine in &engines {
            insert(engine, &format!("merged-{round}"));
            publish_buffered(engine);
        }
    }
    let flushed = served_publication(&several.root);
    for engine in &engines {
        engine.set_merge_for_test(true);
        engine.request_flush();
        wait_until(
            "the merge publishes",
            std::time::Duration::from_secs(30),
            || engine.write_executor_stats().merges >= 1,
        );
    }
    let after = served_publication(&several.root);
    let [zero, first] = [0, 1].map(|id| ShardId::new(id).unwrap());
    let segments = |published: &mosaica_store::read::PublishedManifest, shard: ShardId| {
        published
            .manifest
            .shard(shard)
            .expect("the shard is listed")
            .segments
            .len()
    };
    assert_eq!(segments(&after, zero), segments(&before, zero));
    assert_eq!(
        shard_refs(&after)[0],
        shard_refs(&before)[0],
        "shard 0's file is kept"
    );
    assert_eq!(segments(&flushed, first), segments(&before, first) + 4);
    assert_eq!(
        segments(&after, first),
        segments(&before, first) + 1,
        "the four flushed segments merged into one"
    );

    let [one_engine, engine] = engines;
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served(&engine, &credential),
            served(&one_engine, &credential)
        );
    }
    let expected = served(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = writing(&several);
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
}

/// A flush and then a merge in shard 1 leave shard 0's row space, and what is cached over it,
/// standing: each publication's refresh carries shard 1's projection forward and not shard 0's,
/// the next viewport is answered from the cache without a projection built, and only shard 1's
/// fragment is rebuilt, at the flush that moved its watermark.
#[test]
fn a_flush_or_merge_in_one_shard_leaves_the_others_projections_and_fragments() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    engine.set_merge_for_test(false);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let wait = std::time::Duration::from_secs(30);
    tile_counts(&engine, &session, 3);
    let builds = engine.full_projection_builds();
    assert_eq!(builds, 2, "one projection in each shard");
    let fragments = engine.fragment_cache_rebuilds();

    let mut refreshes = engine.refreshes();
    let mut published = |engine: &Engine| {
        wait_until("the publication's refresh", wait, || {
            engine.refreshes() > refreshes
        });
        assert_eq!(
            engine.refreshes(),
            refreshes + 1,
            "the refresh carries shard 1's projection alone"
        );
        refreshes += 1;
        tile_counts(engine, &session, 3);
        assert_eq!(
            engine.full_projection_builds(),
            builds,
            "the viewport is answered from the cache in both shards"
        );
    };
    for round in 0..4 {
        insert(&engine, &format!("cached-{round}"));
        publish_buffered(&engine);
        published(&engine);
    }
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        fragments + 4,
        "shard 1's fragment, once a flush"
    );
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge publishes", wait, || {
        engine.write_executor_stats().merges >= 1
    });
    published(&engine);
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        fragments + 4,
        "a merge moves no watermark"
    );
}

/// Shard `shard`'s item `entity`, as a change names it.
fn item_in(shard: u32, entity: u64) -> Target {
    Target::Item(ShardId::new(shard).unwrap(), EntityId::new(entity))
}

/// The entities a deny set names.
fn ids_in(set: &EntitySet) -> Vec<u32> {
    set.entities().expect("the set decodes").iter().collect()
}

/// Each shard's `(suppressed, deleted)` in the side-manifest, by shard.
fn shard_denies(root: &std::path::Path) -> Vec<(Vec<u32>, Vec<u32>)> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| (ids_in(&record.deny), ids_in(&record.tombstones)))
        .collect()
}

/// A deletion and a suppression of an item in each shard, then an unsuppression in shard 1, serve
/// as the same changes to one shard holding the same items serve them; each shard's record holds
/// its own denies; a restart after a rotation opens with them.
#[test]
fn denies_in_both_shards_serve_as_one_shard_and_survive_a_restart() {
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    // Shard 1's entities 3, 4 and 6 are entities 25 + 3, 25 + 4 and 25 + 6 of the bundle of one
    // shard.
    let base = SIZE;
    engine
        .accept_changes(vec![
            (item_in(0, 2), ChangeOp::Suppress),
            (item_in(0, 5), ChangeOp::Delete),
            (item_in(1, 3), ChangeOp::Suppress),
            (item_in(1, 4), ChangeOp::Delete),
            (item_in(1, 6), ChangeOp::Suppress),
        ])
        .unwrap();
    one_engine
        .accept_changes(vec![
            (item_in(0, 2), ChangeOp::Suppress),
            (item_in(0, 5), ChangeOp::Delete),
            (item_in(0, base + 3), ChangeOp::Suppress),
            (item_in(0, base + 4), ChangeOp::Delete),
            (item_in(0, base + 6), ChangeOp::Suppress),
        ])
        .unwrap();
    engine
        .accept_changes(vec![(item_in(1, 6), ChangeOp::Unsuppress)])
        .unwrap();
    one_engine
        .accept_changes(vec![(item_in(0, base + 6), ChangeOp::Unsuppress)])
        .unwrap();
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served(&engine, &credential),
            served(&one_engine, &credential)
        );
    }
    assert_eq!(
        (engine.overlay_depth(), engine.retirable_deletions()),
        (4, 2)
    );

    let wanted = vec![(vec![2], vec![5]), (vec![3], vec![4])];
    tick_until(
        &engine,
        "each shard's deny state's publication",
        std::time::Duration::from_secs(30),
        || shard_denies(&several.root) == wanted,
    );
    // The rotation's snapshot carries each shard's denies, and the changes leave the log.
    reclaim_log(&engine);
    let changes = mosaica_lifecycle::wal::Wal::open(&several.wal)
        .unwrap()
        .records()
        .filter(|r| {
            matches!(
                r,
                Ok((_, mosaica_lifecycle::wal::WalRecord::ChangeBatch { .. }))
            )
        })
        .count();
    assert_eq!(changes, 0);

    let expected = served(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = writing(&several);
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
    assert_eq!(
        (engine.overlay_depth(), engine.retirable_deletions()),
        (4, 2)
    );
}

/// An identifier naming a shard-1 item at a tenancy its number is not at names nothing; the item's
/// own names it in shard 1.
#[test]
fn an_identifier_at_another_tenancy_names_nothing_in_its_shard() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    let at = |tenancy: u16| {
        let high = mosaica_types::ItemHigh::new(
            ShardId::new(1).unwrap(),
            mosaica_types::Tenancy::new(tenancy).unwrap(),
        );
        test_key().forward(high, EntityId::new(3)).unwrap()
    };
    assert_eq!(
        engine.resolve_targets(&[at(0), at(1)]).unwrap(),
        vec![Some(item_in(1, 3)), None]
    );
    let table = mosaica_engine::AddressTable {
        rows: 1,
        mosaica_id: Some(vec![Some(at(1))]),
        columns: Vec::new(),
    };
    assert_eq!(
        engine.name_targets(&table).unwrap().verdicts,
        vec![Err(Refusal::UnknownMosaicaId { row: 0 })]
    );
}

/// A deletion of a shard-1 item resolved before an edit moved the item reaches it where it is now:
/// the item is served to no one and its `mosaica_id` names nothing, before and after a restart.
#[test]
fn a_deletion_resolved_before_an_edit_follows_the_item_in_its_shard() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    let moved = insert(&engine, "moved");
    let table = by_identifier_and_id(&[(moved.raw(), key_id("moved"))]);
    let named = engine.name_items(&table).unwrap();
    let resolved = named.verdicts[0].clone().expect("the row names the item");
    assert_eq!(resolved.shard, ShardId::new(1).unwrap());

    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: "edit".to_string(),
            body_hash: [8; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: Some(moved),
                labels: Some(vec![b"0".to_vec()]),
                position: Some((6.0, 6.0)),
                scalars: keyed("moved"),
                scoped: Vec::new(),
                omitted: Vec::new(),
            }],
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the edit is accepted");
    assert_eq!(
        receipt.mosaica_ids[0],
        Some(moved),
        "an edit keeps its identifier"
    );
    assert_ne!(
        engine.resolve_targets(&[moved]).unwrap(),
        vec![Some(resolved.target())],
        "the edit moved the item"
    );

    engine
        .accept_changes_at(vec![(resolved.target(), ChangeOp::Delete)], named.at)
        .unwrap();
    let gone = |engine: &Engine| {
        assert_eq!(engine.resolve_targets(&[moved]).unwrap(), vec![None]);
        let ids = served(engine, &full_coverage_credential()).ids;
        assert!(!ids.contains(&key_id("moved")), "the item is served");
        assert_eq!(ids.len(), N as usize);
    };
    gone(&engine);
    drop(engine);
    gone(&writing(&several));
}

/// Items the grouped fixture builds: `s0` holds the first [`N`], and the group `g`'s view `g:k`
/// holds those from [`GROUPED_FROM`] to here, so the last ten are in `g:k` alone.
const GROUPED: u64 = 50;
const GROUPED_FROM: u64 = 30;

/// The fixture with a group `g` whose view `k` holds items [`GROUPED_FROM`]..[`GROUPED`],
/// declaring `[shards] size` where `shard_size` is given.
fn grouped_fixture(shard_size: Option<u64>) -> Fixture {
    use arrow::array::Float64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    let (points, every, pairs) = (
        dir.join("points.parquet"),
        dir.join("every.parquet"),
        dir.join("pairs.parquet"),
    );
    write_points_n(&points, N);
    write_points_n(&every, GROUPED);
    write_pairs_n(&pairs, GROUPED);
    let grouped = dir.join("grouped.parquet");
    let ids: Vec<u64> = (GROUPED_FROM..GROUPED).collect();
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(UInt64Array::from(ids.clone())),
            std::sync::Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 41) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 59) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut writer = parquet::arrow::ArrowWriter::try_new(
        std::fs::File::create(&grouped).unwrap(),
        schema,
        None,
    )
    .unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let view = |id: &str, points: &std::path::Path| mosaica_build::ViewArgs {
        visibility: None,
        view_id: id.to_string(),
        projection: mosaica_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: mosaica_build::config::AccessInput::relation(pairs.clone()),
    };
    let e = extent();
    let root = dir.join("bundle");
    let schema = id_schema();
    mosaica_build::build(&mosaica_build::BuildArgs {
        views: vec![view("s0", &points), view("g:k", &grouped)],
        anchor: 0,
        groups: vec![mosaica_build::GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
            visibility: None,
            name: "g".to_string(),
            members_of: None,
            views: vec![mosaica_build::GroupViewDescriptor {
                key: "k".to_string(),
                visibility: None,
                metadata: Default::default(),
            }],
            quantisation: mosaica_build::Quantisation {
                x_min: e.x_min,
                x_max: e.x_max,
                y_min: e.y_min,
                y_max: e.y_max,
            },
            projection: mosaica_spatial::Projection::None,
            metadata: Vec::new(),
            scoped_scalars: Vec::new(),
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(every, &schema),
        out: root.clone(),
        limit: None,
        shard_size,
        strict: false,
        identity_key: test_key(),
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the grouped fixture builds");
    Fixture {
        root,
        cache: dir.join("cache"),
        wal: dir.join("wal.log"),
        _tmp: tmp,
    }
}

/// The item each built `id` of `ids` names, `None` where it names none.
fn items_by_id(engine: &Engine, ids: std::ops::Range<u64>) -> Vec<Option<ItemAt>> {
    let table = mosaica_engine::AddressTable {
        rows: ids.clone().count(),
        mosaica_id: None,
        columns: vec![(
            "id".to_string(),
            ids.map(|id| Some(mosaica_engine::AddressValue::Text(id.to_string())))
                .collect(),
        )],
    };
    engine
        .name_items(&table)
        .unwrap()
        .verdicts
        .into_iter()
        .map(Result::ok)
        .collect()
}

/// Dropping a view deletes, in each shard, the items it leaves in no view: the drop's count is
/// the one shard's holding the same items, those items name nothing, every other item stays, and
/// a restart before any compaction agrees.
#[test]
fn a_view_drop_deletes_the_items_it_leaves_in_no_view_in_every_shard() {
    let one = grouped_fixture(None);
    let several = grouped_fixture(Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let alone: Vec<ItemAt> = items_by_id(&engine, N..GROUPED)
        .into_iter()
        .map(|item| item.expect("each built item holds its id"))
        .collect();
    let shards: std::collections::BTreeSet<ShardId> = alone.iter().map(|item| item.shard).collect();
    assert_eq!(
        shards.len(),
        2,
        "the items in `g:k` alone are in both shards"
    );

    let dropped = engine.drop_view("g".into(), "k".into()).unwrap();
    let one_dropped = one_engine.drop_view("g".into(), "k".into()).unwrap();
    assert_eq!(dropped.deleted, GROUPED - N);
    assert_eq!(dropped.deleted, one_dropped.deleted);

    let check = |engine: &Engine| {
        assert!(items_by_id(engine, N..GROUPED).iter().all(Option::is_none));
        assert!(items_by_id(engine, 0..N).iter().all(Option::is_some));
        assert_eq!(engine.retirable_deletions(), GROUPED - N);
    };
    check(&engine);
    check(&one_engine);
    let expected = served(&one_engine, &full_coverage_credential());
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
    drop(engine);
    let engine = writing(&several);
    check(&engine);
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
}

/// A group-scoped column of a view created at a running service has a base only in the shard a
/// flush wrote one in: the other shard reopens without one, and the view serves after a restart
/// what it served before it.
#[test]
fn a_scoped_column_flushed_in_one_shard_reopens_in_every_shard() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    engine
        .create_view_group(mosaica_lifecycle::wal::ViewGroupDeclaration {
            name: "g".into(),
            title: None,
            projection: "none".into(),
            frame: mosaica_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
            members: None,
            metadata: Vec::new(),
        })
        .unwrap();
    engine
        .declare_attribute(mosaica_engine::AttributeRequest {
            name: "heat".into(),
            title: None,
            ty: "u64".into(),
            vocabulary: None,
            analyser: None,
            index: true,
            render: false,
            scope: mosaica_types::layer::LayerScope::Group("g".into()),
            unique: false,
        })
        .unwrap();
    engine
        .create_view("g".into(), "k".into(), None, Default::default())
        .unwrap();
    engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: "hot".to_string(),
            body_hash: [9; 32],
            view: Some("g:k".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec()]),
                position: Some((5.0, 5.0)),
                scalars: keyed("hot"),
                scoped: vec![mosaica_lifecycle::wal::WalScalar::U64(7)],
                omitted: Vec::new(),
            }],
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the insert is accepted");
    publish_buffered(&engine);
    let named: Vec<Vec<String>> = side_manifest(&several.root)
        .shards
        .records
        .iter()
        .map(|record| {
            record
                .scoped_columns
                .iter()
                .map(|c| c.view.clone())
                .collect()
        })
        .collect();
    assert_eq!(named, vec![Vec::<String>::new(), vec!["g:k".to_string()]]);

    let in_view = |engine: &Engine| {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        let request = ViewportRequest::new("g:k", 0, WHOLE_MAP, N as usize);
        engine.viewport(&session, request).unwrap().points.len()
    };
    assert_eq!(in_view(&engine), 1);
    drop(engine);
    let engine = writing(&several);
    assert_eq!(in_view(&engine), 1);
}

/// A label layer whose content is served only to a viewer seeing every item it was generated from.
fn label_layer(name: &str) -> mosaica_types::layer::LayerDeclaration {
    use mosaica_types::layer::{ContentDeclaration, SuppliedContent, SuppliedRequirement};
    let mut declaration = flat_declaration(name);
    declaration.require_member_visibility = None;
    declaration.content = ContentDeclaration {
        computed: Vec::new(),
        supplied: vec![SuppliedContent {
            name: "label".into(),
            ty: "text".into(),
            require_member_visibility: SuppliedRequirement::All,
        }],
    };
    declaration
}

/// One artifact as a credential is served it: its key, masked count and content, and how many
/// items its `member_of` leaf matches.
type ServedArtifact = (Option<String>, u64, Vec<String>, u64);

/// What each credential is served of the layer `t`.
fn served_layer(engine: &Engine) -> Vec<Vec<ServedArtifact>> {
    use mosaica_engine::filter::{FilterExpr, MemberOfLeaf};
    [full_coverage_credential(), subset_credential()]
        .iter()
        .map(|credential| {
            let session = engine.authorise(credential).unwrap();
            let mut served: Vec<_> = artifacts_of(engine, credential)
                .into_iter()
                .map(|artifact| {
                    let request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize).filter(
                        FilterExpr::MemberOf(MemberOfLeaf {
                            layer: "t".to_string(),
                            artifact: artifact.mosaica_id,
                        }),
                    );
                    let out = engine.viewport(&session, request).unwrap();
                    let matched = out.tiles.iter().map(|tile| tile.matched).sum();
                    (
                        artifact.key,
                        artifact.masked_count,
                        artifact.content,
                        matched,
                    )
                })
                .collect();
            served.sort();
            served
        })
        .collect()
}

/// A publication with members and generating sets in both shards, a growth adding a shard-1 item
/// and a page moving a set into shard 1 serve the counts, containment and `member_of` verdicts one
/// shard holding the same items serves, before a tick, after it and across a restart; the tick
/// writes a member pack into each shard.
#[test]
fn memberships_over_both_shards_serve_as_one_shard_does() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let in_shard_one: Vec<u64> = items_by_id(&engine, 0..N)
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_some_and(|item| item.shard == ShardId::new(1).unwrap()))
        .map(|(id, _)| id as u64)
        .collect();
    // An item of shard 1 the subset credential cannot see, and one it can.
    let unseen = *in_shard_one.iter().find(|id| *id % 3 != 0).unwrap();
    let seen = *in_shard_one.iter().find(|id| *id % 3 == 0).unwrap();
    let write = |engine: &Engine| {
        let items = items_by_id(engine, 0..N);
        let sets =
            |ids: &[u64]| ShardSets::of_items(ids.iter().map(|id| items[*id as usize].unwrap()));
        engine.register_layer(label_layer("t")).unwrap();
        let mut artifact = IncomingArtifact::with_content(
            Some("a".into()),
            Vec::new(),
            vec![
                IncomingContent::new(vec!["near".to_string()], Vec::new()),
                IncomingContent::new(vec!["wide".to_string()], Vec::new()),
            ],
        );
        let members: Vec<u64> = (0..N).filter(|id| id % 2 == 0 && *id != unseen).collect();
        artifact.members = sets(&members);
        artifact.contents[0].generated_from = sets(&[0, seen]);
        artifact.contents[1].generated_from = sets(&[3, 6, seen]);
        let mut other = IncomingArtifact::with_content(
            Some("b".into()),
            Vec::new(),
            vec![IncomingContent::new(vec!["b".to_string()], Vec::new())],
        );
        other.members = sets(&[1, 2]);
        other.contents[0].generated_from = sets(&[1]);
        engine
            .publish_artifacts("t".into(), 0, vec![artifact, other])
            .unwrap();
        let mut joins = IncomingGrowth::from_entities("a".into(), Vec::new());
        joins.joining = sets(&[unseen]);
        let mut page =
            IncomingGrowth::page_of_entities("a".into(), Some(0), Vec::new(), Vec::new());
        page.joining = sets(&[unseen]);
        for growth in [joins, page] {
            engine
                .grow_memberships("t".into(), 0, vec![growth])
                .unwrap();
        }
    };
    write(&one_engine);
    write(&engine);
    let expected = served_layer(&one_engine);
    assert_eq!(
        expected[1]
            .iter()
            .find(|a| a.0.as_deref() == Some("a"))
            .map(|a| a.2.clone()),
        Some(vec!["wide".to_string()]),
        "the page took the subset credential's nearer content away"
    );
    assert_eq!(served_layer(&engine), expected, "before a tick");

    tick(&one_engine);
    tick_until(
        &engine,
        "each shard's member pack",
        std::time::Duration::from_secs(30),
        || {
            let manifest = side_manifest(&several.root);
            manifest.shards.records.len() == 2
                && manifest
                    .shards
                    .records
                    .iter()
                    .all(|record| !record.membership_extents.is_empty())
        },
    );
    assert_eq!(served_layer(&one_engine), expected);
    assert_eq!(served_layer(&engine), expected, "after a tick");

    drop(engine);
    let engine = writing(&several);
    assert_eq!(served_layer(&engine), expected, "across a restart");
}

/// A deletion of a shard-1 item that is a member and a generating entity of an artifact with
/// members in both shards leaves at shard 1's compaction: the content it generated is withdrawn
/// from every shard, shard 0's members stay, and every viewer is served what one shard serves
/// after its own compaction, before and after shard 0's compaction and across a restart.
#[test]
fn a_compaction_of_shard_one_retires_its_members_from_its_slice_alone() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let items = items_by_id(&engine, 0..N);
    let in_shard = |shard: u32| -> Vec<u64> {
        (0..N)
            .filter(|id| items[*id as usize].unwrap().shard == ShardId::new(shard).unwrap())
            .collect()
    };
    let (zero, first) = (in_shard(0), in_shard(1));
    let gone = first[1];
    let write = |engine: &Engine| {
        let items = items_by_id(engine, 0..N);
        let sets =
            |ids: &[u64]| ShardSets::of_items(ids.iter().map(|id| items[*id as usize].unwrap()));
        engine.register_layer(label_layer("t")).unwrap();
        let mut artifact = IncomingArtifact::with_content(
            Some("a".into()),
            Vec::new(),
            vec![
                IncomingContent::new(vec!["near".to_string()], Vec::new()),
                IncomingContent::new(vec!["wide".to_string()], Vec::new()),
            ],
        );
        artifact.members = sets(&[zero[0], zero[1], first[0], gone]);
        artifact.contents[0].generated_from = sets(&[zero[0], gone]);
        artifact.contents[1].generated_from = sets(&[zero[1], first[0]]);
        engine
            .publish_artifacts("t".into(), 0, vec![artifact])
            .unwrap();
        tick(engine);
    };
    write(&one_engine);
    write(&engine);
    let deleted = |engine: &Engine| {
        let target = items_by_id(engine, gone..gone + 1)[0].unwrap().target();
        engine
            .accept_changes(vec![(target, ChangeOp::Delete)])
            .unwrap();
    };
    deleted(&one_engine);
    deleted(&engine);
    fold(&one_engine);
    let expected = served_layer(&one_engine);
    assert_eq!(
        expected[0]
            .iter()
            .find(|a| a.0.as_deref() == Some("a"))
            .map(|a| (a.1, a.2.clone())),
        Some((3, vec!["wide".to_string()])),
        "the near content left with its generating item"
    );

    compact(&engine, Some(ShardId::new(1).unwrap()));
    assert_eq!(
        served_layer(&engine),
        expected,
        "after shard 1's compaction"
    );
    compact(&engine, Some(ShardId::new(0).unwrap()));
    assert_eq!(
        served_layer(&engine),
        expected,
        "after shard 0's compaction"
    );
    drop(engine);
    let engine = writing(&several);
    assert_eq!(served_layer(&engine), expected, "across a restart");
}

/// Whether the layered fixture's layer `t` leaves the item of `id` out of every artifact.
fn unheld(id: u64) -> bool {
    id % 4 == 3
}

/// The layered fixture's build configuration: a flat layer `t` over `s0`, served to a viewer who
/// sees one member.
const LAYERED_TOML: &str = r#"
[sources]
keys    = "keys.parquet"
members = "members.parquet"

[[attribute]]
name   = "id"
type   = "u64"
unique = true
field  = "entity_id"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "t"
views = ["s0"]
source = "keys"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 1 }
hierarchy = { kind = "flat" }

  [layer.members]
  source = "members"
  fields = { id = "entity" }
"#;

/// The fixture with the layer `t` built beside it: artifact `c-k` holds the items whose `id` is in
/// `[10k, 10k + 10)` and not [`unheld`], declaring `[shards] size` where `shard_size` is given.
fn layered_fixture(shard_size: Option<u64>) -> Fixture {
    use arrow::array::{StringArray, UInt32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    let write = |name: &str, schema: Schema, columns: Vec<arrow::array::ArrayRef>| {
        let schema = std::sync::Arc::new(schema);
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let file = std::fs::File::create(dir.join(name)).unwrap();
        let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    };
    let keys: Vec<String> = (0..N / 10).map(|k| format!("c-{k}")).collect();
    let members: Vec<(String, u64)> = (0..N)
        .filter(|id| !unheld(*id))
        .map(|id| (keys[(id / 10) as usize].clone(), id))
        .collect();
    write(
        "keys.parquet",
        Schema::new(vec![Field::new("key", DataType::Utf8, false)]),
        vec![std::sync::Arc::new(StringArray::from(keys.clone()))],
    );
    write(
        "members.parquet",
        Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("rank", DataType::UInt32, true),
            Field::new("entity", DataType::UInt64, false),
        ]),
        vec![
            std::sync::Arc::new(StringArray::from(
                members
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt32Array::from(vec![None::<u32>; members.len()])),
            std::sync::Arc::new(UInt64Array::from(
                members.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            )),
        ],
    );
    let (points, pairs, config_path) = (
        dir.join("points.parquet"),
        dir.join("pairs.parquet"),
        dir.join("config.toml"),
    );
    write_points_n(&points, N);
    write_pairs_n(&pairs, N);
    std::fs::write(&config_path, LAYERED_TOML).unwrap();
    let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
        .expect("the layered fixture's config parses");
    let root = dir.join("bundle");
    mosaica_build::build(&mosaica_build::BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(points, &config.schema),
        out: root.clone(),
        limit: None,
        shard_size,
        strict: false,
        identity_key: test_key(),
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    })
    .expect("the layered fixture builds");
    Fixture {
        root,
        cache: dir.join("cache"),
        wal: dir.join("wal.log"),
        _tmp: tmp,
    }
}

/// One batch of one row naming the item of `id` by its unique value: at `position` where given,
/// joining `key` of the layer `t` where given.
fn name_item(
    engine: &Engine,
    batch: &str,
    id: u64,
    position: Option<(f64, f64)>,
    key: Option<&str>,
) {
    let memberships = key
        .map(|key| mosaica_lifecycle::BatchMembership {
            layer: "t".to_string(),
            level: 0,
            view: None,
            key: key.to_string(),
            rows: vec![0],
        })
        .into_iter()
        .collect();
    engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: batch.to_string(),
            body_hash: [3; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: None,
                position,
                scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(id)],
                scoped: Vec::new(),
                omitted: Vec::new(),
            }],
            artifacts: mosaica_lifecycle::BatchArtifacts {
                memberships,
                edges: Vec::new(),
            },
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the row is accepted");
}

/// The `id`s of the layered fixture's items in shard `shard` of `engine`.
fn ids_in_shard(engine: &Engine, shard: u32) -> Vec<u64> {
    items_by_id(engine, 0..N)
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_some_and(|item| item.shard == ShardId::new(shard).unwrap()))
        .map(|(id, _)| id as u64)
        .collect()
}

/// A membership-only row joining a shard-1 item to an artifact, and an edit moving a shard-1
/// member, serve the counts and `member_of` verdicts one shard holding the same items serves,
/// before a tick, after a tick and a flush, and across a restart; the edited item stays in shard 1.
#[test]
fn a_membership_row_and_an_edit_in_shard_one_serve_as_one_shard_does() {
    let one = layered_fixture(None);
    let several = layered_fixture(Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let in_shard_one = ids_in_shard(&engine, 1);
    let joiner = *in_shard_one.iter().find(|id| unheld(**id)).unwrap();
    let edited = *in_shard_one.iter().find(|id| !unheld(**id)).unwrap();
    let before = items_by_id(&engine, edited..edited + 1)[0].unwrap();
    for engine in [&one_engine, &engine] {
        name_item(engine, "join", joiner, None, Some("c-0"));
        name_item(engine, "edit", edited, Some((7.0, 7.0)), None);
    }
    let after = items_by_id(&engine, edited..edited + 1)[0].unwrap();
    assert_eq!(
        after.shard, before.shard,
        "an edit stays in its item's shard"
    );
    assert_ne!(after.entity, before.entity, "the edit took a new entity");
    assert_eq!(
        served_layer(&engine),
        served_layer(&one_engine),
        "before a tick"
    );

    tick(&one_engine);
    tick(&engine);
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    let expected = served_layer(&one_engine);
    assert!(
        expected[0]
            .iter()
            .any(|a| a.0.as_deref() == Some("c-0") && a.1 == 9),
        "the joined item counts for its artifact: {expected:?}"
    );
    assert_eq!(served_layer(&engine), expected, "after a tick and a flush");

    drop(engine);
    let engine = writing(&several);
    assert_eq!(served_layer(&engine), expected, "across a restart");
}

/// Each shard's derived files of the layer `t`'s space, as the served side-manifest names them.
fn derived_of(root: &std::path::Path) -> Vec<Vec<mosaica_store::manifest::DerivedExtent>> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| record.derived_extents.clone())
        .collect()
}

/// Each shard's slice version of level 0, and the level's records version, as the served
/// side-manifest records them.
fn versions_of(root: &std::path::Path) -> (Vec<u64>, u64) {
    let manifest = side_manifest(root);
    let slices = manifest
        .shards
        .records
        .iter()
        .map(|record| {
            record
                .slice_versions
                .iter()
                .find(|version| version.level == 0)
                .map_or(0, |version| version.version)
        })
        .collect();
    let records = manifest
        .layer_spaces
        .iter()
        .find_map(|space| space.level_version(0))
        .unwrap_or(0);
    (slices, records)
}

/// Whether a derived file is one a growth leaves standing: an enumerated level's column and the
/// member file beside it, which an open completes.
fn completed_at_open(extent: &mosaica_store::manifest::DerivedExtent) -> bool {
    use mosaica_store::manifest::DerivedForm;
    matches!(
        extent.form,
        DerivedForm::RowColumn { .. } | DerivedForm::RowMembers
    )
}

/// A growth in shard 0 moves shard 0's slice version and no other: the next manifest names shard
/// 1's derived files as they were and shard 1's figures are still held, while shard 0's files
/// written at one version leave. A new artifact moves the records version and every shard's, and
/// every shard's files written at one version leave.
#[test]
fn a_growth_in_one_shard_leaves_the_others_files_and_figures() {
    use mosaica_lifecycle::ShardSets;
    let several = layered_fixture(Some(SIZE));
    let engine = writing(&several);
    let full = full_coverage_credential();
    let files = derived_of(&several.root);
    assert!(
        files
            .iter()
            .all(|shard| shard.iter().any(|extent| !completed_at_open(extent))),
        "the build writes files at one version in each shard: {files:?}"
    );
    let (slices, records) = versions_of(&several.root);

    let figures = |engine: &Engine| {
        let stats = engine.figures_stats();
        (stats.misses, stats.hits)
    };
    let opened = figures(&engine);
    let served = artifacts_of(&engine, &full);
    let cold = figures(&engine).0 - opened.0;
    assert_eq!(cold, 2, "the first request takes each shard's figures");

    let joiner = *ids_in_shard(&engine, 0)
        .iter()
        .find(|id| unheld(**id))
        .unwrap();
    let mut growth = IncomingGrowth::from_entities("c-3".into(), Vec::new());
    growth.joining = ShardSets::of_items(items_by_id(&engine, joiner..joiner + 1)[0]);
    engine
        .grow_memberships("t".into(), 0, vec![growth])
        .unwrap();
    tick_until(
        &engine,
        "the growth's publication",
        std::time::Duration::from_secs(30),
        || versions_of(&several.root).0[0] > slices[0],
    );
    let (grown, grown_records) = versions_of(&several.root);
    assert_eq!(grown[1], slices[1], "shard 1's slice did not move");
    assert_eq!(grown_records, records, "a growth changes no record");
    let after_growth = derived_of(&several.root);
    assert_eq!(after_growth[1], files[1], "shard 1's derived files stand");
    assert!(
        after_growth[0].iter().all(completed_at_open),
        "shard 0's files at the old version leave: {:?}",
        after_growth[0]
    );
    let before = figures(&engine);
    let regrown = artifacts_of(&engine, &full);
    let count = |served: &[mosaica_engine::ArtifactOut], key: &str| {
        served
            .iter()
            .find(|artifact| artifact.key.as_deref() == Some(key))
            .map(|artifact| artifact.masked_count)
    };
    assert_eq!(
        count(&regrown, "c-3"),
        count(&served, "c-3").map(|held| held + 1),
        "the joined item counts"
    );
    let now = figures(&engine);
    assert!(
        now.0 - before.0 < cold && now.1 > before.1,
        "shard 1's figures are held: {before:?} then {now:?}"
    );

    let first = |shard: u32| {
        let id = ids_in_shard(&engine, shard)[0];
        items_by_id(&engine, id..id + 1)[0].unwrap()
    };
    let mut fresh = IncomingArtifact::from_entities(Some("c-new".into()), Vec::new());
    fresh.members = ShardSets::of_items([first(0), first(1)]);
    engine
        .publish_artifacts("t".into(), 0, vec![fresh])
        .unwrap();
    tick_until(
        &engine,
        "the publication's manifest",
        std::time::Duration::from_secs(30),
        || versions_of(&several.root).1 > records,
    );
    let (published, _) = versions_of(&several.root);
    assert!(
        published[0] > grown[0] && published[1] > grown[1],
        "a new artifact moves every shard's slice: {published:?} after {grown:?}"
    );
    assert!(
        derived_of(&several.root)
            .iter()
            .all(|shard| shard.iter().all(completed_at_open)),
        "every shard's files at the old version leave"
    );
    assert_eq!(
        count(&artifacts_of(&engine, &full), "c-new"),
        Some(2),
        "both shards serve the new artifact's members"
    );
}

/// Dropping a layer at two shards removes every shard's member packs and derived files of its
/// space, and its space's record, from the next manifest, and a restart agrees.
#[test]
fn a_layer_drop_removes_every_shards_files_from_the_next_manifest() {
    let several = layered_fixture(Some(SIZE));
    let engine = writing(&several);
    let space = side_manifest(&several.root).layer_spaces[0].space;
    let holds = |root: &std::path::Path| {
        let manifest = side_manifest(root);
        let shards: Vec<bool> = manifest
            .shards
            .records
            .iter()
            .map(|record| {
                record.membership_extents.iter().any(|e| e.space == space)
                    || record.derived_extents.iter().any(|e| e.space == space)
            })
            .collect();
        let spaces = manifest.layer_spaces.iter().any(|s| s.space == space);
        (shards, spaces)
    };
    assert_eq!(holds(&several.root), (vec![true, true], true));
    engine.drop_layer("t".into()).unwrap();
    tick_until(
        &engine,
        "the drop's manifest",
        std::time::Duration::from_secs(30),
        || holds(&several.root) == (vec![false, false], false),
    );
    drop(engine);
    let engine = writing(&several);
    assert!(engine.registered_layer("t").is_none());
    tick(&engine);
    assert_eq!(holds(&several.root), (vec![false, false], false));
}

/// Forms of a level held in both shards take a tick's publication, growth and page, and read one
/// version of the level's records after it: counts, content and `member_of` verdicts are one
/// shard's holding the same items.
#[test]
fn forms_held_in_both_shards_take_a_ticks_deltas_as_one_level() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    let one = fixture_sized(N, None);
    let several = fixture_sized(N, Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let (zero, one_id) = (ids_in_shard(&engine, 0)[0], ids_in_shard(&engine, 1)[0]);
    let sets = |engine: &Engine, ids: &[u64]| {
        let items = items_by_id(engine, 0..N);
        ShardSets::of_items(ids.iter().map(|id| items[*id as usize].unwrap()))
    };
    let labelled = |engine: &Engine, key: &str, members: &[u64], from: &[u64]| {
        let mut artifact = IncomingArtifact::with_content(
            Some(key.into()),
            Vec::new(),
            vec![IncomingContent::new(vec![key.to_string()], Vec::new())],
        );
        artifact.members = sets(engine, members);
        artifact.contents[0].generated_from = sets(engine, from);
        artifact
    };
    for engine in [&one_engine, &engine] {
        engine.register_layer(label_layer("t")).unwrap();
        engine
            .publish_artifacts(
                "t".into(),
                0,
                vec![labelled(engine, "a", &[zero, one_id], &[zero])],
            )
            .unwrap();
        tick(engine);
    }
    assert_eq!(
        served_layer(&engine),
        served_layer(&one_engine),
        "forms held"
    );

    for engine in [&one_engine, &engine] {
        engine
            .publish_artifacts(
                "t".into(),
                0,
                vec![labelled(engine, "b", &[one_id], &[one_id])],
            )
            .unwrap();
        let mut page =
            IncomingGrowth::page_of_entities("a".into(), Some(0), Vec::new(), Vec::new());
        page.joining = sets(engine, &[one_id]);
        engine.grow_memberships("t".into(), 0, vec![page]).unwrap();
        tick(engine);
    }
    let expected = served_layer(&one_engine);
    assert!(
        expected[0].iter().all(|artifact| !artifact.2.is_empty()),
        "the full credential is served every content: {expected:?}"
    );
    assert_eq!(served_layer(&engine), expected, "after the tick");
}

/// How many items each credential sees in `view` over the whole map.
fn visible_in(engine: &Engine, view: &str) -> Vec<u64> {
    [full_coverage_credential(), subset_credential()]
        .iter()
        .map(|credential| {
            let session = engine.authorise(credential).unwrap();
            engine
                .viewport(
                    &session,
                    ViewportRequest::new(view, 0, WHOLE_MAP, N as usize),
                )
                .unwrap()
                .tiles
                .iter()
                .map(|tile| tile.visible)
                .sum()
        })
        .collect()
}

/// A row joining an item of the sealed shard 0 into a view created while the service runs, a
/// membership-only row joining another to an artifact, and an edit of a third, which takes shard
/// 0's high water past the size, serve as the same writes to one shard serve them, before a tick,
/// after a tick and a flush, and across a restart; each item stays in shard 0.
#[test]
fn a_join_a_membership_row_and_an_edit_in_the_sealed_shard_serve_as_one_shard_does() {
    let one = layered_fixture(None);
    let several = layered_fixture(Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let in_shard_zero = ids_in_shard(&engine, 0);
    let joiner = *in_shard_zero.iter().find(|id| unheld(**id)).unwrap();
    let edited = *in_shard_zero.iter().find(|id| !unheld(**id)).unwrap();
    let viewed = *in_shard_zero
        .iter()
        .find(|id| ![joiner, edited].contains(id))
        .unwrap();
    let frame = mosaica_engine::DeclaredFrame {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    };
    for engine in [&one_engine, &engine] {
        engine
            .create_plain_view(mosaica_engine::PlainViewDeclaration {
                name: "p".into(),
                title: None,
                projection: "none".into(),
                frame,
                visibility: None,
                point_default: None,
            })
            .unwrap();
        engine
            .ingest(mosaica_engine::IngestRequest {
                batch_id: "view".to_string(),
                body_hash: [5; 32],
                view: Some("p".to_string()),
                rows: vec![mosaica_lifecycle::IngestRow {
                    mosaica_id: None,
                    labels: None,
                    position: Some((3.0, 3.0)),
                    scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(viewed)],
                    scoped: Vec::new(),
                    omitted: Vec::new(),
                }],
                artifacts: Default::default(),
                strict: true,
                mosaica_id_column: false,
            })
            .expect("the join is accepted");
        name_item(engine, "join", joiner, None, Some("c-0"));
        name_item(engine, "edit", edited, Some((7.0, 7.0)), None);
    }
    let shard_zero = ShardId::ZERO;
    let held = |engine: &Engine| {
        items_by_id(engine, 0..N)
            .into_iter()
            .map(|item| item.unwrap().shard)
            .collect::<Vec<_>>()
    };
    let before = held(&engine);
    for id in [joiner, edited, viewed] {
        assert_eq!(
            before[id as usize], shard_zero,
            "item {id} stays in shard 0"
        );
    }
    assert_eq!(
        items_by_id(&engine, edited..edited + 1)[0].unwrap().entity,
        EntityId::new(SIZE),
        "the edit takes shard 0's high water, past the size"
    );
    let compare = |engine: &Engine, one_engine: &Engine, when: &str| {
        assert_eq!(served_layer(engine), served_layer(one_engine), "{when}");
        assert_eq!(
            visible_in(engine, "p"),
            visible_in(one_engine, "p"),
            "{when}"
        );
        for credential in [full_coverage_credential(), subset_credential()] {
            assert_eq!(
                served(engine, &credential),
                served(one_engine, &credential),
                "{when}"
            );
        }
    };
    compare(&engine, &one_engine, "before a tick");

    tick(&one_engine);
    tick(&engine);
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    compare(&engine, &one_engine, "after a tick and a flush");
    let joined = vec![1, u64::from(viewed % 3 == 0)];
    assert_eq!(visible_in(&engine, "p"), joined, "the joined row is served");

    drop(engine);
    let engine = writing(&several);
    compare(&engine, &one_engine, "across a restart");
    assert_eq!(visible_in(&engine, "p"), joined);
    assert_eq!(held(&engine), before);
}

/// New items past the open shard's size, some joining an artifact, open shard 2 and serve as the
/// same items in one shard serve: before a tick, across a restart that replays the opening, after
/// a tick and a flush, and across a restart that reads it from a manifest. The opened shard holds
/// an empty member pack beside each record pack.
#[test]
fn items_in_a_shard_opened_while_the_service_runs_serve_as_one_shard_does() {
    let one = layered_fixture(None);
    let several = layered_fixture(Some(SIZE));
    let (one_engine, engine) = (one.open(), writing(&several));
    let served_n = mosaica_store::open_bundle(&several.root)
        .unwrap()
        .partitions[PARTITION]
        .segments_n;
    // Ten fill shard 1 to its size and the last two open shard 2.
    let insert_twelve = |engine: &Engine| {
        let rows = (0..12)
            .map(|at| mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec(), b"1".to_vec()]),
                position: Some((4.0 + at as f64, 4.0)),
                scalars: keyed(&format!("new-{at}")),
                scoped: Vec::new(),
                omitted: Vec::new(),
            })
            .collect();
        engine
            .ingest(mosaica_engine::IngestRequest {
                batch_id: "past-the-size".to_string(),
                body_hash: [6; 32],
                view: Some("s0".to_string()),
                rows,
                artifacts: mosaica_lifecycle::BatchArtifacts {
                    memberships: vec![mosaica_lifecycle::BatchMembership {
                        layer: "t".to_string(),
                        level: 0,
                        view: None,
                        key: "c-1".to_string(),
                        rows: vec![9, 10, 11],
                    }],
                    edges: Vec::new(),
                },
                strict: true,
                mosaica_id_column: false,
            })
            .expect("the batch is accepted")
    };
    insert_twelve(&one_engine);
    let receipt = insert_twelve(&engine);
    let shards: Vec<u32> = engine
        .resolve_targets(
            &receipt
                .mosaica_ids
                .iter()
                .map(|id| id.unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .into_iter()
        .map(|target| match target {
            Some(Target::Item(shard, _)) => shard.raw(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(shards, [vec![1; 10], vec![2; 2]].concat());
    let compare = |engine: &Engine, when: &str| {
        assert_eq!(served_layer(engine), served_layer(&one_engine), "{when}");
        for credential in [full_coverage_credential(), subset_credential()] {
            assert_eq!(
                served(engine, &credential),
                served(&one_engine, &credential),
                "{when}"
            );
        }
    };
    compare(&engine, "before a tick");
    drop(engine);

    let partition = partition_dir(&several.root, &current_prefix(&several.root));
    for entry in std::fs::read_dir(&partition).unwrap() {
        let name = entry.unwrap().file_name();
        let later = name
            .to_str()
            .and_then(|name| name.strip_prefix("SEGMENTS-"))
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|m| m.parse::<u64>().ok())
            .is_some_and(|m| m > served_n);
        if later {
            std::fs::remove_file(partition.join(name)).unwrap();
        }
    }
    let engine = writing(&several);
    compare(&engine, "across a restart that replays the opening");

    tick(&one_engine);
    tick(&engine);
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    compare(&engine, "after a tick and a flush");
    let manifest = side_manifest(&several.root);
    let opened = manifest.shard(ShardId::new(2).unwrap()).unwrap();
    assert_eq!(opened.high_water, 2);
    let first = manifest.shard(ShardId::ZERO).unwrap();
    for record_pack in manifest
        .layer_spaces
        .iter()
        .flat_map(|space| space.record_packs.iter().map(move |r| (space.space, r)))
    {
        assert!(
            opened
                .membership_extents
                .iter()
                .any(|members| members.pairs(record_pack.0, record_pack.1)),
            "shard 2 pairs every record pack"
        );
        assert!(first
            .membership_extents
            .iter()
            .any(|members| members.pairs(record_pack.0, record_pack.1)));
    }

    drop(engine);
    let engine = writing(&several);
    compare(&engine, "across a restart that reads the manifest");
}
