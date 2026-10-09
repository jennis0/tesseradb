//! I4, partially: the *direct* spelling of "reuse the number" is a type error, because `RowId::new`
//! takes a `u32` and `EntityId::raw` yields a `u64`.
//!
//! **This row does not close narrowing, and an earlier version of this comment claimed it did.**
//! `RowId::new(e.raw() as u32)` compiles, and so does the `try_into().unwrap()` form — which rustc
//! helpfully suggests, so the escape hatch is printed inside the `.stderr` fixture that was
//! supposed to close it. What this row actually pins is that the two spaces have incompatible
//! inner widths, so the naive spelling is refused rather than silently accepted.
//!
//! An explicit cast on a `raw()` value is invisible to the type system by construction, and
//! `scripts/check-layers.sh` does not catch it either — its I4 rule greps for `impl From` between
//! the ID newtypes in `tessera-types` and nothing else. So the honest statement of I4's
//! enforcement is: no implicit conversion, no `From` impl, no field access, and a code review for
//! the cast. `tessera_store::Permutation::row_of` remains the only legitimate crossing.

use tessera_types::{EntityId, RowId};

fn main() {
    let e = EntityId::new(7);
    let _ = RowId::new(e.raw());
}
