//! A `ShardRow` is a row in a named shard, built only from a `ShardId` and a `RowId`. An entity id
//! names no row, so neither a conversion nor the one-argument spelling of reusing its number may
//! make one.

use mosaica_types::{EntityId, ShardRow};

fn main() {
    let _: ShardRow = EntityId::new(7).into();
    let _ = ShardRow::new(EntityId::new(7).raw());
}
