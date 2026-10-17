//! Edits of a shard's items: each takes its new entity in the item's own shard, an id freed
//! there at tenancy 0 first and then the shard's high water, which runs past the size; a shard
//! whose ids are spent refuses the edit and changes nothing.
//!
//! Written through the engine's several-shard feature.

mod common;

use common::*;
use mosaica_engine::AcceptError;
use mosaica_lifecycle::ExecError;
use mosaica_types::{ShardFull, ShardId, ShardLimit, ShardState, ENTITY_END};

/// An edit of a sealed shard-0 item takes the ids shard 0 freed at tenancy 0, lowest first, then
/// shard 0's high water past the size, never an id of the open shard 1; the item keeps its
/// `mosaica_id`, which names the new entity, and shard 1's high water does not move.
#[test]
fn an_edit_of_a_sealed_shards_item_stays_in_its_shard_and_runs_past_the_size() {
    let built = two_shards();
    seed_freed(&built.root, ShardId::ZERO, 27, &[(0, &[25, 26])]);
    let engine = built.open_writing();
    let edited: Vec<u64> = ids_in_shard(&engine, TWO_SHARD_N, 0)
        .into_iter()
        .take(3)
        .collect();

    let mut moved = Vec::new();
    for (at, id) in edited.iter().enumerate() {
        let before = item_by_id(&engine, *id);
        let named = mosaica_id_at(before.entity, 0);
        let answered = edit_by_id(&engine, &format!("e{at}"), *id).expect("the edit is accepted");
        assert_eq!(answered, named, "the item keeps its mosaica_id");
        let after = item_by_id(&engine, *id);
        assert_eq!(
            engine.resolve_targets(&[named]).unwrap(),
            vec![Some(after.target())]
        );
        moved.push((after.shard.raw(), after.entity.raw()));
    }
    assert_eq!(moved, vec![(0, 25), (0, 26), (0, 27)]);
    assert_eq!(
        engine.allocator_high_water(),
        28 + (TWO_SHARD_N - TWO_SHARD_SIZE)
    );

    publish_buffered(&engine);
    let wanted = (
        vec![
            (0, ShardState::Sealed, 28, vec![], 0),
            (1, ShardState::Open, TWO_SHARD_N - TWO_SHARD_SIZE, vec![], 0),
        ],
        2,
    );
    assert_eq!(recorded(&side_manifest(&built.root)), wanted);
    drop(engine);

    let engine = built.open_writing();
    for (id, (shard, entity)) in edited.iter().zip(&moved) {
        let item = item_by_id(&engine, *id);
        assert_eq!((item.shard.raw(), item.entity.raw()), (*shard, *entity));
    }
    assert_eq!(
        engine.allocator_high_water(),
        28 + (TWO_SHARD_N - TWO_SHARD_SIZE)
    );
}

/// A sealed shard whose high water is at the end of its ids and which holds no id freed at tenancy
/// 0 refuses an edit of its item with the shard's figures, though it holds an id freed at tenancy
/// 1; nothing changes, and the open shard can still take the item's new entity no more than before.
#[test]
fn a_sealed_shard_at_the_end_of_its_ids_refuses_an_edit_and_changes_nothing() {
    let built = two_shards();
    seed_freed(&built.root, ShardId::ZERO, ENTITY_END, &[(1, &[30])]);
    let engine = built.open_writing();
    let id = ids_in_shard(&engine, TWO_SHARD_N, 0)[0];
    let before = item_by_id(&engine, id);
    let high_water = engine.allocator_high_water();

    let refused = edit_by_id(&engine, "e", id);
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
    assert_eq!(item_by_id(&engine, id), before);
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
    let built = two_shards();
    seed_freed(&built.root, shard(1), TWO_SHARD_SIZE, &[]);
    let engine = built.open_writing();
    let id = ids_in_shard(&engine, TWO_SHARD_N, 1)[0];

    edit_by_id(&engine, "e", id).expect("the edit is accepted");
    let after = item_by_id(&engine, id);
    assert_eq!(
        (after.shard, after.entity.raw()),
        (shard(1), TWO_SHARD_SIZE)
    );
}
