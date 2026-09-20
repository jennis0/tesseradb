//! The tiler: sorts a batch of items into Morton/row order.
//!
//! One implementation shared by a build and a streaming flush, free of I/O. Priority is not
//! computed here: it is the leading 16 bits of the `tessera_id` the caller supplies.

use tessera_types::{EntityId, TesseraId};

use crate::morton::split32;

/// A declared-scalar value carried alongside the fixed columns (`tessera_id`, `residual`).
/// The kinds below are the whole set.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// Microseconds since the Unix epoch, stored as an `i64`; the type exists so the unit is in
    /// the manifest.
    TimestampUs(i64),
    Utf8(String),
    /// No value at all: distinct from every in-band value, including the empty string.
    ///
    /// A category expresses absence in band as the reserved code 0; a string has no such spare
    /// value, since the empty string is one a corpus may legitimately hold.
    Null,
}

impl ScalarValue {
    /// This value as a render column holds it: `columns.arrow`, non-nullable with nowhere to put
    /// [`ScalarValue::Null`]. The zero goes in the column, and a presence bitmap beside it
    /// records the substitution.
    pub fn or_render_placeholder(&self, ty: ScalarType) -> ScalarValue {
        if !matches!(self, ScalarValue::Null) {
            return self.clone();
        }
        match ty {
            ScalarType::Bool => ScalarValue::Bool(false),
            ScalarType::U8 => ScalarValue::U8(0),
            ScalarType::U16 => ScalarValue::U16(0),
            ScalarType::U32 => ScalarValue::U32(0),
            ScalarType::U64 => ScalarValue::U64(0),
            ScalarType::I8 => ScalarValue::I8(0),
            ScalarType::I16 => ScalarValue::I16(0),
            ScalarType::I32 => ScalarValue::I32(0),
            ScalarType::I64 => ScalarValue::I64(0),
            ScalarType::F32 => ScalarValue::F32(0.0),
            ScalarType::F64 => ScalarValue::F64(0.0),
            ScalarType::TimestampUs => ScalarValue::TimestampUs(0),
            // Unreachable in practice: `render` on strings is refused at schema parse.
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                ScalarValue::Utf8(String::new())
            }
        }
    }
}

/// The Arrow type of a declared scalar column, used to build `columns.arrow`'s schema.
///
/// The widths are narrow because a hot column is baked into every row and priced per bit; the
/// width is unalterable, since changing it rewrites the corpus. `Utf8` is the one variable-width
/// member; `Bool` packs to one bit per row in Arrow. `TimestampUs` stores as an `i64` and exists
/// for the declaration, not the storage, and is the only time type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    TimestampUs,
    Utf8,
    /// A short string with no vocabulary, matched exactly. Its value is a [`ScalarValue::Utf8`];
    /// the type names the storage, a per-layer front-coded dictionary with a `u32` ordinal per
    /// present entity. There is no `ScalarValue::Keyword`, since an ordinal means nothing outside
    /// its layer.
    Keyword,
    /// Prose, matched by what it says rather than by its bytes. Its value is a
    /// [`ScalarValue::Utf8`]; the value lives in the record blob, and `index = true` adds a
    /// per-layer token dictionary over terms a named analyser produced.
    ///
    /// The analyser is part of the column's declaration, recorded in the manifest.
    Text,
}

impl ScalarType {
    /// The spelling `MANIFEST.declared_scalars[].arrow_type` uses.
    ///
    /// These are the short forms a schema author types: `u8`, `u16`, `u32` and so on.
    pub fn arrow_type_name(self) -> &'static str {
        match self {
            ScalarType::Bool => "bool",
            ScalarType::U8 => "u8",
            ScalarType::U16 => "u16",
            ScalarType::U32 => "u32",
            ScalarType::U64 => "u64",
            ScalarType::I8 => "i8",
            ScalarType::I16 => "i16",
            ScalarType::I32 => "i32",
            ScalarType::I64 => "i64",
            ScalarType::F32 => "f32",
            ScalarType::F64 => "f64",
            ScalarType::TimestampUs => "timestamp_us",
            ScalarType::Utf8 => "utf8",
            ScalarType::Keyword => "keyword",
            ScalarType::Text => "text",
        }
    }

    /// The inverse of [`ScalarType::arrow_type_name`]; `None` for a spelling this build cannot
    /// write, treated as fail-closed rather than as an absent column.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => ScalarType::Bool,
            "u8" => ScalarType::U8,
            "u16" => ScalarType::U16,
            "u32" => ScalarType::U32,
            "u64" => ScalarType::U64,
            "i8" => ScalarType::I8,
            "i16" => ScalarType::I16,
            "i32" => ScalarType::I32,
            "i64" => ScalarType::I64,
            "f32" => ScalarType::F32,
            "f64" => ScalarType::F64,
            "timestamp_us" => ScalarType::TimestampUs,
            "utf8" => ScalarType::Utf8,
            "keyword" => ScalarType::Keyword,
            "text" => ScalarType::Text,
            _ => return None,
        })
    }

    /// Bits, not bytes, this column adds to every row; `None` for the two string types, since
    /// neither is ever in a row.
    ///
    /// [`ScalarType::Bool`] costs one bit; a byte-denominated figure would round it to 0 or 1.
    pub fn row_bits(self) -> Option<u64> {
        Some(match self {
            ScalarType::Bool => 1,
            ScalarType::U8 | ScalarType::I8 => 8,
            ScalarType::U16 | ScalarType::I16 => 16,
            ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 32,
            ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 64,
            // Neither is ever in a row: `render` is refused on both at the declaration, and their
            // storage is entity-space (a keyword's ordinal) or the record blob (a text value).
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => return None,
        })
    }

    /// Whether a value of this type can be a category code: `u8`, `u16` or `u32` only, since a
    /// wider code space is not a vocabulary and `utf8` cannot index one at all.
    pub fn is_category_width(self) -> bool {
        matches!(self, ScalarType::U8 | ScalarType::U16 | ScalarType::U32)
    }

    /// The largest code this width can carry. Code `0` is the reserved absent sentinel, so the
    /// usable count is one less than this.
    pub fn max_code(self) -> Option<u32> {
        Some(match self {
            ScalarType::U8 => u8::MAX as u32,
            ScalarType::U16 => u16::MAX as u32,
            ScalarType::U32 => u32::MAX,
            _ => return None,
        })
    }
}

/// One item to be placed into a segment: its wire identity, geometry, and any declared scalars.
/// Geometry is already quantised: 32-bit fixed point per axis against the build extent
/// ([`crate::fixed32`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TilerItem {
    pub tessera_id: TesseraId,
    /// 32-bit fixed-point x against the build extent; `qx >> 16` is the cell.
    pub qx: u32,
    /// 32-bit fixed-point y against the build extent; `qy >> 16` is the cell.
    pub qy: u32,
    pub scalars: Vec<ScalarValue>,
}

/// Sort `items` into segment (row) order: `(morton, tessera_id)` ascending. `tessera_id` is a
/// bijection over 2^64 with one row per entity, so no further tiebreak is needed. The entity ID
/// is not a sort key; it is passed alongside to keep `permutation.bin` and the external-ID
/// sidecars aligned with the new row order.
///
/// Returns the sorted items' Morton codes as `u32`s: the high half of each item's fixed-point
/// position ([`split32`]), already computed against the build extent, so this does not
/// re-quantise. `entity_ids` is permuted identically to `items` and must be the same length.
pub fn sort_batch(items: &mut Vec<TilerItem>, entity_ids: &mut Vec<EntityId>) -> Vec<u32> {
    assert_eq!(
        items.len(),
        entity_ids.len(),
        "sort_batch: items and entity_ids must be the same length"
    );

    let codes: Vec<u32> = items
        .iter()
        .map(|item| split32(item.qx, item.qy).0.raw())
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        codes[a]
            .cmp(&codes[b])
            .then_with(|| items[a].tessera_id.cmp(&items[b].tessera_id))
    });

    // Each index appears once in `order`, so every item moves and none is cloned.
    let mut unsorted: Vec<Option<TilerItem>> = items.drain(..).map(Some).collect();
    items.extend(order.iter().map(|&i| unsorted[i].take().expect("an index sorts once")));
    *entity_ids = order.iter().map(|&i| entity_ids[i]).collect();
    order.iter().map(|&i| codes[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morton::{morton_of, Bounds};

    fn unit_extent() -> Bounds {
        Bounds {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        }
    }

    /// An item at coordinate `(x, y)`, quantised against the unit extent the way the importer
    /// quantises: the tiler itself never sees a coordinate.
    fn item(tessera_id: u64, x: f64, y: f64) -> TilerItem {
        let e = unit_extent();
        TilerItem {
            tessera_id: TesseraId::new(tessera_id),
            qx: crate::morton::fixed32(x, e.x_min, e.x_max),
            qy: crate::morton::fixed32(y, e.y_min, e.y_max),
            scalars: vec![],
        }
    }

    #[test]
    fn sorts_by_morton_then_tessera_id() {
        // Two items at the identical coordinate plus a third sharing the leading 16 bits: must
        // order purely by ascending `tessera_id`, or the Morton collision proves nothing.
        let mut items = vec![item(9, 0.5, 0.5), item(2, 0.5, 0.5), item(1, 0.5, 0.5)];
        let mut entity_ids = vec![EntityId::new(90), EntityId::new(20), EntityId::new(10)];
        let codes = sort_batch(&mut items, &mut entity_ids);
        assert_eq!(
            items.iter().map(|i| i.tessera_id.raw()).collect::<Vec<_>>(),
            vec![1, 2, 9]
        );
        // `entity_ids` must be permuted identically to `items`.
        assert_eq!(
            entity_ids.iter().map(|e| e.raw()).collect::<Vec<_>>(),
            vec![10, 20, 90]
        );
        assert_eq!(codes.len(), 3);
        assert_eq!(codes[0], codes[1]);
        assert_eq!(codes[1], codes[2]);
    }

    #[test]
    fn ordering_by_the_priority_prefix_then_the_full_id_equals_ordering_by_the_id() {
        // `priority` is a prefix of `tessera_id`, so the two orders are the same order. Ids
        // share their high 16 bits so the test proves something about the tie.
        let ids: Vec<TesseraId> = vec![
            TesseraId::new(0x0001_0000_0000_0005),
            TesseraId::new(0x0001_0000_0000_0002),
            TesseraId::new(0x0001_0000_0000_0009),
            TesseraId::new(0x0002_0000_0000_0000),
            TesseraId::new(0x0000_ffff_ffff_ffff),
        ];

        let mut by_id = ids.clone();
        by_id.sort();

        let mut by_prefix_then_id = ids.clone();
        by_prefix_then_id.sort_by(|a, b| a.priority().cmp(&b.priority()).then(a.cmp(b)));

        assert_eq!(by_id, by_prefix_then_id);
    }

    #[test]
    fn returned_codes_are_non_decreasing() {
        let mut items = vec![item(1, 0.9, 0.9), item(2, 0.1, 0.1), item(3, 0.5, 0.5)];
        let mut entity_ids = vec![EntityId::new(1), EntityId::new(2), EntityId::new(3)];
        let codes = sort_batch(&mut items, &mut entity_ids);
        assert!(codes.windows(2).all(|w| w[0] <= w[1]));
    }

    /// The code the tiler returns for a point equals `morton_of` on the coordinate it came from:
    /// the join between shifting the importer's fixed point and quantising directly.
    #[test]
    fn returned_codes_are_u32_and_match_morton_of_on_the_source_coordinates() {
        let e = unit_extent();
        let coords = [(1u64, 0.75, 0.75), (2, 0.10, 0.10)];
        let mut items: Vec<TilerItem> = coords.iter().map(|&(id, x, y)| item(id, x, y)).collect();
        let mut entity_ids = vec![EntityId::new(1), EntityId::new(2)];
        let codes: Vec<u32> = sort_batch(&mut items, &mut entity_ids);
        assert_eq!(codes.len(), 2);
        assert!(codes[0] <= codes[1]);
        for (i, it) in items.iter().enumerate() {
            let &(_, x, y) = coords
                .iter()
                .find(|&&(id, _, _)| id == it.tessera_id.raw())
                .expect("every sorted item came from a source coordinate");
            assert_eq!(
                codes[i],
                morton_of(x, y, &e).raw(),
                "row {i}: returned code must equal morton_of() on the source coordinate"
            );
        }
    }
}
