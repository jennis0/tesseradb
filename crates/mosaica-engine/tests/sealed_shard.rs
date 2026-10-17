//! Edits of a shard's items: each takes its new entity in the item's own shard, an id freed
//! there at tenancy 0 first and then the shard's high water, which runs past the size; a shard
//! whose ids are spent refuses the edit and changes nothing.
//!
//! Written through the engine's several-shard feature.

mod common;

use common::*;
use mosaica_engine::{AcceptError, Engine};
use mosaica_lifecycle::ExecError;
use mosaica_types::{ItemAt, MosaicaId, ShardFull, ShardId, ShardLimit, ShardState, ENTITY_END};

/// Items the fixture builds.
const N: u64 = 40;
/// The size the fixture is built at: shard 0 sealed at 25 items, shard 1 open at 15.
const SIZE: u64 = 25;

/// An engine over `built` that writes several shards, its executor running.
fn writing(built: &Fixture) -> Engine {
    let mut engine =
        Engine::open_writing_several_shards(&built.root, &built.cache, &built.wal, config())
            .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

/// The item the unique `id` value `id` names.
fn item_of(engine: &Engine, id: u64) -> ItemAt {
    let table = mosaica_engine::AddressTable {
        rows: 1,
        mosaica_id: None,
        columns: vec![(
            "id".to_string(),
            vec![Some(mosaica_engine::AddressValue::Text(id.to_string()))],
        )],
    };
    engine.name_items(&table).unwrap().verdicts[0]
        .clone()
        .expect("the value names an item")
}

/// The `id`s of the fixture's items in shard 0.
fn ids_in_shard_zero(engine: &Engine) -> Vec<u64> {
    (0..N)
        .filter(|id| item_of(engine, *id).shard == ShardId::ZERO)
        .collect()
}

/// One batch moving the item of `id` to another position: an edit. The `mosaica_id` its receipt
/// answers.
fn edit(engine: &Engine, batch: &str, id: u64) -> Result<MosaicaId, AcceptError> {
    let receipt = engine.ingest(mosaica_engine::IngestRequest {
        batch_id: batch.to_string(),
        body_hash: [4; 32],
        view: Some("s0".to_string()),
        rows: vec![mosaica_lifecycle::IngestRow {
            mosaica_id: None,
            labels: Some(vec![b"0".to_vec()]),
            position: Some((7.0, 7.0)),
            scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(id)],
            scoped: Vec::new(),
            omitted: Vec::new(),
        }],
        artifacts: Default::default(),
        strict: true,
        mosaica_id_column: false,
    })?;
    Ok(receipt.mosaica_ids[0].expect("the row names the item"))
}

/// Each shard's state, high water and freed ids as the served side-manifest records them.
fn recorded(root: &std::path::Path) -> Vec<(u32, ShardState, u64, Vec<u32>)> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| {
            (
                record.id.raw(),
                record.state,
                record.high_water,
                record
                    .free_entities
                    .entities()
                    .map(|ids| ids.iter().collect())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// An edit of a sealed shard-0 item takes the ids shard 0 freed at tenancy 0, lowest first, then
/// shard 0's high water past the size, never an id of the open shard 1; the item keeps its
/// `mosaica_id`, which names the new entity, and shard 1's high water does not move.
#[test]
fn an_edit_of_a_sealed_shards_item_stays_in_its_shard_and_runs_past_the_size() {
    let built = fixture_sized(N, Some(SIZE));
    seed_freed(&built.root, ShardId::ZERO, 27, &[(0, &[25, 26])]);
    let engine = writing(&built);
    let edited: Vec<u64> = ids_in_shard_zero(&engine).into_iter().take(3).collect();

    let mut moved = Vec::new();
    for (at, id) in edited.iter().enumerate() {
        let before = item_of(&engine, *id);
        let named = mosaica_id_at(before.entity, 0);
        let answered = edit(&engine, &format!("e{at}"), *id).expect("the edit is accepted");
        assert_eq!(answered, named, "the item keeps its mosaica_id");
        let after = item_of(&engine, *id);
        assert_eq!(
            engine.resolve_targets(&[named]).unwrap(),
            vec![Some(after.target())]
        );
        moved.push((after.shard.raw(), after.entity.raw()));
    }
    assert_eq!(moved, vec![(0, 25), (0, 26), (0, 27)]);
    assert_eq!(engine.allocator_high_water(), 28 + (N - SIZE));

    publish_buffered(&engine);
    let wanted = vec![
        (0, ShardState::Sealed, 28, vec![]),
        (1, ShardState::Open, N - SIZE, vec![]),
    ];
    assert_eq!(recorded(&built.root), wanted);
    drop(engine);

    let engine = writing(&built);
    for (id, (shard, entity)) in edited.iter().zip(&moved) {
        let item = item_of(&engine, *id);
        assert_eq!((item.shard.raw(), item.entity.raw()), (*shard, *entity));
    }
    assert_eq!(engine.allocator_high_water(), 28 + (N - SIZE));
}

/// A sealed shard whose high water is at the end of its ids and which holds no id freed at tenancy
/// 0 refuses an edit of its item with the shard's figures, though it holds an id freed at tenancy
/// 1; nothing changes, and the open shard can still take the item's new entity no more than before.
#[test]
fn a_sealed_shard_at_the_end_of_its_ids_refuses_an_edit_and_changes_nothing() {
    let built = fixture_sized(N, Some(SIZE));
    seed_freed(&built.root, ShardId::ZERO, ENTITY_END, &[(1, &[30])]);
    let engine = writing(&built);
    let id = ids_in_shard_zero(&engine)[0];
    let before = item_of(&engine, id);
    let high_water = engine.allocator_high_water();

    let refused = edit(&engine, "e", id);
    assert!(
        matches!(
            refused,
            Err(AcceptError::Exec(ExecError::ShardFull(ShardFull {
                limit: ShardLimit::End(ENTITY_END),
                high_water: ENTITY_END,
                needed: 1,
            })))
        ),
        "{refused:?}"
    );
    assert_eq!(item_of(&engine, id), before);
    assert_eq!(engine.allocator_high_water(), high_water);
    let logged = mosaica_lifecycle::wal::Wal::open(&built.wal)
        .unwrap()
        .records()
        .count();
    assert_eq!(logged, 0, "nothing is in the log");
}

/// An edit of an item of the open shard, whose high water is at the size and which holds no id
/// freed at tenancy 0, is accepted and takes the first id past the size. The size bounds new items
/// and not edits.
#[test]
fn an_edit_of_an_open_shard_at_its_size_runs_past_the_size() {
    let built = fixture_sized(N, Some(SIZE));
    let one = ShardId::new(1).unwrap();
    seed_freed(&built.root, one, SIZE, &[]);
    let engine = writing(&built);
    let id = (0..N)
        .find(|id| item_of(&engine, *id).shard == one)
        .expect("shard 1 holds an item");

    edit(&engine, "e", id).expect("the edit is accepted");
    let after = item_of(&engine, id);
    assert_eq!((after.shard, after.entity.raw()), (one, SIZE));
}
