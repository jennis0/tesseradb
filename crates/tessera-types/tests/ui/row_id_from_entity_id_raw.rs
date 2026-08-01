//! I4: nor may a row id be constructed from an entity id's raw value by widening or narrowing it.
//! `RowId::new` takes a `u32` and `EntityId::raw` yields a `u64`, so the direct spelling of "reuse
//! the number" is a type error. `tessera_store::Permutation::row_of` is the only crossing.

use tessera_types::{EntityId, RowId};

fn main() {
    let e = EntityId::new(7);
    let _ = RowId::new(e.raw());
}
