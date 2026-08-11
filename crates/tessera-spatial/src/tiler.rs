//! The tiler: sorts a batch of items into Morton/row order (contracts §2.6, R3).
//!
//! One implementation shared by `tessera build` now and streaming flush later (plan §5).
//! Deliberately free of I/O — segment writing lives in `tessera-store`. Priority is **not**
//! computed here: it is the leading 16 bits of the `tessera_id` the caller supplies (contracts
//! §2.6 r6), so the tiler needs no separate value and the allocator owns nothing about it.

use tessera_types::{EntityId, TesseraId};

use crate::morton::split32;

/// A declared-scalar value carried alongside the fixed columns (`tessera_id`, `residual`).
/// The kinds below are the whole set (contracts §2.2).
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
    /// Microseconds since the Unix epoch. Stored as an `i64`; the *type* exists so the unit is in
    /// the manifest rather than a convention between a schema author and their client.
    TimestampUs(i64),
    Utf8(String),
    /// **No value at all** — distinct from every in-band value, including the empty string.
    ///
    /// A category expresses absence in band, as the reserved code 0, because its value space is the
    /// vocabulary's and 0 is reserved out of it. A string has no such spare value: the empty string
    /// is one a corpus may legitimately hold, and contracts §2.4 already refuses it on the ingest
    /// plane precisely because an unset field and a client bug both produce it. Folding the two
    /// together here would make "carries nothing" and "carries the empty string" the same answer to
    /// every filter — and the filter index would then report an item as matching a value it does
    /// not have.
    Null,
}

impl ScalarValue {
    /// This value as a **render** column holds it — `columns.arrow`, which is contractually
    /// non-nullable (contracts R4) and has nowhere to put [`ScalarValue::Null`].
    ///
    /// ⊘ **Absence is lost here, deliberately and visibly.** [Decision 0064] rules that a render
    /// column records absence in a presence bitmap beside it, exactly as a filter column does, and
    /// **defers the render half while the client is under active development**: it needs a file, a
    /// manifest entry, a way for the points batch to say "absent", and a client that understands
    /// it. Until that lands an absent number is drawn at the type's zero, so the two artefacts
    /// disagree — the filter says an item has no score while the map draws it at 0. That is a
    /// *narrowing* disagreement (**I12**: the filter shows fewer items, never more), which is why
    /// it is a stated residual rather than a blocker.
    ///
    /// This function is the one place that substitution happens, so the three render paths — the
    /// linear build, the streaming build and the flush — cannot come to disagree about it, and so
    /// the render half has one call site to delete when it lands.
    ///
    /// [Decision 0064]: ../../../docs/decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md
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
            // Unreachable in practice — `render` on `utf8` is refused at schema parse — and the
            // empty string rather than a panic, because this function's whole job is to keep a
            // non-nullable column writable.
            ScalarType::Utf8 => ScalarValue::Utf8(String::new()),
        }
    }
}

/// The Arrow type of a declared scalar column, used to build `columns.arrow`'s schema
/// (`scalar_schema` in [`crate::tiler`]'s consumers, e.g. `tessera_store::write::write_segment`).
///
/// **The narrow widths are the point, not a convenience.** A hot column is baked into every row
/// and priced at 0.93 GiB per byte per row per 10⁹ items (§10.5), so a category code declared
/// `u64` because that was the only integer available costs 7.45 GiB where `u8` costs 0.93. The
/// width is also unalterable — changing it rewrites the corpus (per-point-attributes §2.2), which
/// is why the set is widened here rather than left for a caller to work around.
///
/// `Utf8` is the one variable-width member and the one the segment writer pays an offset table
/// for. It predates the fixed-width set and is kept, but per-point-attributes §3.6 is explicit
/// that a category belongs in a fixed-width column: a string repeated per row is the vocabulary
/// stored a hundred million times.
/// **`Bool` is the only member that is not a flat slice of itself.** Arrow packs it to one bit per
/// row, so it is eight times cheaper than the `u8` a flag would otherwise cost — and every reader
/// of it needs the array rather than a `&[bool]`, which is why `ScalarSlice` carries a
/// `&BooleanArray` for it as it does for `Utf8`.
///
/// **`TimestampUs` stores as an `i64` and exists for the declaration, not the storage.** Without
/// it a time is an `i64` in the manifest and its unit is a convention between the schema author
/// and whoever reads the column; with it the unit is a fact a reader can check. It is deliberately
/// the *only* time type: nothing records a unit per column beyond the type name, so admitting
/// milliseconds too would let two builds store incomparable numbers under one declaration.
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
}

impl ScalarType {
    /// The spelling `MANIFEST.declared_scalars[].arrow_type` uses.
    ///
    /// **One definition, because there were two and they disagreed.** The flush path parsed
    /// `"u64"` while ingest validation compared against `"uint64"`, so a manifest either path
    /// accepted was one the other refused. Neither had ever run — `declared_scalars` was written
    /// empty unconditionally — so the disagreement was invisible until something populated it.
    /// These are the short forms because they are what the design's own §3.6 writes (`u8`,
    /// `u16`, `u32`) and what a schema author therefore types.
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
        }
    }

    /// The inverse of [`ScalarType::arrow_type_name`]; `None` for a spelling this build cannot
    /// write, which every caller must treat as fail-closed rather than as an absent column.
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
            _ => return None,
        })
    }

    /// **Bits**, not bytes, this column adds to every row — `None` for [`ScalarType::Utf8`],
    /// whose cost depends on the data.
    ///
    /// Bits because [`ScalarType::Bool`] costs one, and a byte-denominated figure would have to
    /// round it to either 0 or 1 — the first hiding the cost, the second reporting eight times it
    /// and erasing the reason to declare a `bool` at all.
    pub fn row_bits(self) -> Option<u64> {
        Some(match self {
            ScalarType::Bool => 1,
            ScalarType::U8 | ScalarType::I8 => 8,
            ScalarType::U16 | ScalarType::I16 => 16,
            ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 32,
            ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 64,
            ScalarType::Utf8 => return None,
        })
    }

    /// Whether a value of this type can be a category code — per-point-attributes §3.6's three
    /// declarable widths. `u64`, `i64`, `f32` and `utf8` are excluded: a code space wider than
    /// `u32` is not a vocabulary, and the last two cannot index one at all.
    pub fn is_category_width(self) -> bool {
        matches!(self, ScalarType::U8 | ScalarType::U16 | ScalarType::U32)
    }

    /// The largest code this width can carry. Code `0` is the reserved *absent* sentinel
    /// (§3.6), so the usable count is one less than this.
    pub fn max_code(self) -> Option<u32> {
        Some(match self {
            ScalarType::U8 => u8::MAX as u32,
            ScalarType::U16 => u16::MAX as u32,
            ScalarType::U32 => u32::MAX,
            _ => return None,
        })
    }
}

/// One item to be placed into a segment: its wire identity, geometry, and any declared
/// scalars. No `priority` field — it is a prefix of `tessera_id`, and a stored second copy
/// would be a second source of truth (contracts §2.6 r6, 2026-07-30 fold).
///
/// Geometry is **already quantised**: 32-bit fixed point per axis against the build extent
/// ([`crate::fixed32`]), not the coordinates the source held. The item therefore carries the
/// position in the one form from which both stored words — the cell code and its sub-cell
/// residual — fall out by shift and mask ([`crate::split32`]), so the tiler cannot disagree
/// with the segment writer about which cell a point is in. Coordinates are quantised exactly
/// once, upstream in the importer.
#[derive(Debug, Clone, PartialEq)]
pub struct TilerItem {
    pub tessera_id: TesseraId,
    /// 32-bit fixed-point x against the build extent; `qx >> 16` is the cell.
    pub qx: u32,
    /// 32-bit fixed-point y against the build extent; `qy >> 16` is the cell.
    pub qy: u32,
    pub scalars: Vec<ScalarValue>,
}

/// Sort `items` into segment (row) order: `(morton, tessera_id)` ascending (contracts §2.6 r6).
/// No further tiebreak: `tessera_id` is a bijection over 2^64 and there is one row per entity,
/// so the order is total — and because `priority` is the leading 16 bits of `tessera_id`,
/// ordering by `(morton, priority, tessera_id)` is identically this order. The entity ID is
/// **not** a sort key at any position; it is passed alongside so the caller can keep
/// `permutation.bin` and the external-ID sidecars aligned with the new row order. Row order is
/// key-dependent: a different deployment key reorders rows inside a Morton cell (contracts
/// §2.2, rotation).
///
/// Returns the sorted items' Morton codes as `u32`s, matching `morton.u32`'s on-disk
/// representation, in the same order as `items` post-sort. The code is 32 bits because §5.2
/// fixes the grid at 2^16 x 2^16 — a property of the *grid*, not the population, so this width
/// does not change at 10^10 or 10^11 (contracts §2.5, r5; was a low-aligned `u64`).
///
/// **No extent, and no quantisation here.** The code is the high half of the item's fixed-point
/// position ([`split32`]), which the importer already computed against the build extent — so
/// this function cannot re-quantise, and there is no second place a coordinate could be turned
/// into a cell under bounds that have drifted from the ones `MANIFEST.json` declares.
///
/// `entity_ids` is permuted identically to `items` (a companion vector, not a sort key) and
/// must be the same length.
pub fn sort_batch(items: &mut Vec<TilerItem>, entity_ids: &mut Vec<EntityId>) -> Vec<u32> {
    assert_eq!(
        items.len(),
        entity_ids.len(),
        "sort_batch: items and entity_ids must be the same length"
    );

    // Pair each item with its Morton code up front so the sort comparator and the
    // returned code vector both derive from one computation (avoids recomputing per
    // comparison, and avoids the code and the sorted item order ever disagreeing).
    let mut codes: Vec<u32> = items
        .iter()
        .map(|item| split32(item.qx, item.qy).0.raw())
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        codes[a]
            .cmp(&codes[b])
            .then_with(|| items[a].tessera_id.cmp(&items[b].tessera_id))
    });

    // Apply the permutation to `items`, `entity_ids` and `codes` in lockstep so all three
    // stay aligned with the new row order.
    let mut sorted_items = Vec::with_capacity(items.len());
    let mut sorted_entity_ids = Vec::with_capacity(entity_ids.len());
    let mut sorted_codes: Vec<u32> = Vec::with_capacity(items.len());
    for &i in &order {
        sorted_items.push(items[i].clone());
        sorted_entity_ids.push(entity_ids[i]);
        sorted_codes.push(codes[i]);
    }
    *items = sorted_items;
    *entity_ids = sorted_entity_ids;
    codes = sorted_codes;
    codes
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
    /// quantises — the tiler itself never sees a coordinate.
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
        // Two items at the identical coordinate (same Morton code) plus a third sharing the
        // leading 16 bits with one of them: must order purely by ascending `tessera_id` — the
        // tiebreak is contract (contracts §2.6 r6). Without a Morton collision this test would
        // prove nothing about the order that just changed.
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
        // Contracts §2.6 r6: `priority` is a PREFIX of `tessera_id`, so the two orders are
        // the same order. This is what licenses an implementation to compare the cheap
        // 16-bit prefix first (pipeline.rs's 12-byte RowRec does exactly that). Ids share
        // their high 16 bits (prefix ties) so the test proves something.
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

    /// The code the tiler returns for a point is the code `morton_of` gives for the coordinate
    /// that point came from. The tiler now derives it by shifting the importer's fixed point
    /// rather than by quantising, so this is the join between the two routes — if they ever
    /// disagreed, `morton.u32` would stop describing where the points actually are.
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
