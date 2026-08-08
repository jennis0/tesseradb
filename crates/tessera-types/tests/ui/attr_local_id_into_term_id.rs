//! The attribute index's ordinal space and the authorisation index's must not convert.
//!
//! `AttrLocalId` and `TermId` are both `u32` ordinals into the same CSR postings format, and the
//! two files are read by the same code — so the one thing standing between them is that no
//! conversion exists. A `TermId` gates label containment (I3); an `AttrLocalId` may only narrow
//! `M_sel` (I12). Passing the second where the first belongs unions the items carrying an attribute
//! value into `M_auth`, which is the authorisation bypass `docs/design/filter-index.md` §2.1 exists
//! to make impossible rather than merely unlikely.
//!
//! Paired with `.stderr` for the reason the harness's module doc gives: a row that failed for an
//! unrelated reason would still "fail to compile".

use tessera_types::{AttrLocalId, TermId};

fn main() {
    let a = AttrLocalId::new(7);
    let _: TermId = a.into();
}
