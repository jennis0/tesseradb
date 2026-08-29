//! The declared attribute columns in entity order — mapped, not held.
//!
//! # What this is
//!
//! One [`EntityColumn`] per declared attribute, indexed by entity: a typed array of values with a
//! presence bit beside each, filled by the attribute join and read by every stage from the layer
//! publication to the segment write. It is the shape [`crate::residency`] models and the largest
//! thing the build holds outside its batch loop.
//!
//! # Why it is on disk
//!
//! The columns are *n* values by construction — a column in entity order does not shrink when the
//! batch stride does — and their consumers want random access by entity, so there is no chunking to
//! be had. What is left is to stop the bytes being anonymous memory. Measured on the Overture
//! corpus (73,631,092 entities, 12 declared attributes, one `text` column), the columns were
//! **5.0 GB** of heap, live across five consecutive stages — the attribute tail, the layer
//! publication, the text index, the filter postings and the record blob — of which the `text`
//! column's `String` headers alone were 1.77 GB **before a character was stored**, plus one heap
//! allocation per row.
//!
//! Mapped, the same bytes are page cache: the kernel keeps what fits and evicts the rest under
//! pressure, so a corpus larger than the box gets slower rather than OOM-killed. This is
//! [`crate::spill::MappedArray`]'s argument, made for the geometry pass's scratch first and
//! extended here to every declared type.
//!
//! # The two halves
//!
//! A **fixed-width** column is a [`MappedArray`] of its own element type, and its presence bits are
//! another one. That is the whole of it.
//!
//! A **string** column — `utf8`, `keyword` and `text` alike — is an entity-indexed
//! [`MappedArray<u64>`] of offsets into a [`MappedArena`], each offset naming a `u32` length
//! followed by the bytes. The arena is appended in *arrival* order, which is what keeps the fill to
//! one pass: values are discovered in the source file's order and scattered by entity, so an
//! entity-ordered arena would need a prefix-sum pass over the lengths and a second scan of the
//! source. Nothing reads the arena in order, so its order means nothing.
//!
//! # Absence
//!
//! **Absence is a presence bit and never a value**, exactly as it was on the heap. A zero-length
//! string and an absent one are different states here: the empty string is a value a corpus may
//! legitimately hold ([`ScalarValue::Null`]'s own docs give the reason), so a design in which a
//! zero length reads as absence would be silently wrong. A slot whose value is absent keeps its
//! type's zero, which is what every consumer of an absent value already reads.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use tessera_spatial::{ScalarType, ScalarValue};

use crate::error::{BuildError, Result};
use crate::spill::{MappedArena, MappedArray};

/// The fixed-width members, each with its [`ScalarValue`] variant and its storage element.
///
/// Generated rather than written out, for the reason `tessera_store::write`'s twin of this list is:
/// eleven types across five methods is fifty near-identical arms whose only failure mode is a type
/// appearing in one of another's — a column silently taking another's width or another's values,
/// which no aggregate check sees. `Bool` and `Utf8` are written by hand: a `bool` has bit patterns
/// that are not values (so it is stored as `u8`), and a string is not a flat array of itself.
macro_rules! fixed_width_columns {
    ($mac:ident) => {
        $mac! {
            (U8, u8),
            (U16, u16),
            (U32, u32),
            (U64, u64),
            (I8, i8),
            (I16, i16),
            (I32, i32),
            (I64, i64),
            (F32, f32),
            (F64, f64),
            (TimestampUs, i64),
        }
    };
}

/// Where a build's mapped columns live, and what names they take.
///
/// A column names its own files rather than the caller doing it, because the only property the
/// names need is uniqueness within one build and the only place that can promise it is the thing
/// handing them out. The directory is the build's `.build-tmp/`, so [`crate::spill::TmpDir`] clears
/// what a killed build leaves behind exactly as it does for the spill and band files.
#[derive(Debug)]
pub(crate) struct ColumnScratch {
    dir: PathBuf,
    next: Cell<u64>,
}

impl ColumnScratch {
    pub(crate) fn new(dir: &Path) -> Self {
        ColumnScratch {
            dir: dir.to_path_buf(),
            next: Cell::new(0),
        }
    }

    fn name(&self, kind: &str) -> String {
        let serial = self.next.get();
        self.next.set(serial + 1);
        format!("column-{serial}.{kind}")
    }
}

/// One attribute's values in entity order: a typed mapped column, with presence beside it.
///
/// **This replaced a `Vec<ScalarValue>` per column and then the heap under that.** `ScalarValue`
/// carries a `Utf8(String)` variant, so every slot cost 32 bytes whatever the column declared, and
/// the typed `Vec`s that replaced it still cost the machine every byte they held for five stages.
/// See the module docs for what that measured and what it is now.
///
/// **Presence is a bit rather than a value, because a typed column has no spare one.**
/// `ScalarValue::Null` gave the old intermediate an absent representation for free; a `u8` array
/// has no `u8` to reserve. So absence is carried alongside, which is also the shape the output
/// already wanted — the presence bitmaps written beside each render column are exactly this. It
/// costs an eighth of a byte per entity: 9 MB at 7.4×10⁷, mapped like the rest.
#[derive(Debug)]
pub(crate) struct EntityColumn {
    ty: ScalarType,
    data: ColumnData,
    present: MappedArray<u64>,
    len: usize,
}

/// The typed storage behind an [`EntityColumn`].
#[derive(Debug)]
enum ColumnData {
    Bool(MappedArray<u8>),
    Utf8(StringColumn),
    U8(MappedArray<u8>),
    U16(MappedArray<u16>),
    U32(MappedArray<u32>),
    U64(MappedArray<u64>),
    I8(MappedArray<i8>),
    I16(MappedArray<i16>),
    I32(MappedArray<i32>),
    I64(MappedArray<i64>),
    F32(MappedArray<f32>),
    F64(MappedArray<f64>),
    TimestampUs(MappedArray<i64>),
}

/// A string column: where each entity's bytes are, and the arena they are in.
///
/// The offset names a `u32` little-endian length immediately followed by that many bytes — one
/// entity-indexed array rather than an offset array and a length array, because both would be
/// written at the same random entity index and the second would double the page faults the scatter
/// takes to save four bytes an entity it does not need to.
#[derive(Debug)]
struct StringColumn {
    at: MappedArray<u64>,
    arena: MappedArena,
}

/// The width prefix the arena stores before each value's bytes.
const LENGTH_PREFIX: usize = std::mem::size_of::<u32>();

/// The refusal every setter shares: a value whose tag is not the column's.
///
/// `#[cold]` and out of line because it is the arm no caller expects to reach, and building its
/// message is the whole of its cost — the same reasoning `tessera_store::write` records for its
/// own twin of this, measured at ~10⁹ constructions across one build's columns.
#[cold]
#[inline(never)]
fn tag_mismatch(expected: ScalarType, name: &str, got: &ScalarValue) -> BuildError {
    BuildError::Invalid(format!(
        "attribute column '{name}' is {expected:?}, got {got:?}"
    ))
}

impl EntityColumn {
    /// A column of `n` entities, every one absent, backed by files under `scratch`.
    pub(crate) fn filled(scratch: &ColumnScratch, ty: ScalarType, n: usize) -> Result<Self> {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match ty {
                    $(ScalarType::$v => ColumnData::$v(
                        MappedArray::<$t>::zeroed(&scratch.dir, &scratch.name("col"), n)?,
                    ),)*
                    ScalarType::Bool => ColumnData::Bool(
                        MappedArray::<u8>::zeroed(&scratch.dir, &scratch.name("col"), n)?,
                    ),
                    // A keyword and a text column both store their bytes as they stand; what the
                    // declared type changes is the index built over them, not this.
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                        ColumnData::Utf8(StringColumn {
                            at: MappedArray::<u64>::zeroed(
                                &scratch.dir,
                                &scratch.name("at"),
                                n,
                            )?,
                            arena: MappedArena::create(&scratch.dir, &scratch.name("arena"))?,
                        })
                    }
                }
            };
        }
        Ok(EntityColumn {
            ty,
            data: fixed_width_columns!(arms),
            present: MappedArray::<u64>::zeroed(
                &scratch.dir,
                &scratch.name("present"),
                n.div_ceil(64),
            )?,
            len: n,
        })
    }

    /// Collect an entity-ordered sequence into a typed column, for a caller that already holds the
    /// values in entity order rather than discovering them in file order.
    /// `ExactSizeIterator` rather than `IntoIterator`, so the length is known without collecting:
    /// buffering into a `Vec<ScalarValue>` first would rebuild, for one moment, exactly the
    /// 32-B-per-value intermediate this type exists to avoid.
    pub(crate) fn from_values<I>(
        scratch: &ColumnScratch,
        ty: ScalarType,
        values: I,
        name: &str,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = ScalarValue>,
        I::IntoIter: ExactSizeIterator,
    {
        let values = values.into_iter();
        let mut column = Self::filled(scratch, ty, values.len())?;
        for (entity, value) in values.enumerate() {
            column.set(entity, value, name)?;
        }
        Ok(column)
    }

    /// Give the column's storage back, leaving it empty.
    ///
    /// Called where a stage boundary is the last reader — the release after the record blob — and
    /// it unlinks the files as well as dropping the mappings, so the disk returns with the memory.
    pub(crate) fn release(&mut self) {
        self.data = ColumnData::empty(self.ty);
        self.present = MappedArray::empty();
        self.len = 0;
    }

    pub(crate) fn set(&mut self, entity: usize, value: ScalarValue, name: &str) -> Result<()> {
        // Absent: the zero stands, and the bit is *cleared* to say so rather than merely left
        // alone. Clearing matters where slots are reused — the staging buffer in
        // `read_attributes_by_entity` writes a fresh chunk over the last one, and a bit left set
        // by a previous row would make this row's absence read as that row's value.
        if matches!(value, ScalarValue::Null) {
            self.clear_present(entity);
            return Ok(());
        }
        self.data.set(entity, value, self.ty, name)?;
        self.mark_present(entity);
        Ok(())
    }

    /// Write one string value without owning it — what the attribute join's move across from its
    /// staging buffer uses, where [`Self::set`] would want a `String` allocated for the moment
    /// between the two columns.
    fn set_str(&mut self, entity: usize, value: &str, name: &str) -> Result<()> {
        self.data.set_str(entity, value, self.ty, name)?;
        self.mark_present(entity);
        Ok(())
    }

    fn mark_present(&mut self, entity: usize) {
        self.present.as_mut_slice()[entity / 64] |= 1u64 << (entity % 64);
    }

    fn clear_present(&mut self, entity: usize) {
        self.present.as_mut_slice()[entity / 64] &= !(1u64 << (entity % 64));
    }

    pub(crate) fn is_present(&self, entity: usize) -> bool {
        self.present.as_slice()[entity / 64] >> (entity % 64) & 1 == 1
    }

    pub(crate) fn value_at(&self, entity: usize) -> ScalarValue {
        if self.is_present(entity) {
            self.data.get(entity)
        } else {
            ScalarValue::Null
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The column in entity order. Yields **owned** values: a fixed-width value is a copy either
    /// way, and a string one is copied out of the arena for the caller that asked for a
    /// `ScalarValue` rather than a borrow.
    pub(crate) fn iter(&self) -> impl Iterator<Item = ScalarValue> + '_ {
        (0..self.len()).map(|entity| self.value_at(entity))
    }

    /// The entities that carry a value, ascending, **skipping an absent run 64 at a time**.
    ///
    /// The sweeps that walk a whole column do per-entity work only where there is a value, and
    /// presence is a bit vector: a column 84.5% absent — which one column of the campaign's corpus
    /// is — costs a shift and a branch per absent entity through [`Self::is_present`] to learn
    /// nothing, against one test per 64 here. The yielded order is ascending, which every caller
    /// relies on: the postings each builds are sorted by construction, not by a later sort.
    pub(crate) fn present_entities(&self) -> impl Iterator<Item = usize> + '_ {
        let words = self.present.as_slice();
        // The word being drained, with each yielded bit cleared out of it. The trailing word's
        // bits above the column's length are never set — the mapping is zeroed and `set` is
        // indexed by an entity — so no bound test is needed per bit.
        let mut residual = 0u64;
        let mut next_word = 0usize;
        std::iter::from_fn(move || loop {
            if residual != 0 {
                let bit = residual.trailing_zeros() as usize;
                residual &= residual - 1;
                return Some((next_word - 1) * 64 + bit);
            }
            residual = *words.get(next_word)?;
            next_word += 1;
        })
    }

    /// [`Self::present_entities`] over one contiguous entity range `[lo, hi)`, skipping an absent
    /// run 64 at a time exactly as that does.
    ///
    /// **This is what makes a column-wide pass divisible.** The text index splits entity space
    /// into chunks and indexes them in parallel, and every consumer of a chunk's output relies on
    /// the yielded order being ascending *and* on the chunks partitioning the column — so the
    /// range is masked into the boundary words rather than filtered out of the whole-column
    /// iterator, which would make each chunk cost a scan of every other chunk's presence bits.
    pub(crate) fn present_entities_in(
        &self,
        lo: usize,
        hi: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        let words = self.present.as_slice();
        let lo = lo.min(self.len());
        let hi = hi.min(self.len());
        let first_word = lo / 64;
        let end_word = hi.div_ceil(64);
        let mut next_word = first_word;
        let mut residual = 0u64;
        std::iter::from_fn(move || loop {
            if residual != 0 {
                let bit = residual.trailing_zeros() as usize;
                residual &= residual - 1;
                return Some((next_word - 1) * 64 + bit);
            }
            if next_word >= end_word {
                return None;
            }
            let mut word = words[next_word];
            // The two boundary words are the whole of the range logic: a chunk starts and ends
            // mid-word in general, and a bit outside `[lo, hi)` left set here would be indexed
            // twice — once by this chunk and once by its neighbour — which the merge would see as
            // a repeated entity in one term's postings.
            if next_word == first_word {
                word &= u64::MAX << (lo % 64);
            }
            if next_word == end_word - 1 && !hi.is_multiple_of(64) {
                word &= !(u64::MAX << (hi % 64));
            }
            residual = word;
            next_word += 1;
        })
    }

    /// Borrow a string value, or `None` where the entity has none. For the readers that only scan
    /// the column — the keyword dictionary and the text index — where [`Self::iter`]'s copy would
    /// be a second copy of every string in the corpus.
    pub(crate) fn str_at(&self, entity: usize) -> Option<&str> {
        self.is_present(entity)
            .then(|| self.data.str_at(entity))
            .flatten()
    }

    /// Move one value across from a staging column, leaving the source slot absent.
    ///
    /// The string arm copies the bytes from one arena to the other rather than going through
    /// [`Self::value_at`], whose `Utf8` arm would allocate a `String` per row for the moment
    /// between reading and writing it.
    pub(crate) fn take_from(
        &mut self,
        entity: usize,
        src: &mut EntityColumn,
        pos: usize,
        name: &str,
    ) -> Result<()> {
        if !src.is_present(pos) {
            return Ok(());
        }
        src.clear_present(pos);
        match src.data.str_at(pos) {
            Some(text) => self.set_str(entity, text, name),
            None => self.set(entity, src.data.get(pos), name),
        }
    }

    /// Forget a staging column's arena, keeping its file. The attribute join reuses one staging
    /// buffer per chunk and every string in it has been moved across by the time a chunk ends, so
    /// without this the buffer's arena would grow to the whole source's payload.
    pub(crate) fn reset_staging(&mut self) {
        if let ColumnData::Utf8(strings) = &mut self.data {
            strings.arena.reset();
        }
    }
}

impl ColumnData {
    /// The type's storage with nothing in it — no file and no mapping.
    fn empty(ty: ScalarType) -> Self {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match ty {
                    $(ScalarType::$v => ColumnData::$v(MappedArray::<$t>::empty()),)*
                    ScalarType::Bool => ColumnData::Bool(MappedArray::<u8>::empty()),
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                        ColumnData::Utf8(StringColumn::empty())
                    }
                }
            };
        }
        fixed_width_columns!(arms)
    }

    /// Write one value at `entity`, refusing a tag that is not the column's: a coerced value gives
    /// one entity another's identity, with every value present and none its own.
    fn set(&mut self, entity: usize, value: ScalarValue, ty: ScalarType, name: &str) -> Result<()> {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnData::$v(col) => match value {
                        ScalarValue::$v(x) => col.as_mut_slice()[entity] = x,
                        got => return Err(tag_mismatch(ty, name, &got)),
                    },)*
                    ColumnData::Bool(col) => match value {
                        ScalarValue::Bool(x) => col.as_mut_slice()[entity] = x as u8,
                        got => return Err(tag_mismatch(ty, name, &got)),
                    },
                    ColumnData::Utf8(col) => match value {
                        ScalarValue::Utf8(x) => col.set(entity, &x)?,
                        got => return Err(tag_mismatch(ty, name, &got)),
                    },
                }
            };
        }
        fixed_width_columns!(arms);
        Ok(())
    }

    fn set_str(&mut self, entity: usize, value: &str, ty: ScalarType, name: &str) -> Result<()> {
        match self {
            ColumnData::Utf8(col) => col.set(entity, value),
            _ => Err(tag_mismatch(ty, name, &ScalarValue::Utf8(value.to_owned()))),
        }
    }

    fn get(&self, entity: usize) -> ScalarValue {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnData::$v(col) => ScalarValue::$v(col.as_slice()[entity]),)*
                    ColumnData::Bool(col) => ScalarValue::Bool(col.as_slice()[entity] != 0),
                    ColumnData::Utf8(col) => ScalarValue::Utf8(col.get(entity).to_owned()),
                }
            };
        }
        fixed_width_columns!(arms)
    }

    /// The string at `entity`, or `None` where this column is not string-typed — which is a caller
    /// error rather than an absence: absence is the presence bit, not a variant here.
    fn str_at(&self, entity: usize) -> Option<&str> {
        match self {
            ColumnData::Utf8(col) => Some(col.get(entity)),
            _ => None,
        }
    }
}

impl StringColumn {
    fn empty() -> Self {
        StringColumn {
            at: MappedArray::empty(),
            arena: MappedArena::empty(),
        }
    }

    fn set(&mut self, entity: usize, value: &str) -> Result<()> {
        let len = u32::try_from(value.len()).map_err(|_| {
            BuildError::Invalid(format!(
                "a string value of {} bytes exceeds the arena's u32 length",
                value.len()
            ))
        })?;
        // Length and bytes in one append, so a value is never split across a growth.
        let mut record = Vec::with_capacity(LENGTH_PREFIX + value.len());
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(value.as_bytes());
        let offset = self.arena.append(&record)?;
        self.at.as_mut_slice()[entity] = offset;
        Ok(())
    }

    fn get(&self, entity: usize) -> &str {
        let offset = self.at.as_slice()[entity];
        let len = u32::from_le_bytes(
            self.arena
                .bytes(offset, LENGTH_PREFIX)
                .try_into()
                .expect("a four-byte slice is four bytes"),
        ) as usize;
        // Checked rather than `from_utf8_unchecked`: the bytes came from a `&str` and cannot be
        // anything else, so the scan costs a second over a corpus's prose and buys a loud failure
        // where a torn mapping or a wrong offset would otherwise be a silently wrong value.
        std::str::from_utf8(self.arena.bytes(offset + LENGTH_PREFIX as u64, len))
            .expect("the arena holds only bytes written from a &str")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (ColumnScratch, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        (ColumnScratch::new(dir.path()), dir)
    }

    /// The distinction the arena's shape invites a design to lose: a zero-length value is a value,
    /// and absence is the bit.
    #[test]
    fn the_empty_string_is_a_value_and_absence_is_not() {
        let (scratch, _dir) = scratch();
        let mut column = EntityColumn::filled(&scratch, ScalarType::Text, 3).unwrap();
        column
            .set(0, ScalarValue::Utf8(String::new()), "t")
            .unwrap();
        column
            .set(2, ScalarValue::Utf8("value".into()), "t")
            .unwrap();
        assert_eq!(column.str_at(0), Some(""));
        assert!(column.is_present(0));
        assert_eq!(column.str_at(1), None);
        assert!(!column.is_present(1));
        assert_eq!(column.value_at(1), ScalarValue::Null);
        assert_eq!(column.str_at(2), Some("value"));
        assert_eq!(column.present_entities().collect::<Vec<_>>(), vec![0, 2]);
    }

    /// The ranges partition the column and nothing is yielded twice.
    ///
    /// A bit outside `[lo, hi)` left set by the boundary masking would be indexed by a chunk and
    /// by its neighbour both, which the text index's merge sees as a repeated entity in one term's
    /// postings — a wrong answer with no crash behind it. So the assertion is over strides that
    /// divide the 64-entity word and strides that do not, and it is over the concatenation rather
    /// than over each range alone.
    #[test]
    fn the_ranges_partition_the_column_at_any_stride() {
        let (scratch, _dir) = scratch();
        const N: usize = 300;
        let mut column = EntityColumn::filled(&scratch, ScalarType::U32, N).unwrap();
        // A scatter that leaves whole words empty (a run of absences the walk skips 64 at a time)
        // and words partly filled either side of an unaligned boundary.
        let present: Vec<usize> = (0..N)
            .filter(|e| e % 7 == 0 || (64..80).contains(e))
            .collect();
        for &e in &present {
            column.set(e, ScalarValue::U32(e as u32), "t").unwrap();
        }
        assert_eq!(column.present_entities().collect::<Vec<_>>(), present);
        for stride in [1usize, 7, 63, 64, 65, 128, 299, N, N + 11] {
            let walked: Vec<usize> = (0..N)
                .step_by(stride)
                .flat_map(|lo| column.present_entities_in(lo, lo + stride))
                .collect();
            assert_eq!(
                walked, present,
                "stride {stride} did not partition the column"
            );
        }
        // Past the end, and empty: both are no entities rather than a panic on the trailing word.
        assert_eq!(column.present_entities_in(N, N + 64).count(), 0);
        assert_eq!(column.present_entities_in(70, 70).count(), 0);
    }

    /// Values arrive by random entity index and the arena grows past its first mapping while they
    /// do — every earlier value must still read back.
    #[test]
    fn scattered_writes_survive_the_arena_growing() {
        let (scratch, _dir) = scratch();
        const N: usize = 4_096;
        let mut column = EntityColumn::filled(&scratch, ScalarType::Utf8, N).unwrap();
        // A stride coprime with N visits every entity exactly once, in no ascending order.
        let text = |e: usize| format!("{e}:{}", "x".repeat(e % 700));
        for step in 0..N {
            let entity = (step * 1_237) % N;
            column
                .set(entity, ScalarValue::Utf8(text(entity)), "t")
                .unwrap();
        }
        for entity in 0..N {
            assert_eq!(column.str_at(entity), Some(text(entity).as_str()));
        }
    }

    /// Every declared type round-trips its own value and refuses another's.
    #[test]
    fn each_type_round_trips_and_refuses_a_foreign_tag() {
        let (scratch, _dir) = scratch();
        let cases = [
            (ScalarType::Bool, ScalarValue::Bool(true)),
            (ScalarType::U8, ScalarValue::U8(7)),
            (ScalarType::U16, ScalarValue::U16(70)),
            (ScalarType::U32, ScalarValue::U32(700)),
            (ScalarType::U64, ScalarValue::U64(7_000)),
            (ScalarType::I8, ScalarValue::I8(-7)),
            (ScalarType::I16, ScalarValue::I16(-70)),
            (ScalarType::I32, ScalarValue::I32(-700)),
            (ScalarType::I64, ScalarValue::I64(-7_000)),
            (ScalarType::F32, ScalarValue::F32(0.5)),
            (ScalarType::F64, ScalarValue::F64(-0.25)),
            (ScalarType::TimestampUs, ScalarValue::TimestampUs(1_700)),
            (ScalarType::Keyword, ScalarValue::Utf8("k".into())),
        ];
        for (ty, value) in cases {
            let mut column = EntityColumn::filled(&scratch, ty, 2).unwrap();
            column.set(1, value.clone(), "c").unwrap();
            assert_eq!(column.value_at(1), value, "{ty:?} did not round-trip");
            assert_eq!(column.value_at(0), ScalarValue::Null);
            let wrong = if matches!(value, ScalarValue::U64(_)) {
                ScalarValue::U32(1)
            } else {
                ScalarValue::U64(1)
            };
            let error = column
                .set(0, wrong, "c")
                .expect_err("a foreign tag is refused");
            assert!(error.to_string().contains("attribute column 'c'"));
            // Refused, and nothing recorded: the entity stays absent.
            assert!(!column.is_present(0));
        }
    }

    /// The staging buffer's move across, including the arena reset a reused chunk depends on.
    #[test]
    fn take_from_moves_the_value_and_leaves_the_source_absent() {
        let (scratch, _dir) = scratch();
        let mut staged = EntityColumn::filled(&scratch, ScalarType::Text, 2).unwrap();
        let mut by_entity = EntityColumn::filled(&scratch, ScalarType::Text, 4).unwrap();
        for chunk in 0..3 {
            staged
                .set(0, ScalarValue::Utf8(format!("a{chunk}")), "t")
                .unwrap();
            staged.set(1, ScalarValue::Null, "t").unwrap();
            by_entity.take_from(chunk, &mut staged, 0, "t").unwrap();
            by_entity.take_from(3, &mut staged, 1, "t").unwrap();
            assert!(!staged.is_present(0), "the source slot is left absent");
            staged.reset_staging();
        }
        for chunk in 0..3 {
            assert_eq!(by_entity.str_at(chunk), Some(format!("a{chunk}").as_str()));
        }
        assert_eq!(by_entity.str_at(3), None);
    }

    /// A released column holds nothing and unlinks what it held.
    #[test]
    fn release_gives_the_files_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let scratch = ColumnScratch::new(dir.path());
        let mut column = EntityColumn::filled(&scratch, ScalarType::Text, 64).unwrap();
        column
            .set(0, ScalarValue::Utf8("held".into()), "t")
            .unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().count() > 0);
        column.release();
        assert_eq!(column.len(), 0);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "release unlinks the column's files"
        );
    }

    /// A column of nothing costs no file at all, which is what a schema-less build and a released
    /// column both want.
    #[test]
    fn a_zero_length_column_holds_no_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let scratch = ColumnScratch::new(dir.path());
        let column = EntityColumn::filled(&scratch, ScalarType::U32, 0).unwrap();
        assert_eq!(column.len(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
