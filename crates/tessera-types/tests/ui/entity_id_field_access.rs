//! I4, precondition: the newtypes' inner fields are private, so no caller can reach around
//! `new`/`raw` to move a value between the two spaces without saying so. Named for `EntityId`,
//! which is what it exercises — the same privacy holds for every newtype the macro defines, and
//! this row would have been misfiled as a `RowId` case had a reviewer not read it.

use tessera_types::EntityId;

fn main() {
    let e = EntityId::new(7);
    let _ = e.0;
}
