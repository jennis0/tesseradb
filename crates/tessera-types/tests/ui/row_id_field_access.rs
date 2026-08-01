//! I4: the newtypes' inner fields are private, so no caller can reach around `new`/`raw` to move a
//! value between the two spaces without saying so.

use tessera_types::EntityId;

fn main() {
    let e = EntityId::new(7);
    let _ = e.0;
}
