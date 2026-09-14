mod identity;
pub use identity::{IdentityError, IdentityKey, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

/// The process's own memory — the allocator's retention and the kernel's resident figures.
///
/// Here because the build and the serve path both need it and this is the lowest crate both can
/// see (decision 0139). It carries no feature gate: the calls are platform-gated inside the
/// module, and a crate that never asks about its own memory never names it.
pub mod process;

/// The annotation layer declaration, shared by the WAL record that makes a registration durable,
/// the manifest section that carries it, and the gate-filtered `/v1/meta` view.
///
/// Behind the `serde` feature because it exists only to be serialised — a declaration nobody can
/// write down is not a declaration. That is a different reason from the ID newtypes', which are
/// useful without it, and it is why this module is gated as a whole rather than per-derive.
#[cfg(feature = "serde")]
pub mod layer;

/// The view roster — the record a create makes durable and the tombstone a drop leaves. Behind
/// `serde` for [`layer`]'s reason, and here for the crate-graph reason its own doc gives.
#[cfg(feature = "serde")]
pub mod view;

/// A vocabulary's two declared discriminants, shared by the manifest that carries a built one and
/// the WAL record that makes a runtime declaration durable. Behind `serde` for [`layer`]'s reason,
/// and here for the crate-graph reason [`view`] gives.
#[cfg(feature = "serde")]
pub mod vocabulary;

/// Macro for creating ID newtypes with no cross-space conversions (invariant I4).
/// Each type gets new(raw) and raw(self) methods, with derives Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug.
/// Under the (off-by-default) `serde` feature, also derives `Serialize`/`Deserialize`
/// (transparent, single-field) so newtypes can appear in on-disk formats (e.g. the lifecycle
/// WAL) without leaking a cross-type conversion.
macro_rules! define_id_newtype {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        pub struct $name($inner);

        impl $name {
            #[inline]
            pub const fn new(raw: $inner) -> Self {
                $name(raw)
            }

            #[inline]
            pub const fn raw(self) -> $inner {
                self.0
            }
        }
    };
}

// Define integer ID newtypes — no conversions between types (I4)
define_id_newtype!(EntityId, u64);
define_id_newtype!(RowId, u32);
define_id_newtype!(TermId, u32);
// An attribute index ordinal, local to one column (`docs/design/filter-index.md` §2.2).
//
// **Deliberately not convertible to `TermId`, and the reason is an authorisation one.** The two
// indexes share the CSR postings format, so a call site could otherwise pass one where the other
// belongs. A `TermId` gates label containment and frontier depth (I3); an `AttrLocalId` may only
// ever narrow `M_sel` (I12). Mistaking the second for the first unions the items carrying an
// attribute value into `M_auth` — the authorisation bypass separate files were chosen to make
// impossible (per-point-attributes §3.5), arriving instead through a shared reader. Separate files
// close the descriptor collision; only separate types close the call-site one.
//
// This is also why the shared format takes a bare `u32` rather than either newtype
// (`tessera_authz::PostingsReader::posting_at`): typing it in one crate's newtype would force the
// other to convert at every call, reintroducing the crossing as boilerplate.
define_id_newtype!(AttrLocalId, u32);
define_id_newtype!(Handle, u32);
define_id_newtype!(Priority, u16);
define_id_newtype!(MortonCode, u32);

/// String newtype for view identifiers
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ViewId(pub String);

/// String newtype for segment identifiers
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegId(pub String);

/// Which geometry a response was answered from: `(prefix, segments_version)`.
///
/// **A stamp, not a selector.** A client echoes the stamp of the response it is holding back on
/// its next request, and the server answers from the **live** geometry regardless, reporting only
/// whether anything has moved since (`geometry-pinning.md` §7). Presenting a superseded stamp is
/// an ordinary request with an ordinary answer — never a `410`, never a refusal, and never a route
/// to a superseded generation. Nothing is retained on its behalf.
///
/// **This is not I11.** I11 is the *within-request* rule — a request resolves its geometry once
/// and uses it throughout, which an `Arc` held for the request's duration gives for free. The
/// cross-request half, which this type used to implement, was deleted:
/// `geometry-pinning.md` carries the argument, and its §4 carries the one thing a future reader
/// must not get wrong (merge permutes row space within the merged span, so **no row-space artefact
/// may key on the prefix**).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GenerationStamp {
    pub prefix: String,
    pub segments_version: u64,
}

// Constants (contracts spec r3)
// 2: `declared_scalars[..].filter` renamed to `index` and `record_extents` required in
// SEGMENTS-<n>.json (records-and-search §2/§3/§7).
// 3: a vocabulary's `listing` renamed to `visibility` with `per_viewer` respelt `derived`, and a
// value's `label` renamed to `title` (configuration.md §1, decision 0088). The second is the one
// that needs the number: `title` is optional, so a bundle at 2 opens against a reader at 3 with
// every value title silently dropped, where the required key refuses loudly on its own.
// 4: every `views` entry carries the `projection` it was built under (projections.md §3). The
// frame alone does not imply one — a `[0, 1]` extent is a legal frame for a view with no
// projection at all — so a bundle at 3 read as unprojected would have every second reader
// quantising a degree as though it were a frame coordinate. The key is required, which is what
// makes a bundle at 3 refuse at open rather than open as `none`.
// 5: an artifact record carries its parents as a list — a `dag` layer's child may name several
// (`dag-hierarchies.md` §7, decision 0117) — in the record blob's hand-rolled encoding
// (`tessera_lifecycle::membership`, decision 0077) and in the WAL row. The blob at 4 carried a
// one-byte tag and one parent, so a reader at 5 would decode its first two bytes as a count and
// read parents out of the shape bytes that follow; the number is what stops it opening.
// 6: every `views` entry and every `groups` entry carries `point_default`, the view's declared
// `point_visibility.default` or `null` where none was declared (decision 0133). `/control/ingest`
// reads it to fill a row whose access label is empty, or to refuse the batch; a bundle at 5 has no
// field to read, and a reader defaulting it would either fill with a label nobody declared or
// refuse every unlabelled row of a corpus whose declaration filled them.
// 7: the record blob stores a digest and the generating set's cardinality beside every content and
// a digest beside every shape (`ingest.md` §1.5, §7.1; decision 0136), and the segments manifest
// gains the four lists that are the durable home of a declaration made while the service runs
// (`attributes`, `scoped_attributes`, `vocabularies`, `groups`). A reader at 7 would take the
// first forty bytes of a 6 blob's generating set as a digest and a cardinality and read the set
// out of the bytes that follow; the number is what stops it opening.
// 8: the record blob carries the artifact's view beside its key — part of the identity on a
// layer whose `scope` names a group (`ingest.md` §1.5, `views.md` §3.5; decision 0136, ruling 9).
// A reader at 8 would take the two bytes a 7 blob spends on its content count as a view length
// and read the view out of the membership; and a 7 blob read by a reader that ignored the field
// restores every artifact of a group-scoped level with no view, collapsing two views' keys into
// one index. The number is what stops it opening.
// 9: the record blob states identity once per block instead of once per row (decision 0141). A row
// is now its fields alone: the entity and the payload length that headed it are gone, the block
// carries its first entity and one varint gap per row after it, and the directory's row offsets
// are what delimit a row. A reader at 9 would take the first four bytes of an 8 block's first row
// as a row count and address the rest of the blob against it; an 8 reader handed a 9 block would
// read the block header as a row and serve its bytes as an entity's fields. The number is what
// stops either opening.
// 10: the record blob delimits a row by a length the row states, and the directory holds nothing
// per row. A 9 directory's fifth column is the blocks' row offsets as a list; a 10 directory's is
// one row count a block, so a reader at either takes the other's fifth column for its own and
// mis-reads every block boundary. A 9 block's rows carry no length, so a 10 reader would take the
// first two bytes of a row's first field tag as a length and frame the rest against it. The number
// is what stops either opening.
// 11: a segment holds `cuts.u32` beside `morton.u32` — where each occupied leaf Morton cell's rows
// begin, which is what selection evaluates per cell instead of per row. A 10 bundle does not have
// the file, and the open names every segment file it maps, so a reader at 11 refuses it there. The
// number is what says the absence is a stale bundle rather than a damaged one.
// Each bump makes a stale local bundle a loud refusal rather than a silent misread — a fail-closed
// guard, not compatibility (decision 0048).
pub const BUNDLE_FORMAT: u32 = 11;
pub const API_VERSION: u32 = 1;
pub const ABI_VERSION: u32 = 1;
pub const ROW_ABSENT: u32 = 0xFFFF_FFFF;
pub const NODE_NONE: u32 = 0xFFFF_FFFF;
pub const SMALL_TERM_THRESHOLD_DEFAULT: u32 = 32;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_distinct_types_with_raw_access() {
        let e = EntityId::new(7);
        let r = RowId::new(7);
        assert_eq!(e.raw(), 7u64);
        assert_eq!(r.raw(), 7u32);
        // The I4 claim that used to sit here as a commented-out line — `let _: RowId = e.into();`,
        // annotated "MUST NOT compile" — is now checked, in `tests/ui/entity_id_into_row_id.rs`.
        // A comment cannot fail; that one was written when the conversion was already absent and
        // would have gone on reading as an assurance for as long as nobody added the conversion.
        // Adding a `From<EntityId> for RowId` makes the compile-fail row fail, measured.
    }
    #[test]
    fn constants() {
        assert_eq!(BUNDLE_FORMAT, 11);
        assert_eq!(ROW_ABSENT, 0xFFFF_FFFF);
    }
}
