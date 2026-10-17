//! A bundle a build wrote past its `[shards] size`, opened by a running service: it answers every
//! read as the same items built as one shard answer it, names each item in its own shard, and
//! answers every write as a bundle of one shard does.

mod common;

use common::*;
use mosaica_engine::{AcceptError, Engine, ExecutorPosture, RecordsOrder, ViewportRequest};
use mosaica_lifecycle::resolve::Refusal;
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::{DeclaredValue, IncomingArtifact, IncomingGrowth, VocabularyRequest};
use mosaica_store::manifest::EntitySet;
use mosaica_types::{EntityId, ItemAt, MosaicaId, ShardId, Target};

#[test]
fn a_bundle_of_several_shards_answers_every_read_as_one_shard_does() {
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    assert_eq!(
        shards_of(&several.root).1.len(),
        2,
        "the build wrote two shards"
    );
    let (one, several) = (one.open(), several.open());

    for credential in [full_coverage_credential(), subset_credential()] {
        let answered = served_to(&several, &credential);
        assert_eq!(answered, served_to(&one, &credential));
        let ids = answered.ids;
        let want: Vec<u64> = (0..TWO_SHARD_N)
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
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
        let base = TWO_SHARD_SIZE as u32;
        manifest.shards.records[0].deny = entities(&[2, base + 3]);
        manifest.shards.records[0].tombstones = entities(&[base + 4]);
    });
    let (one, several) = (one.open(), several.open());

    assert_serves_as(&several, &one, "with each shard's denies");
    let full = several.authorise(&full_coverage_credential()).unwrap();
    let after = served_to(&several, &full_coverage_credential()).ids;
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open());
    assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);

    assert_eq!(engine.allocator_high_water(), TWO_SHARD_N);
    assert_eq!(
        engine.allocator_high_water(),
        one_engine.allocator_high_water()
    );
    assert_eq!(engine.shard_size(), TWO_SHARD_SIZE);
    let (status, one_status) = (engine.generation_status(), one_engine.generation_status());
    assert_eq!(status.live_rows, one_status.live_rows);
    assert_eq!(status.overlay_depth, one_status.overlay_depth);
    assert_eq!(status.retirable_deletions, one_status.retirable_deletions);
    let watermarks = |s: &mosaica_engine::GenerationStatus| -> Vec<u64> {
        s.partitions.iter().map(|p| p.watermark).collect()
    };
    assert_eq!(watermarks(&status), vec![TWO_SHARD_N]);
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

/// Every write kind, through the engine, naming items of shard 0, which holds the same items
/// whether the fixture is built as one shard or two.
fn writes(engine: &Engine) -> Vec<(&'static str, Result<(), AcceptError>)> {
    let item = item_in(0, 1);
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
        ("shards", engine.declare_shards(TWO_SHARD_N + 1).map(drop)),
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
                            croaring::Bitmap::of(&[2]),
                        ),
                        leaving: Default::default(),
                        rank: None,
                        parts: Default::default(),
                    }],
                )
                .map(drop),
        ),
        ("layer drop", engine.drop_layer("l".into())),
    ]
}

/// What an answer was: `ok`, or the names of the error's variants, outermost first.
fn kind(answered: &Result<(), AcceptError>) -> String {
    let Err(e) = answered else {
        return "ok".into();
    };
    let debug = format!("{e:?}");
    let named = debug.split('"').next().unwrap_or_default();
    named
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| word.starts_with(|c: char| c.is_ascii_uppercase()))
        .collect::<Vec<_>>()
        .join("::")
}

/// Every write kind answers over a bundle of two shards as over the same items built as one, the
/// executor runs over both, and a restart replays the log the writes left to the same answers.
#[test]
fn every_write_answers_over_two_shards_as_over_one_and_a_restart_replays_it() {
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let mut answered = Vec::new();
    for built in [&one, &several] {
        let engine = built.open();
        assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);
        let kinds: Vec<(&str, String)> = writes(&engine)
            .iter()
            .map(|(route, answer)| (*route, kind(answer)))
            .collect();
        let served = served_to(&engine, &full_coverage_credential());
        drop(engine);
        let logged = mosaica_lifecycle::wal::Wal::open(&built.wal)
            .unwrap()
            .records()
            .count();
        assert!(logged > 0, "the writes are in the log");
        let engine = built.open();
        assert_eq!(
            served_to(&engine, &full_coverage_credential()),
            served,
            "the restart serves what the writes left"
        );
        answered.push((kinds, served));
    }
    let [(one_kinds, one_served), (several_kinds, several_served)] = &answered[..] else {
        unreachable!()
    };
    assert_eq!(several_kinds, one_kinds);
    assert!(
        one_kinds.iter().any(|(_, kind)| kind == "ok"),
        "{one_kinds:?}"
    );
    assert_eq!(several_served, one_served);
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
    let built = two_shards();
    let engine = built.open();
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let read = read_items(&engine, &session, &Items::new(RecordsOrder::Stored)).rows_with_ids;
    assert_eq!(read.len(), TWO_SHARD_N as usize);

    let named = engine.name_items(&by_identifier_and_id(&read)).unwrap();
    let items: Vec<ItemAt> = named
        .verdicts
        .iter()
        .map(|verdict| verdict.clone().expect("each row names its item"))
        .collect();
    let distinct: std::collections::BTreeSet<ItemAt> = items.iter().copied().collect();
    assert_eq!(distinct.len(), TWO_SHARD_N as usize, "no two items are one");
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
        (
            TWO_SHARD_SIZE as usize,
            (TWO_SHARD_N - TWO_SHARD_SIZE) as usize
        )
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

/// An insert into a bundle built as two shards lands in the open one, shard 1, at its high
/// water: its `mosaica_id` and its unique value name it there, and a restart replays it there.
#[test]
fn an_insert_lands_in_the_open_shard_and_replays_there() {
    let several = two_shards();
    let engine = several.open_writing();
    let before = served_to(&engine, &full_coverage_credential());
    let inserted = insert_keyed(&engine, "new", &[true])[0];
    let landed = ItemAt::new(shard(1), EntityId::new(TWO_SHARD_N - TWO_SHARD_SIZE));
    let named = |engine: &Engine| {
        let table = by_identifier_and_id(&[(inserted.raw(), key_id("new-0"))]);
        let verdicts = engine.name_items(&table).unwrap().verdicts;
        (engine.resolve_targets(&[inserted]).unwrap(), verdicts)
    };
    assert_eq!(
        named(&engine),
        (vec![Some(landed.target())], vec![Ok(landed)])
    );
    assert_eq!(engine.allocator_high_water(), TWO_SHARD_N + 1);
    drop(engine);

    let engine = several.open_writing();
    assert_eq!(
        named(&engine),
        (vec![Some(landed.target())], vec![Ok(landed)]),
        "the log replays the row into shard 1"
    );
    assert_eq!(engine.allocator_high_water(), TWO_SHARD_N + 1);
    assert_eq!(served_to(&engine, &full_coverage_credential()), before);
    let next = insert_keyed(&engine, "next", &[true])[0];
    assert_eq!(
        engine.resolve_targets(&[next]).unwrap(),
        vec![Some(item_in(1, TWO_SHARD_N - TWO_SHARD_SIZE + 1))],
        "shard 1's high water came back from its own rows"
    );
}

/// A bundle of two shards whose log holds deletions and suppressions in both reopens with each
/// shard's denies: what the same denies leave served in a bundle of one shard.
#[test]
fn a_log_denying_in_both_shards_reopens_with_each_shards_denies() {
    use mosaica_lifecycle::wal::{Wal, WalRecord};
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let log = |path: &std::path::Path, changes: Vec<(Target, ChangeOp)>| {
        let mut wal = Wal::open(path).expect("the log opens");
        wal.append(&WalRecord::ChangeBatch { changes })
            .expect("an append");
        wal.fsync().expect("an fsync");
    };
    let changes = vec![
        (item_in(0, 2), ChangeOp::Suppress),
        (item_in(1, 3), ChangeOp::Suppress),
        (item_in(1, 4), ChangeOp::Delete),
    ];
    let one_shard = changes
        .iter()
        .map(|(t, op)| (as_one_shard(*t), *op))
        .collect();
    log(&several.wal, changes);
    log(&one.wal, one_shard);
    let (one, several) = (one.open(), several.open_writing());
    assert_serves_as(&several, &one, "with the log's denies");
    assert_eq!(several.overlay_depth(), 3);
    assert_eq!(several.retirable_deletions(), 1);
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open_writing());
    let before = served_publication(&several.root);
    for key in ["a", "b", "c"] {
        insert_keyed(&one_engine, key, &[true]);
        insert_keyed(&engine, key, &[true]);
    }
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    assert_eq!(engine.buffered_items(), 0, "every row flushed");

    let after = served_publication(&several.root);
    assert!(after.n > before.n);
    let (refs_before, refs_after) = (shard_refs(&before), shard_refs(&after));
    let [zero, first] = [0, 1].map(shard);
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

    assert_serves_as(&engine, &one_engine, "after the flush");
    let expected = served_to(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(
        engine.buffered_items(),
        0,
        "the restart replays no flushed row"
    );
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
}

/// Four flushes into shard 1 merge there and nowhere else: shard 1's segments come down, shard
/// 0's file and segments are as they were, and every viewer is served what the same writes into
/// one shard serve, before and after a restart.
#[test]
fn a_merge_in_one_shard_leaves_the_others_segments_and_counts() {
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let engines = [one.open(), several.open_writing()];
    let before = served_publication(&several.root);
    for engine in &engines {
        engine.set_merge_for_test(false);
    }
    for round in 0..4 {
        for engine in &engines {
            insert_keyed(engine, &format!("merged-{round}"), &[true]);
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
    let [zero, first] = [0, 1].map(shard);
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
    assert_serves_as(&engine, &one_engine, "after the merge");
    let expected = served_to(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
}

/// A flush and then a merge in shard 1 leave shard 0's row space, and what is cached over it,
/// standing: each publication's refresh carries shard 1's projection forward and not shard 0's,
/// the next viewport is answered from the cache without a projection built, and only shard 1's
/// fragment is rebuilt, at the flush that moved its watermark.
#[test]
fn a_flush_or_merge_in_one_shard_leaves_the_others_projections_and_fragments() {
    let several = two_shards();
    let engine = several.open_writing();
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
        refreshes = engine.refreshes();
        tile_counts(engine, &session, 3);
        assert_eq!(
            engine.full_projection_builds(),
            builds,
            "the viewport is answered from the cache in both shards"
        );
    };
    for round in 0..4 {
        insert_keyed(&engine, &format!("cached-{round}"), &[true]);
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

/// Each shard's `(suppressed, deleted)` in the side-manifest, by shard.
fn shard_denies(root: &std::path::Path) -> Vec<(Vec<u32>, Vec<u32>)> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| (entity_ids(&record.deny), entity_ids(&record.tombstones)))
        .collect()
}

/// A deletion and a suppression of an item in each shard, then an unsuppression in shard 1, serve
/// as the same changes to one shard holding the same items serve them; each shard's record holds
/// its own denies; a restart after a rotation opens with them.
#[test]
fn denies_in_both_shards_serve_as_one_shard_and_survive_a_restart() {
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open_writing());
    let accept = |changes: Vec<(Target, ChangeOp)>| {
        let one_shard = changes
            .iter()
            .map(|(t, op)| (as_one_shard(*t), *op))
            .collect();
        engine.accept_changes(changes).unwrap();
        one_engine.accept_changes(one_shard).unwrap();
    };
    accept(vec![
        (item_in(0, 2), ChangeOp::Suppress),
        (item_in(0, 5), ChangeOp::Delete),
        (item_in(1, 3), ChangeOp::Suppress),
        (item_in(1, 4), ChangeOp::Delete),
        (item_in(1, 6), ChangeOp::Suppress),
    ]);
    accept(vec![(item_in(1, 6), ChangeOp::Unsuppress)]);
    assert_serves_as(&engine, &one_engine, "with the denies");
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

    let expected = served_to(&one_engine, &full_coverage_credential());
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
    assert_eq!(
        (engine.overlay_depth(), engine.retirable_deletions()),
        (4, 2)
    );
}

/// An identifier naming a shard-1 item at a tenancy its number is not at names nothing; the item's
/// own names it in shard 1.
#[test]
fn an_identifier_at_another_tenancy_names_nothing_in_its_shard() {
    let several = two_shards();
    let engine = several.open_writing();
    let at = |tenancy: u16| {
        let high =
            mosaica_types::ItemHigh::new(shard(1), mosaica_types::Tenancy::new(tenancy).unwrap());
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
    let several = two_shards();
    let engine = several.open_writing();
    let moved = insert_keyed(&engine, "moved", &[true])[0];
    let table = by_identifier_and_id(&[(moved.raw(), key_id("moved-0"))]);
    let named = engine.name_items(&table).unwrap();
    let resolved = named.verdicts[0].clone().expect("the row names the item");
    assert_eq!(resolved.shard, shard(1));

    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: "edit".to_string(),
            body_hash: [8; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: Some(moved),
                labels: Some(vec![b"0".to_vec()]),
                position: Some((6.0, 6.0)),
                scalars: keyed("moved-0"),
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
        let ids = served_to(engine, &full_coverage_credential()).ids;
        assert!(!ids.contains(&key_id("moved-0")), "the item is served");
        assert_eq!(ids.len(), TWO_SHARD_N as usize);
    };
    gone(&engine);
    drop(engine);
    gone(&several.open_writing());
}

/// Items the grouped fixture builds: `s0` holds the first [`TWO_SHARD_N`], and the group `g`'s
/// view `g:k` holds those from [`GROUPED_FROM`] to here, so the last ten are in `g:k` alone.
const GROUPED: u64 = 50;
const GROUPED_FROM: u64 = 30;

/// The fixture with a group `g` whose view `k` holds items [`GROUPED_FROM`]..[`GROUPED`],
/// declaring `[shards] size` where `shard_size` is given.
fn grouped_fixture(shard_size: Option<u64>) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    let (points, every, pairs) = (
        dir.join("points.parquet"),
        dir.join("every.parquet"),
        dir.join("pairs.parquet"),
    );
    write_points_n(&points, TWO_SHARD_N);
    write_points_n(&every, GROUPED);
    write_pairs_n(&pairs, GROUPED);
    let grouped = dir.join("grouped.parquet");
    write_points_of(&grouped, GROUPED_FROM..GROUPED);

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

/// Dropping a view deletes, in each shard, the items it leaves in no view: the drop's count is
/// the one shard's holding the same items, those items name nothing, every other item stays, and
/// a restart before any compaction agrees.
#[test]
fn a_view_drop_deletes_the_items_it_leaves_in_no_view_in_every_shard() {
    let one = grouped_fixture(None);
    let several = grouped_fixture(Some(TWO_SHARD_SIZE));
    let (one_engine, engine) = (one.open(), several.open_writing());
    let alone: Vec<ItemAt> = items_by_id(&engine, TWO_SHARD_N..GROUPED)
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
    assert_eq!(dropped.deleted, GROUPED - TWO_SHARD_N);
    assert_eq!(dropped.deleted, one_dropped.deleted);

    let check = |engine: &Engine| {
        assert!(items_by_id(engine, TWO_SHARD_N..GROUPED)
            .iter()
            .all(Option::is_none));
        assert!(items_by_id(engine, 0..TWO_SHARD_N)
            .iter()
            .all(Option::is_some));
        assert_eq!(engine.retirable_deletions(), GROUPED - TWO_SHARD_N);
    };
    check(&engine);
    check(&one_engine);
    let expected = served_to(&one_engine, &full_coverage_credential());
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
    drop(engine);
    let engine = several.open_writing();
    check(&engine);
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
}

/// A group-scoped column of a view created at a running service has a base only in the shard a
/// flush wrote one in: the other shard holds the column empty, so a filter on it is admitted and
/// answered in both shards. Each shard's compaction gives it a base, and the family leaves for
/// `MANIFEST.json` at the second; the view serves the same at every step and after each restart.
#[test]
fn a_scoped_column_flushed_in_one_shard_reopens_in_every_shard() {
    let several = two_shards();
    let engine = several.open_writing();
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
        let request = ViewportRequest::new("g:k", 0, WHOLE_MAP, TWO_SHARD_N as usize);
        let all = engine.viewport(&session, request.clone()).unwrap().points.len();
        let hot = request.filter(mosaica_engine::filter::FilterExpr::Leaf {
            column: "heat@g:k".to_string(),
            operand: mosaica_engine::filter::FilterOperand::Range {
                lo: Some(mosaica_engine::filter::Endpoint {
                    value: mosaica_engine::filter::Scalar::Int(7),
                    inclusive: true,
                }),
                hi: None,
            },
        });
        let hot = engine.viewport(&session, hot).unwrap().points.len();
        (all, hot)
    };
    // Each shard's listed pairs' views, and whether it lists the family's base.
    let stored = |root: &std::path::Path| -> Vec<(Vec<String>, bool)> {
        side_manifest(root)
            .shards
            .records
            .iter()
            .map(|record| {
                (
                    record.scoped_columns.iter().map(|c| c.view.clone()).collect(),
                    record.runtime_bases.contains(&"heat".to_string()),
                )
            })
            .collect()
    };
    let k = || vec!["g:k".to_string()];
    assert_eq!(in_view(&engine), (1, 1));
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(in_view(&engine), (1, 1));

    compact(&engine, Some(ShardId::ZERO));
    assert_eq!(
        stored(&several.root),
        vec![(k(), true), (k(), false)],
        "shard 0 holds the column's base now, and shard 1 not the family's"
    );
    assert_eq!(in_view(&engine), (1, 1));
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(in_view(&engine), (1, 1));

    compact(&engine, Some(shard(1)));
    assert_eq!(
        stored(&several.root),
        vec![(Vec::new(), false), (Vec::new(), false)],
        "MANIFEST.json declares the family with the view, a base in every shard"
    );
    assert_eq!(in_view(&engine), (1, 1));
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(in_view(&engine), (1, 1));
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
                    let request = ViewportRequest::new("s0", 0, WHOLE_MAP, TWO_SHARD_N as usize)
                        .filter(FilterExpr::MemberOf(MemberOfLeaf {
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

/// A level's contents read at open, for a publication over both shards, are the ones the first
/// request serves: the request reads no table again.
#[test]
fn the_contents_read_at_open_serve_the_first_request() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    let several = two_shards();
    let engine = several.open_writing();
    let items = items_by_id(&engine, 0..TWO_SHARD_N);
    let sets = |ids: &[u64]| ShardSets::of_items(ids.iter().map(|id| items[*id as usize].unwrap()));
    let both = [
        ids_in_shard(&engine, TWO_SHARD_N, 0)[0],
        ids_in_shard(&engine, TWO_SHARD_N, 1)[0],
    ];
    engine.register_layer(label_layer("t")).unwrap();
    let mut artifact = IncomingArtifact::with_content(
        Some("a".into()),
        Vec::new(),
        vec![IncomingContent::new(vec!["label".to_string()], Vec::new())],
    );
    artifact.members = sets(&both);
    artifact.contents[0].generated_from = sets(&both);
    engine
        .publish_artifacts("t".into(), 0, vec![artifact])
        .unwrap();
    tick(&engine);
    drop(engine);

    let engine = several.open_writing();
    assert_eq!(
        engine.artifact_content_cache_stats().builds,
        1,
        "read at open"
    );
    let served = served_layer(&engine);
    assert_eq!(served[0].len(), 1);
    assert_eq!(served[0][0].2, vec!["label".to_string()]);
    assert_eq!(
        engine.artifact_content_cache_stats().builds,
        1,
        "the request read the table the open built"
    );
}

/// A publication with members and generating sets in both shards, a growth adding a shard-1 item
/// and a page moving a set into shard 1 serve the counts, containment and `member_of` verdicts one
/// shard holding the same items serves, before a tick, after it and across a restart; the tick
/// writes a member pack into each shard.
#[test]
fn memberships_over_both_shards_serve_as_one_shard_does() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open_writing());
    let in_shard_one = ids_in_shard(&engine, TWO_SHARD_N, 1);
    // An item of shard 1 the subset credential cannot see, and one it can.
    let unseen = *in_shard_one.iter().find(|id| *id % 3 != 0).unwrap();
    let seen = *in_shard_one.iter().find(|id| *id % 3 == 0).unwrap();
    let write = |engine: &Engine| {
        let items = items_by_id(engine, 0..TWO_SHARD_N);
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
        let members: Vec<u64> = (0..TWO_SHARD_N)
            .filter(|id| id % 2 == 0 && *id != unseen)
            .collect();
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
    let engine = several.open_writing();
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open_writing());
    let (zero, first) = (
        ids_in_shard(&engine, TWO_SHARD_N, 0),
        ids_in_shard(&engine, TWO_SHARD_N, 1),
    );
    let gone = first[1];
    let write = |engine: &Engine| {
        let items = items_by_id(engine, 0..TWO_SHARD_N);
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
        let target = item_by_id(engine, gone).target();
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

    compact(&engine, Some(shard(1)));
    assert_eq!(
        served_layer(&engine),
        expected,
        "after shard 1's compaction"
    );
    compact(&engine, Some(ShardId::ZERO));
    assert_eq!(
        served_layer(&engine),
        expected,
        "after shard 0's compaction"
    );
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(served_layer(&engine), expected, "across a restart");
}

/// A compaction of one shard chooses a level's layout from its shape over every shard: an overlap
/// only shard 1 holds moves the level to the list layout at shard 0's compaction, and both shards'
/// columns are written in it in that publication. Every viewer is served the same before, after
/// and across a restart.
#[test]
fn a_compaction_of_one_shard_chooses_a_layout_over_every_shard() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::ShardSets;
    use mosaica_store::manifest::DerivedForm;
    use mosaica_types::layer::ServingLayout;
    let several = two_shards();
    let engine = several.open_writing();
    let items = items_by_id(&engine, 0..TWO_SHARD_N);
    let (zero, first) = (
        ids_in_shard(&engine, TWO_SHARD_N, 0),
        ids_in_shard(&engine, TWO_SHARD_N, 1),
    );
    let sets = |ids: &[u64]| ShardSets::of_items(ids.iter().map(|id| items[*id as usize].unwrap()));
    let artifact = |key: &str, members: &[u64]| {
        let mut artifact = IncomingArtifact::with_content(
            Some(key.into()),
            Vec::new(),
            vec![IncomingContent::new(vec![key.to_string()], Vec::new())],
        );
        artifact.members = sets(members);
        artifact.contents[0].generated_from = sets(members);
        artifact
    };
    engine.register_layer(label_layer("t")).unwrap();
    engine
        .publish_artifacts(
            "t".into(),
            0,
            vec![
                artifact("a", &[zero[0], zero[1], first[0]]),
                artifact("b", &[zero[2], first[1]]),
            ],
        )
        .unwrap();
    tick(&engine);
    let layout = |root: &std::path::Path| {
        side_manifest(root)
            .partition
            .layers
            .iter()
            .find(|layer| layer.declaration.name == "t")
            .expect("the layer is registered")
            .layout_of(0)
    };
    assert_eq!(layout(&several.root), ServingLayout::RowMajorLabel);
    let mut growth = IncomingGrowth::from_entities("b".into(), Vec::new());
    growth.joining = sets(&[first[0]]);
    engine
        .grow_memberships("t".into(), 0, vec![growth])
        .unwrap();
    tick(&engine);
    let expected = served_layer(&engine);

    compact(&engine, Some(ShardId::ZERO));
    assert_eq!(
        layout(&several.root),
        ServingLayout::RowMajorList,
        "shard 1's overlap moves the level"
    );
    let columns: Vec<Vec<ServingLayout>> = derived_of(&several.root)
        .iter()
        .map(|extents| {
            extents
                .iter()
                .filter_map(|extent| match extent.form {
                    DerivedForm::RowColumn { layout } => Some(layout),
                    _ => None,
                })
                .collect()
        })
        .collect();
    assert_eq!(
        columns,
        vec![vec![ServingLayout::RowMajorList]; 2],
        "every shard's column is written in the new layout"
    );
    assert_eq!(served_layer(&engine), expected);
    drop(engine);
    let engine = several.open_writing();
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
    use arrow::array::{StringArray, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
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
    let keys: Vec<String> = (0..TWO_SHARD_N / 10).map(|k| format!("c-{k}")).collect();
    let members: Vec<(String, u64)> = (0..TWO_SHARD_N)
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
    write_points_n(&points, TWO_SHARD_N);
    write_pairs_n(&pairs, TWO_SHARD_N);
    std::fs::write(&config_path, LAYERED_TOML).unwrap();
    let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
        .expect("the layered fixture's config parses");
    let root = dir.join("bundle");
    build_with_layers_sized(&root, &points, &pairs, extent(), config, shard_size);
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

/// A membership-only row joining a shard-1 item to an artifact, and an edit moving a shard-1
/// member, serve the counts and `member_of` verdicts one shard holding the same items serves,
/// before a tick, after a tick and a flush, and across a restart; the edited item stays in shard 1.
#[test]
fn a_membership_row_and_an_edit_in_shard_one_serve_as_one_shard_does() {
    let one = layered_fixture(None);
    let several = layered_fixture(Some(TWO_SHARD_SIZE));
    let (one_engine, engine) = (one.open(), several.open_writing());
    let in_shard_one = ids_in_shard(&engine, TWO_SHARD_N, 1);
    let joiner = *in_shard_one.iter().find(|id| unheld(**id)).unwrap();
    let edited = *in_shard_one.iter().find(|id| !unheld(**id)).unwrap();
    let before = item_by_id(&engine, edited);
    for engine in [&one_engine, &engine] {
        name_item(engine, "join", joiner, None, Some("c-0"));
        name_item(engine, "edit", edited, Some((7.0, 7.0)), None);
    }
    let after = item_by_id(&engine, edited);
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
    let engine = several.open_writing();
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
    let several = layered_fixture(Some(TWO_SHARD_SIZE));
    let engine = several.open_writing();
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

    let joiner = *ids_in_shard(&engine, TWO_SHARD_N, 0)
        .iter()
        .find(|id| unheld(**id))
        .unwrap();
    let mut growth = IncomingGrowth::from_entities("c-3".into(), Vec::new());
    growth.joining = ShardSets::of_items([item_by_id(&engine, joiner)]);
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

    let first = |shard: u32| item_by_id(&engine, ids_in_shard(&engine, TWO_SHARD_N, shard)[0]);
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
    let several = layered_fixture(Some(TWO_SHARD_SIZE));
    let engine = several.open_writing();
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
    let engine = several.open_writing();
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let several = two_shards();
    let (one_engine, engine) = (one.open(), several.open_writing());
    let (zero, one_id) = (
        ids_in_shard(&engine, TWO_SHARD_N, 0)[0],
        ids_in_shard(&engine, TWO_SHARD_N, 1)[0],
    );
    let sets = |engine: &Engine, ids: &[u64]| {
        let items = items_by_id(engine, 0..TWO_SHARD_N);
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
                    ViewportRequest::new(view, 0, WHOLE_MAP, TWO_SHARD_N as usize),
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
    let several = layered_fixture(Some(TWO_SHARD_SIZE));
    let (one_engine, engine) = (one.open(), several.open_writing());
    let in_shard_zero = ids_in_shard(&engine, TWO_SHARD_N, 0);
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
        items_by_id(engine, 0..TWO_SHARD_N)
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
        item_by_id(&engine, edited).entity,
        EntityId::new(TWO_SHARD_SIZE),
        "the edit takes shard 0's high water, past the size"
    );
    let compare = |engine: &Engine, one_engine: &Engine, when: &str| {
        assert_eq!(served_layer(engine), served_layer(one_engine), "{when}");
        assert_eq!(
            visible_in(engine, "p"),
            visible_in(one_engine, "p"),
            "{when}"
        );
        assert_serves_as(engine, one_engine, when);
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
    let engine = several.open_writing();
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
    let several = layered_fixture(Some(TWO_SHARD_SIZE));
    let (one_engine, engine) = (one.open(), several.open_writing());
    let built_n = served_n(&several.root);
    // Ten fill shard 1 to its size and the last two open shard 2.
    let insert_twelve = |engine: &Engine| {
        let memberships = vec![mosaica_lifecycle::BatchMembership {
            layer: "t".to_string(),
            level: 0,
            view: None,
            key: "c-1".to_string(),
            rows: vec![9, 10, 11],
        }];
        let artifacts = mosaica_lifecycle::BatchArtifacts {
            memberships,
            edges: Vec::new(),
        };
        insert_keyed_joining(engine, "new", &[true; 12], artifacts)
    };
    insert_twelve(&one_engine);
    let inserted = insert_twelve(&engine);
    let shards: Vec<u32> = engine
        .resolve_targets(&inserted)
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
        assert_serves_as(engine, &one_engine, when);
    };
    compare(&engine, "before a tick");
    drop(engine);

    crash_back_to(&several.root, built_n);
    let engine = several.open_writing();
    compare(&engine, "across a restart that replays the opening");

    tick(&one_engine);
    tick(&engine);
    publish_buffered(&one_engine);
    publish_buffered(&engine);
    compare(&engine, "after a tick and a flush");
    let manifest = side_manifest(&several.root);
    let opened = manifest.shard(shard(2)).unwrap();
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
    let engine = several.open_writing();
    compare(&engine, "across a restart that reads the manifest");
}
