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
//! id by other means.** Arithmetic on `raw()` values would be invisible here, as it is to the type
//! system. `scripts/check-layers.sh` carries the grep-based half of that (no cross-space
//! conversions outside the permutation), and neither check subsumes the other.
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
//! `.stderr` fixtures are compiler output, so a rustc upgrade can reword them. `rust-toolchain.toml`
//! pins the version this repository builds with, which is what keeps that from being a moving
//! target; when the pin moves and these fail on wording alone, `TRYBUILD=overwrite cargo test -p
//! tessera-types` regenerates them, and the diff should be read rather than accepted — a changed
//! *error* is a finding, only changed *phrasing* is noise.

#[test]
fn entity_space_and_row_space_do_not_convert() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
