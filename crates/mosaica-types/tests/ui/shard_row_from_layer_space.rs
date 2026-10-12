//! A layer space holds no rows, so it does not become a `ShardRow`.

use mosaica_types::{LayerSpace, ShardRow};

fn main() {
    let _: ShardRow = LayerSpace::ZERO.into();
}
