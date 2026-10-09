//! I4 as compile errors: permissions live in entity space, geometry in row space, and the two meet
//! only at an explicit permutation.
//!
//! # Why a compile-fail harness rather than a runtime test
//!
//! I4 is not a property of what the code *does*; it is a property of what the code *cannot say*.
//! `EntityId` and `RowId` are distinct newtypes with no conversion between them, and
//! `tessera_store::Permutation::row_of` is documented as the only crossing in the tree. A runtime
//! test can confirm that the crossing which exists behaves; it cannot confirm that the crossings
//! which do not exist are refused, because code that does not compile cannot be run. Conformance
//! §4.1 names this and I8 as the two rows a compile-fail harness is uniquely suited to, and until
//! now they were also the two rows with no evidence at all: the invariant was upheld in the type
//! system and proved by nothing.
//!
//! Before this, the negative case lived as a commented-out line in `lib.rs`'s test module —
//! `// let _: RowId = e.into();` — with a note that it must not compile. A comment cannot fail.
//! The cases in `tests/ui/` are that same claim, made checkable.
//!
//! # What is asserted, and what is not
//!
//! Each `.rs` file under `tests/ui/` must fail to compile, with the error in its paired `.stderr`.
//! That pairing is what stops the harness rotting into a tautology: a file that failed for an
//! unrelated reason — a typo, a missing import, a renamed type — would still "fail to compile" and
//! would still pass a harness that only checked for failure. The `.stderr` fixture pins *which*
//! error, so the day a `From` impl is added between the two spaces, the test fails with a diff
//! rather than quietly continuing to pass on a stale error.
//!
//! **This proves the conversion is absent, not that no code path derives a row id from an entity
//! id by other means**, and the gap is wider than a first draft of this comment allowed. An
//! explicit cast is invisible to the type system by construction — `RowId::new(e.raw() as u32)`
//! compiles — and it is invisible to `scripts/check-layers.sh` too, whose I4 rule greps for
//! `impl From` between the ID newtypes in this crate and nothing else. The draft deferred to that
//! script as though it carried the other half; it does not.
//!
//! So I4's enforcement, stated completely: distinct newtypes with private fields, no `From` impl
//! (checked here and by the script), no field access (checked here), and **a code review for the
//! explicit cast**. `tessera_store::Permutation::row_of` is the only legitimate crossing, and the
//! cast is the one spelling nothing mechanical refuses.
//!
//! # I8 has no row here
//!
//! §4.1's other case is that a label's generating set is immutable once supplied. There are no
//! generating sets, so there is nothing whose mutation could be refused. A placeholder asserting
//! that some stand-in type is immutable would report green while checking nothing the invariant is
//! about; §4.6 records I8 as uncovered instead.
//!
//! # Toolchain sensitivity, stated because the first unexplained failure will look like a defect
//!
//! `.stderr` fixtures are compiler output, so a rustc release can reword them and turn this gate
//! red on phrasing alone. **Nothing prevents that:** `rust-toolchain.toml` says `channel =
//! "stable"`, which is a rolling channel, not a pin. An earlier version of this comment claimed
//! the file pinned a version and therefore protected these fixtures — it does not, and believing
//! it would make regenerating them feel like routine maintenance.
//!
//! `TRYBUILD=overwrite cargo test -p tessera-types` regenerates them. **Read the diff.** A changed
//! *error code* or a row that stopped failing is a finding — the whole point of pairing each case
//! with its `.stderr` is that "it still fails to compile" is not enough. Only changed *phrasing*
//! is noise. That judgement is the only thing standing between a rustc upgrade and a silently
//! weakened I4 gate.

#[test]
fn entity_space_and_row_space_do_not_convert() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
