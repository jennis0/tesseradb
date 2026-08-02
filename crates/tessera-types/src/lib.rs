mod identity;
pub use identity::{IdentityError, IdentityKey, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

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
            pub fn new(raw: $inner) -> Self {
                $name(raw)
            }

            #[inline]
            pub fn raw(self) -> $inner {
                self.0
            }
        }
    };
}

// Define integer ID newtypes — no conversions between types (I4)
define_id_newtype!(EntityId, u64);
define_id_newtype!(RowId, u32);
define_id_newtype!(TermId, u32);
define_id_newtype!(Handle, u32);
define_id_newtype!(Priority, u16);
define_id_newtype!(MortonCode, u32);

/// String newtype for slice identifiers
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SliceId(pub String);

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
pub const BUNDLE_FORMAT: u32 = 1;
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
        assert_eq!(BUNDLE_FORMAT, 1);
        assert_eq!(ROW_ABSENT, 0xFFFF_FFFF);
    }
}
