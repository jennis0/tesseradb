//! `ShardRow` has no `row()`: a row comes out only through `row_in`, naming the shard it is
//! expected in.

use mosaica_types::{RowId, ShardId, ShardRow};

fn main() {
    let _: RowId = ShardRow::new(ShardId::ZERO, RowId::new(7)).row();
}
