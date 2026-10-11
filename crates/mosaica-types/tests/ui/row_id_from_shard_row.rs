//! A `RowId` is read out of a `ShardRow` only by naming the shard it is expected in, with
//! `row_in`. There is no accessor that gives the row whatever its shard.

use mosaica_types::{RowId, ShardId, ShardRow};

fn main() {
    let _: RowId = ShardRow::new(ShardId::ZERO, RowId::new(7)).row();
}
