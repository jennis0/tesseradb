//! The value column: a filterable attribute's values in entity order, and the masked scan over them.
//!
//! **This is the artefact of record** (`filter-index.md` §2.1). Every accelerator — a category's
//! per-value postings today, anything else later — is derived from it and rebuilt at the fold, which
//! is what keeps a filter value out of the ordinal-stability rules the term dictionary lives under.
//!
//! # Why the scan carries no timing channel
//!
//! A predicate is evaluated by walking the **candidate mask** and testing the values it selects, so
//! the work is a function of `(candidate, column)` and never of the value being sought. A value the
//! principal cannot see costs exactly what a value that does not exist costs, because the same bytes
//! are read either way. per-point-attributes §3.8 requires those to be indistinguishable *in work*
//! and not merely in outcome, and no design that evaluated an operand before masking it could
//! deliver that (`filter-surface.md` §2.1).
//!
//! This is the reason the loop is written candidate-first rather than column-first. A column-first
//! scan with an early exit — "stop once every candidate is accounted for" — would be faster on some
//! inputs and would make the running time a function of where the matches lie, which is the
//! property this construction exists to deny.
//!
//! # Addressing, and why it is measured rather than chosen
//!
//! Where every entity carries a value, the entity id *is* the array index and no addressing
//! structure is stored. Where presence is partial — the ordinary case once commit windows interleave
//! slices, which makes an entity range ascending-*with-holes* (write-path §4.2) — a Roaring presence
//! bitmap accompanies a compact value array, the *k*-th set bit's value at slot *k*.
//!
//! Two layouts were measured and refused (`probes/2026-08-08-filter-layout/`). An explicit
//! `(entity_id, value)` pair column is never optimal on either axis at any scale or presence shape:
//! it costs 4 B/entity *per column*, and the ids must be read to know what is in them. A run table
//! `(start, len, base_rank)` is the smallest structure of all — 12 bytes for a 10⁹ column — and
//! collapses to 2.9–10.2 s on a broad candidate, because rank becomes a binary search per candidate
//! entity. **Neither is a tuning choice a later reader should revisit from the storage column
//! alone.**

use std::io;
use std::path::Path;

use croaring::{Bitmap, Portable};
use tessera_types::AttrLocalId;

/// A column's values, at the declared width.
///
/// The width is the point, not a convenience: a hot column is priced at 0.93 GiB per byte per row
/// per 10⁹ items, and the entity-space column is priced the same way — 1 GB per byte of width at
/// 10⁹, per declared column (Appendix A). Widening a category from `u8` to `u32` to avoid a match
/// arm here would cost 3 GB.
#[derive(Debug, Clone)]
pub enum Codes {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    /// UTF-8 values, concatenated, with `offsets[k]..offsets[k+1]` delimiting slot `k`.
    ///
    /// **A string column carries no dictionary and no index, and that is the design rather than a
    /// stage it has not reached** (`filter-index.md` §2.3). A dictionary exists to give a value an
    /// integer identity so an inverted index can key on it; nothing here is keyed on a value, so
    /// there is nothing to intern. Equality, prefix and substring are all the same masked scan with
    /// a different comparison, and the FST a prefix walk would need exists to *order* distinct
    /// values, which only matters when the values are being looked up rather than tested.
    ///
    /// Interning would also not be free of consequence: it is what made a value's identity durable,
    /// and a durable per-value identity is what the C11 ordinal hazard lives in.
    Text {
        bytes: Vec<u8>,
        offsets: Vec<u32>,
    },
}

impl Codes {
    fn len(&self) -> usize {
        match self {
            Codes::U8(v) => v.len(),
            Codes::U16(v) => v.len(),
            Codes::U32(v) => v.len(),
            Codes::Text { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }

    /// The UTF-8 value at `slot`, for a text column. `None` for a numeric one.
    #[inline]
    fn text_at(&self, slot: usize) -> Option<&str> {
        match self {
            Codes::Text { bytes, offsets } => {
                let lo = offsets[slot] as usize;
                let hi = offsets[slot + 1] as usize;
                // Validated once at open (`read_values`), so the slice is known UTF-8 and the
                // unchecked conversion would be sound — but the checked one costs a length-
                // proportional scan only on invalid input, and this is a request path where a
                // corrupted file must fail closed rather than reinterpret bytes.
                std::str::from_utf8(&bytes[lo..hi]).ok()
            }
            _ => None,
        }
    }

    /// Build a text column from values in slot order.
    pub fn text(values: impl IntoIterator<Item = String>) -> Codes {
        let mut bytes = Vec::new();
        let mut offsets = vec![0u32];
        for v in values {
            bytes.extend_from_slice(v.as_bytes());
            offsets.push(bytes.len() as u32);
        }
        Codes::Text { bytes, offsets }
    }

    #[inline]
    fn at(&self, slot: usize) -> u32 {
        match self {
            Codes::U8(v) => v[slot] as u32,
            Codes::U16(v) => v[slot] as u32,
            Codes::U32(v) => v[slot],
            // A text column has no code. Callers reach it through `text_at`; this arm exists so
            // that a numeric predicate applied to a text column matches nothing rather than
            // panicking or, worse, comparing an offset to a code.
            Codes::Text { .. } => u32::MAX,
        }
    }
}

/// One filterable column: its values in entity order, and how an entity id reaches one.
#[derive(Debug)]
pub struct ValueColumn {
    codes: Codes,
    /// Present entities. `None` means every entity in `0..codes.len()` carries a value, and the
    /// entity id is the slot — the case that costs nothing to address.
    presence: Option<Bitmap>,
}

impl ValueColumn {
    /// A column every entity carries a value in.
    pub fn universal(codes: Codes) -> Self {
        ValueColumn {
            codes,
            presence: None,
        }
    }

    /// A column only some entities carry a value in. `presence` must have exactly one set bit per
    /// value, ascending — checked, not trusted: a mismatch would silently pair every entity after
    /// the discrepancy with another entity's value, which is a disclosure with no symptom.
    pub fn partial(codes: Codes, presence: Bitmap) -> io::Result<Self> {
        if presence.cardinality() != codes.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "value column: presence has {} entities but {} values were supplied",
                    presence.cardinality(),
                    codes.len()
                ),
            ));
        }
        Ok(ValueColumn {
            codes,
            presence: Some(presence),
        })
    }

    /// Walk the candidate, resolving each entity to its slot, and keep the entities whose value
    /// satisfies `keep`.
    ///
    /// **Every predicate goes through this one loop**, which is what makes the timing property a
    /// property of the module rather than of each function: the traversal is a function of
    /// `(candidate, presence)` alone, and `keep` sees a slot only after the traversal has already
    /// decided to visit it. A predicate cannot skip work no matter what it is testing for, so
    /// adding a family adds a comparison and cannot add a channel.
    #[inline]
    fn walk(&self, candidate: &Bitmap, mut keep: impl FnMut(&Codes, usize) -> bool) -> Bitmap {
        let mut hits: Vec<u32> = Vec::new();
        match &self.presence {
            // The entity id is the slot. One direct index per candidate entity.
            None => {
                let bound = self.codes.len();
                for e in candidate.iter() {
                    let slot = e as usize;
                    if slot < bound && keep(&self.codes, slot) {
                        hits.push(e);
                    }
                }
            }
            // Container arithmetic first, so blocks the candidate does not touch are never visited,
            // then one lockstep walk to turn entity ids into slots. The walk is O(present) rather
            // than O(hits), which is why the universal case above does not use this path even
            // though it would be correct: measured, it costs 1,078 ms against 28.7 ms at 10⁹.
            Some(presence) => {
                let live = candidate.and(presence);
                let mut slot: usize = 0;
                let mut it = presence.iter();
                let mut cur = it.next();
                for e in live.iter() {
                    while let Some(p) = cur {
                        if p < e {
                            slot += 1;
                            cur = it.next();
                        } else {
                            break;
                        }
                    }
                    if keep(&self.codes, slot) {
                        hits.push(e);
                    }
                }
            }
        }
        let mut out = Bitmap::new();
        out.add_many(&hits);
        out
    }

    /// Entities whose UTF-8 value equals `needle`, restricted to `candidate`.
    ///
    /// A string column needs no dictionary to answer this: the comparison is against the stored
    /// bytes (`Codes::Text`).
    pub fn scan_text_eq(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot) == Some(needle)
        })
    }

    /// Entities whose UTF-8 value starts with `prefix`, restricted to `candidate`.
    ///
    /// The FST an inverted design needed here existed to walk *distinct values in order*, which is
    /// only necessary when a prefix has to be turned into a set of value identifiers to look up.
    /// Testing a stored value directly needs no ordering at all.
    pub fn scan_text_prefix(&self, candidate: &Bitmap, prefix: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot).is_some_and(|v| v.starts_with(prefix))
        })
    }

    /// Entities whose UTF-8 value contains `needle`, restricted to `candidate`.
    ///
    /// **This is why substring matching stopped needing a trigram index.** A trigram conjunction
    /// returns a *superset* that must be verified against the stored value, and the cut to issue #44
    /// was made because a filter-only attribute had no route to that value. The value column is that
    /// route, and with it the verification step *is* the whole operation — there is nothing left for
    /// the trigram index to accelerate that the budget does not already afford.
    pub fn scan_text_contains(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot).is_some_and(|v| v.contains(needle))
        })
    }

    /// The UTF-8 value an entity carries, or `None` where it carries none or the column is numeric.
    pub fn text_of(&self, entity: u32) -> Option<&str> {
        self.slot_of(entity).and_then(|slot| self.codes.text_at(slot))
    }

    fn slot_of(&self, entity: u32) -> Option<usize> {
        match &self.presence {
            None => {
                let slot = entity as usize;
                (slot < self.codes.len()).then_some(slot)
            }
            Some(presence) => presence
                .contains(entity)
                .then(|| (presence.rank(entity) - 1) as usize),
        }
    }

    /// Entities carrying `value`, **restricted to `candidate`**.
    ///
    /// The candidate is `M_auth` (or a narrower composition of it), pushed in first as §8.2
    /// requires. The result is therefore already inside the authorised set — unlike
    /// [`crate::ColumnPostings::entities`], which resolves over the whole corpus and leaves the
    /// intersection to its caller.
    pub fn scan_eq(&self, candidate: &Bitmap, value: AttrLocalId) -> Bitmap {
        let wanted = value.raw();
        self.walk(candidate, |codes, slot| codes.at(slot) == wanted)
    }

    /// Entities carrying any of `values`, restricted to `candidate` — set membership, one pass.
    ///
    /// One pass rather than a union of per-value scans: the work stays a function of the candidate
    /// alone, so an IN-set naming ten invisible values costs what one naming ten visible values
    /// costs. A per-value loop would make the running time proportional to the number of *matching*
    /// values, which is the channel [`Self::scan_eq`]'s doc comment exists to deny.
    pub fn scan_in(&self, candidate: &Bitmap, values: &[AttrLocalId]) -> Bitmap {
        let mut wanted: Vec<u32> = values.iter().map(|v| v.raw()).collect();
        wanted.sort_unstable();
        wanted.dedup();
        self.walk(candidate, |codes, slot| {
            wanted.binary_search(&codes.at(slot)).is_ok()
        })
    }

    /// The value an entity carries, or `None` where it carries none.
    ///
    /// This is the direction an inverted index cannot answer, and having it is why the conformance
    /// oracle reads the artefact under test rather than a parallel relation the fold could forget
    /// (`filter-index.md` §9), and why substring matching needs no trigram index to verify against.
    pub fn value_of(&self, entity: u32) -> Option<AttrLocalId> {
        let slot = self.slot_of(entity)?;
        match &self.codes {
            Codes::Text { .. } => None,
            codes => Some(AttrLocalId::new(codes.at(slot))),
        }
    }

    /// Entities this column holds a value for. `None` presence means the dense range.
    pub fn present(&self) -> Bitmap {
        match &self.presence {
            None => {
                let mut b = Bitmap::new();
                if self.codes.len() > 0 {
                    b.add_range(0u32..self.codes.len() as u32);
                }
                b.run_optimize();
                b
            }
            Some(p) => p.clone(),
        }
    }

    /// Read a column written by [`write_value_column`].
    pub fn open(values_path: &Path, presence_path: Option<&Path>) -> io::Result<Self> {
        let codes = read_values(values_path)?;
        match presence_path {
            None => Ok(ValueColumn::universal(codes)),
            Some(path) => {
                let bytes = std::fs::read(path)?;
                let presence = Bitmap::try_deserialize::<Portable>(&bytes).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("presence bitmap at {} is not portable Roaring", path.display()),
                    )
                })?;
                ValueColumn::partial(codes, presence)
            }
        }
    }
}

fn read_values(path: &Path) -> io::Result<Codes> {
    use arrow::array::{Array, StringArray, UInt16Array, UInt32Array, UInt8Array};
    let file = std::fs::File::open(path)?;
    let reader = arrow::ipc::reader::FileReader::try_new(file, None)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut u8s: Vec<u8> = Vec::new();
    let mut u16s: Vec<u16> = Vec::new();
    let mut u32s: Vec<u32> = Vec::new();
    let mut text_bytes: Vec<u8> = Vec::new();
    let mut text_offsets: Vec<u32> = vec![0];
    let mut kind: Option<u8> = None;
    for batch in reader {
        let batch = batch.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let col = batch.column(0);
        if let Some(a) = col.as_any().downcast_ref::<UInt8Array>() {
            kind = Some(1);
            u8s.extend(a.values().iter().copied());
        } else if let Some(a) = col.as_any().downcast_ref::<UInt16Array>() {
            kind = Some(2);
            u16s.extend(a.values().iter().copied());
        } else if let Some(a) = col.as_any().downcast_ref::<UInt32Array>() {
            kind = Some(4);
            u32s.extend(a.values().iter().copied());
        } else if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
            kind = Some(8);
            // Materialised rather than borrowed from the mapped file: `Codes::Text` owns its bytes,
            // and a text column read on a request path must be valid UTF-8 *before* any predicate
            // sees it — `StringArray` has already checked that, which is why the concatenation here
            // needs no second validation pass.
            for k in 0..a.len() {
                let v = a.value(k);
                text_bytes.extend_from_slice(v.as_bytes());
                text_offsets.push(text_bytes.len() as u32);
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "value column at {}: unsupported arrow type {:?}",
                    path.display(),
                    col.data_type()
                ),
            ));
        }
    }
    match kind {
        Some(1) => Ok(Codes::U8(u8s)),
        Some(2) => Ok(Codes::U16(u16s)),
        Some(4) => Ok(Codes::U32(u32s)),
        Some(8) => Ok(Codes::Text {
            bytes: text_bytes,
            offsets: text_offsets,
        }),
        // An empty file is an empty column, not an error: a schema may declare a filterable
        // column a corpus has no values for.
        _ => Ok(Codes::U8(Vec::new())),
    }
}

/// Write a value column, and its presence bitmap where presence is partial.
///
/// `presence` is `None` when every entity in `0..codes.len()` carries a value. Writing an
/// all-ones bitmap instead would be correct and would cost the scan its fast path, so the
/// distinction is carried in the file set rather than in the bitmap's contents.
pub fn write_value_column(
    values_path: &Path,
    presence_path: &Path,
    codes: &Codes,
    presence: Option<&Bitmap>,
) -> io::Result<()> {
    use arrow::array::{ArrayRef, UInt16Array, UInt32Array, UInt8Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let (array, ty): (ArrayRef, DataType) = match codes {
        Codes::U8(v) => (Arc::new(UInt8Array::from(v.clone())), DataType::UInt8),
        Codes::U16(v) => (Arc::new(UInt16Array::from(v.clone())), DataType::UInt16),
        Codes::U32(v) => (Arc::new(UInt32Array::from(v.clone())), DataType::UInt32),
        Codes::Text { .. } => {
            let n = codes.len();
            let values: Vec<&str> = (0..n).map(|k| codes.text_at(k).unwrap_or("")).collect();
            (
                Arc::new(arrow::array::StringArray::from(values)),
                DataType::Utf8,
            )
        }
    };
    let schema = Arc::new(Schema::new(vec![Field::new("value", ty, false)]));
    let batch = RecordBatch::try_new(schema.clone(), vec![array])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let file = std::fs::File::create(values_path)?;
    let mut w = arrow::ipc::writer::FileWriter::try_new(file, &schema)
        .map_err(|e| io::Error::other(e.to_string()))?;
    w.write(&batch)
        .map_err(|e| io::Error::other(e.to_string()))?;
    w.finish()
        .map_err(|e| io::Error::other(e.to_string()))?;

    if let Some(p) = presence {
        std::fs::write(presence_path, p.serialize::<Portable>())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(all: impl IntoIterator<Item = u32>) -> Bitmap {
        let mut b = Bitmap::new();
        b.add_many(&all.into_iter().collect::<Vec<_>>());
        b
    }

    #[test]
    fn a_universal_column_scans_by_direct_index() {
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7]));
        let hits = column.scan_eq(&candidate(0..5), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2, 4]);
    }

    /// The mask goes in first: an entity carrying the value but outside the candidate is absent
    /// from the result, and never contributes work either.
    #[test]
    fn the_candidate_bounds_the_result() {
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7]));
        let hits = column.scan_eq(&candidate([0, 1, 3]), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn a_partial_column_resolves_slots_through_presence() {
        // Entities 10, 20, 30 carry values; everything else carries none.
        let column =
            ValueColumn::partial(Codes::U16(vec![100, 200, 100]), candidate([10, 20, 30])).unwrap();
        let hits = column.scan_eq(&candidate(0..40), AttrLocalId::new(100));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![10, 30]);
        assert_eq!(column.value_of(20), Some(AttrLocalId::new(200)));
        assert_eq!(column.value_of(21), None);
    }

    /// A presence bitmap that disagrees with the value count is refused rather than trusted: it
    /// would pair every entity after the discrepancy with another entity's value.
    #[test]
    fn a_presence_count_mismatch_is_refused() {
        let err = ValueColumn::partial(Codes::U8(vec![1, 2]), candidate([5, 6, 7])).unwrap_err();
        assert!(format!("{err}").contains("presence has 3 entities but 2 values"));
    }

    #[test]
    fn set_membership_is_one_pass() {
        let column = ValueColumn::universal(Codes::U16(vec![1, 2, 3, 4, 5]));
        let hits = column.scan_in(
            &candidate(0..5),
            &[AttrLocalId::new(2), AttrLocalId::new(5), AttrLocalId::new(99)],
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1, 4]);
    }

    /// A value no entity carries returns empty rather than erroring — the same answer, and the
    /// same work, as a value that does not exist in the vocabulary at all.
    #[test]
    fn an_unheld_value_is_empty_rather_than_an_error() {
        let column = ValueColumn::universal(Codes::U8(vec![1, 2, 3]));
        assert!(column
            .scan_eq(&candidate(0..3), AttrLocalId::new(42))
            .is_empty());
    }

    fn text_column(values: &[&str]) -> ValueColumn {
        ValueColumn::universal(Codes::text(values.iter().map(|s| s.to_string())))
    }

    #[test]
    fn a_string_column_answers_equality_without_a_dictionary() {
        let column = text_column(&["smith", "smythe", "smith", "jones"]);
        let hits = column.scan_text_eq(&candidate(0..4), "smith");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2]);
    }

    /// Prefix needs no FST: the FST existed to order *distinct values* so a prefix could be turned
    /// into a set of identifiers to look up, and nothing here looks a value up.
    #[test]
    fn a_string_column_answers_prefix_without_an_fst() {
        let column = text_column(&["smith", "smythe", "smote", "jones"]);
        let hits = column.scan_text_prefix(&candidate(0..4), "sm");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(column
            .scan_text_prefix(&candidate(0..4), "zz")
            .is_empty());
    }

    /// Substring was cut to #44 because a trigram conjunction returns a superset needing
    /// verification against the stored value, and a filter-only attribute had no route to that
    /// value. The value column is that route, and the verification step is the whole operation.
    #[test]
    fn a_string_column_answers_substring_without_a_trigram_index() {
        let column = text_column(&["blacksmith", "smythe", "goldsmith", "jones"]);
        let hits = column.scan_text_contains(&candidate(0..4), "smith");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2]);
    }

    /// The mask still goes in first for text, by the same shared walker every other family uses.
    #[test]
    fn the_candidate_bounds_a_text_result() {
        let column = text_column(&["a", "a", "a", "a"]);
        let hits = column.scan_text_prefix(&candidate([1, 3]), "a");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    /// A string column's absent values are its presence bitmap's business, exactly as a
    /// category's are — there is no in-band empty-string sentinel, because the empty string is a
    /// value a corpus may legitimately hold.
    #[test]
    fn an_absent_string_is_not_an_empty_string() {
        let column = ValueColumn::partial(
            Codes::text(["".to_string(), "x".to_string()]),
            candidate([5, 9]),
        )
        .unwrap();
        assert_eq!(column.text_of(5), Some(""));
        assert_eq!(column.text_of(7), None);
        // Entity 5 holds the empty string and matches an empty-prefix test; entity 7 holds no
        // value and matches nothing at all.
        let hits = column.scan_text_prefix(&candidate(0..10), "");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![5, 9]);
    }

    /// A numeric predicate against a text column matches nothing rather than comparing an offset
    /// to a code.
    #[test]
    fn a_numeric_predicate_on_text_matches_nothing() {
        let column = text_column(&["1", "2"]);
        assert!(column
            .scan_eq(&candidate(0..2), AttrLocalId::new(1))
            .is_empty());
        assert_eq!(column.value_of(0), None);
    }

    #[test]
    fn a_text_column_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let values = dir.path().join("values.arrow");
        let presence = dir.path().join("presence.roaring");
        let codes = Codes::text(["alpha".to_string(), "".to_string(), "gamma".to_string()]);
        write_value_column(&values, &presence, &codes, None).unwrap();
        let column = ValueColumn::open(&values, None).unwrap();
        assert_eq!(column.text_of(0), Some("alpha"));
        assert_eq!(column.text_of(1), Some(""));
        assert_eq!(column.text_of(2), Some("gamma"));
        assert_eq!(
            column
                .scan_text_contains(&candidate(0..3), "amm")
                .iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_column_round_trips_through_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let values = dir.path().join("values.arrow");
        let presence = dir.path().join("presence.roaring");

        let codes = Codes::U32(vec![5, 6, 7]);
        let present = candidate([2, 4, 8]);
        write_value_column(&values, &presence, &codes, Some(&present)).unwrap();
        let column = ValueColumn::open(&values, Some(&presence)).unwrap();
        assert_eq!(column.value_of(4), Some(AttrLocalId::new(6)));
        assert_eq!(column.value_of(3), None);
        assert_eq!(column.present().iter().collect::<Vec<_>>(), vec![2, 4, 8]);

        // Universal presence writes no bitmap and reads back without one.
        let values2 = dir.path().join("v2.arrow");
        write_value_column(&values2, &presence, &Codes::U8(vec![9, 8]), None).unwrap();
        let dense = ValueColumn::open(&values2, None).unwrap();
        assert_eq!(dense.value_of(0), Some(AttrLocalId::new(9)));
        assert_eq!(dense.value_of(5), None);
    }
}
