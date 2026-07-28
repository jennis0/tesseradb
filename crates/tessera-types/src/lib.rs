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

/// Pin identity: geometry anchor (invariant I11).
/// Pinned to (prefix, segments_version, watermark); row-space artifacts identified by this tuple.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PinId {
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
        // The next line MUST NOT compile if uncommented — I4:
        // let _: RowId = e.into();
    }
    #[test]
    fn constants() {
        assert_eq!(BUNDLE_FORMAT, 1);
        assert_eq!(ROW_ABSENT, 0xFFFF_FFFF);
    }
}
