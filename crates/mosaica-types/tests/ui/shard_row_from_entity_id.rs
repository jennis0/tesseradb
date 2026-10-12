//! An entity id names no row, so it does not become a `ShardRow`.

use mosaica_types::{EntityId, ShardRow};

fn main() {
    let _: ShardRow = EntityId::new(7).into();
}
