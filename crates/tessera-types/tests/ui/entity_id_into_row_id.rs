//! I4: an entity id must not convert into a row id. This is the case that lived as a commented-out
//! line in `lib.rs`'s test module, where it could never fail.

use tessera_types::{EntityId, RowId};

fn main() {
    let e = EntityId::new(7);
    let _: RowId = e.into();
}
