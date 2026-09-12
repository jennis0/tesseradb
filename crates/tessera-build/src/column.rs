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
//! A **string** column is an entity-indexed [`MappedArray<u64>`] of words into a
//! [`MappedArena`], each word naming a record. What the word holds and how the record is framed
//! is [`RecordShape`]: a `text` column's arena is also read in its own order, so its records carry
//! an entity and a length; every other string column is reached at an entity alone, so its
//! records are the characters and the word carries both numbers. A string column is stored this
//! way when a pass reads it at an entity, and not otherwise.
//!
//! # A column no pass reads at an entity has no storage here
//!
//! A string column's characters are a share of the corpus's bytes, and placing them at an entity
//! index is a permutation of the source: at the 10⁸ PaperSeek rung one `text` column is 128 GiB
//! written to a mapping on a 47 GB box and read back at random. The arena buys random access by
//! entity, so a column nothing reaches that way pays for an answer no pass asks for. Its values
//! are spilled as record-blob extents while the join decodes them ([`crate::extents`],
//! `build-column-extents.md`), and its slot here is [`EntityColumn::spilled`]: the length, and no
//! files at all. Not even presence bits — the join has no lane that would mark one
//! ([`ColumnStorage`]), so a bitmap beside such a column is `n/8` bytes reserved, never written,
//! and read only to answer absence.
//!
//! **The readers decide which columns those are**, and the rule is stated once, in
//! [`crate::pipeline::takes_extents`]: a string column with no value column and no `render` slot
//! is read by the record blob alone, and the blob reads an extent as readily as a column. That is
//! every bundle-wide `text` column, and every `keyword` or `utf8` column declared with neither
//! `index` nor `render`.
//!
//! What is left below about the arena is about the string columns that keep one. A group-scoped
//! `text` column is one of them: it has no blob row (`views.md` §5), so the scoped pass builds it
//! as an ordinary string column and indexes it from the arena.
//!
//! # The arena is filled in arrival order
//!
//! A value is appended where the arena has got to, which is the order the source file yields
//! them in — one pass, and the cheapest fill there is.
//!
//! From 2026-09-03 to 2026-09-04 a second fill existed: two passes over the source's string
//! columns, pass one keeping each entity's length and a prefix sum then laying every record out
//! in entity order, so the passes after the join that read the arena by entity — the record
//! blob's merge, the keyword dictionary — walked it sequentially rather than at one random read a
//! value. `--arena-order` chose between the two. It was withdrawn once the join's own scatter
//! (below) turned out to be the term that mattered: `probes/2026-09-03-entity-ordered-arena/`
//! measured the record blob finishing at its uncapped wall in *both* orders once the scatter into
//! the entity-major columns ascends, so the second decode bought nothing a sorted write had not
//! already bought, and it governed only `keyword`/`utf8` columns even before that (`text` columns
//! take extents — `build-column-extents.md` §6) and no corpus in the ladder ever reached its share.
//!
//! **The record names its own entity, so a reader that wants every value can walk the arena
//! instead of the column.** Entity order is signature-then-Morton order and arrival order is the
//! source file's, so under an arrival-order arena a pass that walked entities and reached the
//! arena by offset made one random access per value. That is free while the arena fits in memory
//! and ruinous when it does not: the text index over 1.02×10⁸ abstracts, when prose was still an
//! arena, ran for over four hours at ~480 major faults a second and did not finish
//! (`probes/2026-09-03-text-arena-streaming/`). [`EntityColumn::for_each_record_in`] is the walk
//! that replaced it, over the contiguous byte ranges [`EntityColumn::arena_windows`] hands out,
//! and the entity in the header is the four bytes an entity per record that buys it. A
//! group-scoped `text` column is what still reads it.
//!
//! **A `keyword` or `utf8` column is never walked, so its records carry no header at all.** Every
//! reader of one reaches it at an entity — the keyword dictionary, the filter postings, the record
//! blob — through [`StringColumn::get`], which has just read the word in `at` that led it there.
//! So the length goes in that word beside the offset and the entity is not written down twice; the
//! walk refuses a column of that shape rather than looking for a header that is not there. The
//! declared type decides which shape a column writes ([`record_shape`]), so the two cannot drift
//! apart per call site: 4 B/item of entity and 4 B a value of length, 28.0 GB and 26.9 GB across
//! the GBIF rung's two keyword columns (modelled, items × 4 B and present values × 4 B; the
//! entity term measured at 0.98 GB over 125,789,091 of them).
//!
//! A record is authoritative only while `at[entity]` still names it: a value written twice for one
//! entity leaves the first record in the arena with nothing pointing at it, so the walk checks
//! the offset back against the column before it yields a record. That check is a random read of
//! eight bytes an entity — the `at` array, three orders of magnitude smaller than the arena — and
//! it is what makes the walk yield exactly [`EntityColumn::str_at`]'s values and no others.
//!
//! # Absence
//!
//! **Absence is a presence bit and never a value**, exactly as it was on the heap. A zero-length
//! string and an absent one are different states here: the empty string is a value a corpus may
//! legitimately hold ([`ScalarValue::Null`]'s own docs give the reason), so a design in which a
//! zero length reads as absence would be silently wrong. A slot whose value is absent keeps its
//! type's zero, which is what every consumer of an absent value already reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
///
/// The counter is atomic because the segment tail builds its render columns one lane per column
/// and each lane creates its own file. Which lane draws which serial is then a scheduling detail,
/// and it is allowed to be: these names reach no artefact, and uniqueness is the whole of what is
/// asked of them.
#[derive(Debug)]
pub(crate) struct ColumnScratch {
    dir: PathBuf,
    next: AtomicU64,
}

impl ColumnScratch {
    pub(crate) fn new(dir: &Path) -> Self {
        ColumnScratch {
            dir: dir.to_path_buf(),
            next: AtomicU64::new(0),
        }
    }

    fn name(&self, kind: &str) -> String {
        let serial = self.next.fetch_add(1, Ordering::Relaxed);
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
    storage: ColumnStorage,
    len: usize,
}

/// What a column holds: its values in entity order with a presence bit beside each, or nothing.
///
/// **The presence bits are in the same variant as the values**, so a column with no values has no
/// bitmap a writer could mark. That is the shape a column [`crate::pipeline::takes_extents`]
/// routed needs: the join writes its characters out as record-blob extents and never reaches the
/// slot here, so a bitmap beside it is `n/8` bytes reserved, never written, and read only to
/// answer absence at every entity — 437 MB a column at the GBIF rung's 3,495,729,729 items
/// (modelled, items ÷ 8; measured at 0.125 B/item over 125,789,091 of them). [`Self::Empty`]
/// answers the same absence and reserves nothing.
///
/// One of these exists per declared attribute, so the 224 bytes the held variant is larger by are
/// twenty allocations in a build. Boxing it to even them up would put a pointer chase in front of
/// [`EntityColumn::is_present`], which runs once per entity per column.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum ColumnStorage {
    /// The values, and one presence bit per entity beside them.
    ByEntity {
        data: ColumnData,
        present: MappedArray<u64>,
    },
    /// No values and no presence bits. A spilled column's slot, and what [`EntityColumn::release`]
    /// leaves behind once a column has met its last reader.
    Empty,
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
/// **One entity-indexed array rather than an offset array and a length array.** Both would be
/// written at the same random entity index, so the second would double the page faults the
/// scatter takes. What the word at that index holds differs by [`RecordShape`]: the record's
/// offset where the arena is also walked in its own order, and the offset and the length together
/// where it is not.
#[derive(Debug)]
struct StringColumn {
    at: MappedArray<u64>,
    arena: MappedArena,
    /// How a record is framed, and what `at` holds, by the column's declared type.
    shape: RecordShape,
}

/// How a string column frames a record, and what its `at` word holds.
///
/// **The declared type decides it, not the caller**, so the two cannot drift apart per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordShape {
    /// **Also read in arena order.** The record is the entity, then the length, then the bytes;
    /// `at` holds its offset.
    ///
    /// The entity is in the record and not only in `at`, because that is what makes the arena
    /// readable in its own order — see the module docs. Four bytes an entity, against a walk that
    /// otherwise costs a major fault per document on any corpus larger than the box.
    Walked,
    /// **Read only through `at`.** The record is the bytes alone; `at` holds the offset and the
    /// length in one word.
    ///
    /// A `keyword` or `utf8` column is reached at an entity and nowhere else —
    /// [`EntityColumn::str_at`] from the keyword dictionary, the filter postings and the record
    /// blob — so a header on one of its records carries an entity no consumer reads and a length
    /// read only by [`StringColumn::get`], which has just read the word that led it there. Both go
    /// in that word: [`PACKED_LENGTH_BITS`] of length above [`PACKED_OFFSET_BITS`] of offset, for
    /// an arena of [`PACKED_OFFSET_LIMIT`] bytes holding values of [`PACKED_LENGTH_LIMIT`]. That
    /// is 4 B an item of entity and 4 B a value of length the arena never holds — 28.0 GB and
    /// 26.9 GB across the GBIF rung's two `keyword` columns at 3,495,729,729 items (modelled,
    /// items × 4 B and present values × 4 B). Measured on `gbif-64p`: 199.2 MB of record header
    /// off 49,801,433 present values at 25,846,007 items, and 160 MiB off both the arenas'
    /// reserved capacity and the build's peak disk, the arena growing in steps.
    ///
    /// [`EntityColumn::for_each_record_in`] is the arena walk and it refuses a column of this
    /// shape: there is no header for a scan to resynchronise on. The only column built with an
    /// arena and walked in its own order is a group-scoped `text` one, which has no blob row to be
    /// read from instead (`views.md` §5).
    Indexed,
}

/// A [`RecordShape::Walked`] record's header: the entity `u32`, then the length `u32`.
const RECORD_HEADER_WALKED: usize = 2 * std::mem::size_of::<u32>();

/// The low bits of a [`RecordShape::Indexed`] column's `at` word: where the record starts.
const PACKED_OFFSET_BITS: u32 = 40;

/// The high bits of that word: how long the record is.
const PACKED_LENGTH_BITS: u32 = u64::BITS - PACKED_OFFSET_BITS;

/// The arena an indexed column's offsets can name: 1 TiB. The largest any rung models is
/// `scientificname`'s at 123 GB, where the ladder's own largest measured arena is 128 GiB of
/// PaperSeek prose — which is `text` and keeps its header (modelled, 35.2 B/item over
/// 3,495,729,729 items; see [`crate::spill::ARENA_GROWTH_STEP`] for the per-item figure).
const PACKED_OFFSET_LIMIT: u64 = 1 << PACKED_OFFSET_BITS;

/// The longest value an indexed column can hold: 16 MiB, where a walked one holds 4 GiB. Both are
/// refused at [`StringColumn::set`] rather than truncated — a length that wrapped would serve the
/// neighbouring record's bytes.
const PACKED_LENGTH_LIMIT: usize = 1 << PACKED_LENGTH_BITS;

/// Which shape a string column of `ty` writes.
fn record_shape(ty: ScalarType) -> RecordShape {
    match ty {
        ScalarType::Text => RecordShape::Walked,
        _ => RecordShape::Indexed,
    }
}

/// The refusal every setter shares: a value whose tag is not the column's.
///
/// `#[cold]` and out of line because it is the arm no caller expects to reach, and building its
/// message is the whole of its cost — the same reasoning `tessera_store::write` records for its
/// own twin of this, measured at ~10⁹ constructions across one build's columns.
/// The set bits of a presence bitmap, ascending, **skipping an absent run 64 at a time** — see
/// [`EntityColumn::present_entities`], and the arena layout sweep, which walks the same bits while
/// the column's storage is mutably borrowed.
fn present_entities_of(words: &[u64]) -> impl Iterator<Item = usize> + '_ {
    // The word being drained, with each yielded bit cleared out of it. The trailing word's bits
    // above the column's length are never set — the mapping is zeroed and `set` is indexed by an
    // entity — so no bound test is needed per bit.
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

/// Whether `entity`'s bit is set.
fn present_bit(words: &[u64], entity: usize) -> bool {
    words[entity / 64] >> (entity % 64) & 1 == 1
}

/// Mark `entity` present.
///
/// **A free function over the bitmap rather than a method on the column**, so that it cannot be
/// called on a column that has no bitmap ([`ColumnStorage`]): a spilled column's values are its
/// extents, and a bit marked here would say the slot holds one.
fn mark_present(words: &mut MappedArray<u64>, entity: usize) {
    words.as_mut_slice()[entity / 64] |= 1u64 << (entity % 64);
}

/// Mark `entity` absent.
fn clear_present(words: &mut [u64], entity: usize) {
    words[entity / 64] &= !(1u64 << (entity % 64));
}

/// The refusal a write to a column with no storage raises.
///
/// Reached where a pass writes to a column [`crate::pipeline::takes_extents`] routed, or to one
/// already released. Both are build defects rather than corpus errors, and both would otherwise
/// put a value where no reader looks for it.
#[cold]
#[inline(never)]
fn no_slot(ty: ScalarType, name: &str) -> BuildError {
    BuildError::Invalid(format!(
        "attribute column '{name}' ({ty:?}) holds no values in entity order — its characters are \
         record-blob extents, or its last reader has run"
    ))
}

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
                            shape: record_shape(ty),
                        })
                    }
                }
            };
        }
        Ok(EntityColumn {
            ty,
            storage: ColumnStorage::ByEntity {
                data: fixed_width_columns!(arms),
                present: MappedArray::<u64>::zeroed(
                    &scratch.dir,
                    &scratch.name("present"),
                    n.div_ceil(64),
                )?,
            },
            len: n,
        })
    }

    /// A spilled column's slot: `n` entities, every one absent, and **no files at all**.
    ///
    /// A column [`crate::pipeline::takes_extents`] routed is never held in entity order
    /// (`build-column-extents.md`): the join spills it as record-blob extents in its own chunks and
    /// the record blob reads those, a `text` column's token index having read them first. What is
    /// left here is the length, so the column keeps its place in the declaration-indexed vector
    /// every later pass indexes by attribute position.
    ///
    /// **Not even a presence bitmap**, which the join has no lane to mark ([`ColumnStorage`]):
    /// [`Self::str_at`], [`Self::value_at`] and [`Self::present_entities`] answer absence at every
    /// entity from the variant.
    ///
    /// `scratch` is taken so that the two constructors read alike at their call sites and a column
    /// that later owes a file does not change its caller's signature.
    pub(crate) fn spilled(_scratch: &ColumnScratch, ty: ScalarType, n: usize) -> Result<Self> {
        Ok(EntityColumn {
            ty,
            storage: ColumnStorage::Empty,
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
        self.storage = ColumnStorage::Empty;
        self.len = 0;
    }

    pub(crate) fn set(&mut self, entity: usize, value: ScalarValue, name: &str) -> Result<()> {
        let ty = self.ty;
        let ColumnStorage::ByEntity { data, present } = &mut self.storage else {
            return Err(no_slot(ty, name));
        };
        // Absent: the zero stands, and the bit is *cleared* to say so rather than merely left
        // alone. Clearing matters where slots are reused — the staging buffer in
        // `read_attributes_by_entity` writes a fresh chunk over the last one, and a bit left set
        // by a previous row would make this row's absence read as that row's value.
        if matches!(value, ScalarValue::Null) {
            clear_present(present.as_mut_slice(), entity);
            return Ok(());
        }
        data.set(entity, value, ty, name)?;
        mark_present(present, entity);
        Ok(())
    }

    /// Write one string value without owning it — what the attribute join's move across from its
    /// staging buffer uses, where [`Self::set`] would want a `String` allocated for the moment
    /// between the two columns.
    fn set_str(&mut self, entity: usize, value: &str, name: &str) -> Result<()> {
        let ty = self.ty;
        let ColumnStorage::ByEntity { data, present } = &mut self.storage else {
            return Err(no_slot(ty, name));
        };
        data.set_str(entity, value, ty, name)?;
        mark_present(present, entity);
        Ok(())
    }

    /// What one value of this column occupies in its file, or `None` where the column is a string
    /// one or carries no values at all.
    ///
    /// **The attribute join routes on this.** A fixed-width column's values go through a
    /// `(entity, value)` partition and are written back as contiguous runs; a string column keeps
    /// its scattered offset write, the arena beside it being appended in arrival order and the
    /// route not taken at any scale where the cache would fail it.
    pub(crate) fn fixed_width(&self) -> Option<usize> {
        match &self.storage {
            ColumnStorage::ByEntity { data, .. } => data.fixed_width(),
            ColumnStorage::Empty => None,
        }
    }

    /// One row's bytes exactly as the file holds them, or `None` where the row carries no value —
    /// the payload the attribute join's partition pushes beside the entity.
    pub(crate) fn raw_at(&self, pos: usize) -> Option<&[u8]> {
        let width = self.fixed_width()?;
        if !self.is_present(pos) {
            return None;
        }
        let ColumnStorage::ByEntity { data, .. } = &self.storage else {
            return None;
        };
        data.as_bytes().get(pos * width..(pos + 1) * width)
    }

    /// Write one contiguous run of values and the presence bits beside them — the attribute
    /// join's replay of one partition bucket.
    ///
    /// `lo` is the run's first entity and must be a multiple of 64, so that the presence words
    /// this writes are whole words of the column's own bitmap and no read-modify-write is needed
    /// at either end. `values` is the run's bytes at the column's width and `present` its bits,
    /// least significant first.
    ///
    /// **This is the whole reason a value lane is a partition.** Writing each value at its entity
    /// as it was joined scattered the writes over the column's whole span: 123 GB written to grow
    /// the bundle by 34 in one stage at rung 6, every page of every value column written back and
    /// re-dirtied many times over
    /// (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §4).
    pub(crate) fn write_value_run(
        &mut self,
        lo: usize,
        values: &[u8],
        present: &[u64],
        name: &str,
    ) -> Result<()> {
        let ty = self.ty;
        let Some(width) = self.fixed_width() else {
            return Err(no_slot(ty, name));
        };
        let ColumnStorage::ByEntity {
            data,
            present: bits,
        } = &mut self.storage
        else {
            return Err(no_slot(ty, name));
        };
        debug_assert_eq!(lo % 64, 0, "a run starts at a presence-word boundary");
        let bytes = data.as_mut_bytes();
        let at = lo * width;
        bytes[at..at + values.len()].copy_from_slice(values);
        let words = bits.as_mut_slice();
        words[lo / 64..lo / 64 + present.len()].copy_from_slice(present);
        Ok(())
    }

    pub(crate) fn is_present(&self, entity: usize) -> bool {
        match &self.storage {
            ColumnStorage::ByEntity { present, .. } => present_bit(present.as_slice(), entity),
            ColumnStorage::Empty => false,
        }
    }

    pub(crate) fn value_at(&self, entity: usize) -> ScalarValue {
        match &self.storage {
            ColumnStorage::ByEntity { data, .. } if self.is_present(entity) => data.get(entity),
            _ => ScalarValue::Null,
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
        present_entities_of(match &self.storage {
            ColumnStorage::ByEntity { present, .. } => present.as_slice(),
            ColumnStorage::Empty => &[],
        })
    }

    /// Borrow a string value, or `None` where the entity has none. For the readers that only scan
    /// the column — the keyword dictionary and the text index — where [`Self::iter`]'s copy would
    /// be a second copy of every string in the corpus.
    pub(crate) fn str_at(&self, entity: usize) -> Option<&str> {
        match &self.storage {
            ColumnStorage::ByEntity { data, .. } if self.is_present(entity) => data.str_at(entity),
            _ => None,
        }
    }

    /// At most `target` contiguous arena byte ranges, together covering every record this column
    /// wrote. Empty on a non-string column or one with nothing in it.
    ///
    /// **This is what makes a whole-column string pass divisible without making it random.** The
    /// text index used to divide *entity* space, which is what a chunk's postings being ascending
    /// came free from; it divides the arena instead, and pays for that with a sort at the spill
    /// and a merge rather than a concatenation at the fan-in (`pipeline.rs`).
    pub(crate) fn arena_windows(&self, target: usize) -> Vec<(u64, u64)> {
        match &self.storage {
            ColumnStorage::ByEntity {
                data: ColumnData::Utf8(col),
                ..
            } => col.arena.windows(target),
            _ => Vec::new(),
        }
    }

    /// Every live record in one arena window, **in arena order**, as `(entity, value)`.
    ///
    /// A record is live when its entity is still present and `at` still names this record: a
    /// second value written for one entity leaves the first behind, with nothing pointing at it,
    /// and indexing that stale prose would give the entity terms it does not carry. So the yielded
    /// pairs are exactly the `(entity, str_at(entity))` of the entities whose records fall in the
    /// window — the same set the entity walk yielded, in another order.
    pub(crate) fn for_each_record_in(
        &self,
        lo: u64,
        hi: u64,
        visit: &mut dyn FnMut(usize, &str) -> Result<()>,
    ) -> Result<()> {
        let ColumnStorage::ByEntity {
            data: ColumnData::Utf8(col),
            ..
        } = &self.storage
        else {
            return Ok(());
        };
        if col.shape != RecordShape::Walked {
            return Err(BuildError::Invalid(format!(
                "a {:?} column is read at an entity, and its records carry no header to walk them \
                 by",
                self.ty
            )));
        }
        let mut window = col.arena.window(lo, hi)?;
        while window.has_more() {
            let offset = window.offset();
            let header = window.peek(RECORD_HEADER_WALKED)?;
            if header.len() < RECORD_HEADER_WALKED {
                return Err(BuildError::Invalid(format!(
                    "arena window [{lo}, {hi}) ends inside a record header at {offset}"
                )));
            }
            let entity = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
            let len = u32::from_le_bytes(header[4..8].try_into().expect("four bytes")) as usize;
            window.consume(RECORD_HEADER_WALKED);
            let body = window.peek(len)?;
            if body.len() < len {
                return Err(BuildError::Invalid(format!(
                    "arena window [{lo}, {hi}) ends inside the {len}-byte value at {offset}"
                )));
            }
            let live = entity < self.len
                && self.is_present(entity)
                && col.at.as_slice()[entity] == offset;
            if live {
                let value = std::str::from_utf8(&body[..len]).map_err(|e| {
                    BuildError::Invalid(format!(
                        "arena record at {offset} is not UTF-8 ({e}) — the arena holds only bytes \
                         written from a &str, so this is a torn file rather than a corpus value"
                    ))
                })?;
                visit(entity, value)?;
            }
            window.consume(len);
        }
        Ok(())
    }

    /// Give the arena's pages back to the kernel before a streaming walk over it — see
    /// [`crate::spill::MappedArena::unmap_pages`].
    pub(crate) fn unmap_arena_pages(&self) {
        if let ColumnStorage::ByEntity {
            data: ColumnData::Utf8(col),
            ..
        } = &self.storage
        {
            col.arena.unmap_pages();
        }
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
        // A staging buffer is always held in entity order: the join stages every source column
        // that way and routes only the destination (`pipeline::JoinLane`).
        let ColumnStorage::ByEntity { data, present } = &mut src.storage else {
            return Err(no_slot(src.ty, name));
        };
        if !present_bit(present.as_slice(), pos) {
            return Ok(());
        }
        clear_present(present.as_mut_slice(), pos);
        match data.str_at(pos) {
            Some(text) => self.set_str(entity, text, name),
            None => self.set(entity, data.get(pos), name),
        }
    }

    /// Forget a staging column's arena, keeping its file. The attribute join reuses one staging
    /// buffer per chunk and every string in it has been moved across by the time a chunk ends, so
    /// without this the buffer's arena would grow to the whole source's payload.
    pub(crate) fn reset_staging(&mut self) {
        if let ColumnStorage::ByEntity {
            data: ColumnData::Utf8(strings),
            ..
        } = &mut self.storage
        {
            strings.arena.reset();
        }
    }
}

impl ColumnData {
    /// What one value occupies, or `None` for the string member, whose values are in an arena.
    fn fixed_width(&self) -> Option<usize> {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnData::$v(_) => Some(std::mem::size_of::<$t>()),)*
                    // A byte a row in the file, packed to a bit only on the way into Arrow.
                    ColumnData::Bool(_) => Some(1),
                    ColumnData::Utf8(_) => None,
                }
            };
        }
        fixed_width_columns!(arms)
    }

    /// The values as the file holds them. Empty for the string member.
    fn as_bytes(&self) -> &[u8] {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnData::$v(col) => col.as_bytes(),)*
                    ColumnData::Bool(col) => col.as_bytes(),
                    ColumnData::Utf8(_) => &[],
                }
            };
        }
        fixed_width_columns!(arms)
    }

    /// [`Self::as_bytes`], writable.
    fn as_mut_bytes(&mut self) -> &mut [u8] {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnData::$v(col) => col.as_mut_bytes(),)*
                    ColumnData::Bool(col) => col.as_mut_bytes(),
                    ColumnData::Utf8(_) => &mut [],
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
    fn set(&mut self, entity: usize, value: &str) -> Result<()> {
        match self.shape {
            // Header and bytes in one write, so a value is never split across a growth and the
            // arena's own record marks land where a record starts.
            RecordShape::Walked => {
                let len = u32::try_from(value.len()).map_err(|_| {
                    BuildError::Invalid(format!(
                        "a string value of {} bytes exceeds the u32 length an arena record's \
                         header carries",
                        value.len()
                    ))
                })?;
                let tag = u32::try_from(entity).map_err(|_| {
                    BuildError::Invalid(format!(
                        "entity {entity} exceeds the u32 an arena record's header carries"
                    ))
                })?;
                let mut record = Vec::with_capacity(RECORD_HEADER_WALKED + value.len());
                record.extend_from_slice(&tag.to_le_bytes());
                record.extend_from_slice(&len.to_le_bytes());
                record.extend_from_slice(value.as_bytes());
                let offset = self.arena.append(&record)?;
                self.at.as_mut_slice()[entity] = offset;
            }
            // The characters alone, and the two numbers a reader needs in the word it already
            // reads to find them.
            RecordShape::Indexed => {
                if value.len() >= PACKED_LENGTH_LIMIT {
                    return Err(BuildError::Invalid(format!(
                        "a string value of {} bytes exceeds the {PACKED_LENGTH_LIMIT}-byte length \
                         an entity-indexed column packs beside its offset",
                        value.len()
                    )));
                }
                let offset = self.arena.append(value.as_bytes())?;
                if offset >= PACKED_OFFSET_LIMIT {
                    return Err(BuildError::Invalid(format!(
                        "an entity-indexed column's arena reached {offset} bytes, past the \
                         {PACKED_OFFSET_LIMIT} its offsets can name"
                    )));
                }
                self.at.as_mut_slice()[entity] =
                    offset | ((value.len() as u64) << PACKED_OFFSET_BITS);
            }
        }
        Ok(())
    }

    fn get(&self, entity: usize) -> &str {
        let word = self.at.as_slice()[entity];
        let (offset, len) = match self.shape {
            RecordShape::Walked => {
                let len = u32::from_le_bytes(
                    self.arena
                        .bytes(word + 4, 4)
                        .try_into()
                        .expect("a four-byte slice is four bytes"),
                ) as usize;
                (word + RECORD_HEADER_WALKED as u64, len)
            }
            RecordShape::Indexed => (
                word & (PACKED_OFFSET_LIMIT - 1),
                (word >> PACKED_OFFSET_BITS) as usize,
            ),
        };
        // Checked rather than `from_utf8_unchecked`: the bytes came from a `&str` and cannot be
        // anything else, so the scan costs a second over a corpus's prose and buys a loud failure
        // where a torn mapping or a wrong offset would otherwise be a silently wrong value.
        std::str::from_utf8(self.arena.bytes(offset, len))
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

    /// The arena windows partition the column's records and nothing is yielded twice.
    ///
    /// **This is the property the text index's merge rests on.** A record yielded by two windows
    /// would be a repeated entity in one term's postings, and one yielded by none would be a
    /// document with no terms — a wrong answer with no crash behind it either way. So the
    /// assertion is over the concatenation of every window at several window counts, against the
    /// entity walk that the arena walk replaced, and it includes the count that exceeds the marks
    /// the arena took.
    #[test]
    fn the_arena_windows_partition_the_records_at_any_count() {
        let (scratch, _dir) = scratch();
        const N: usize = 4_096;
        let mut column = EntityColumn::filled(&scratch, ScalarType::Text, N).unwrap();
        // A scatter that leaves whole words empty, filled in an order that is not entity order —
        // which is what the arena's arrival order is.
        let present: Vec<usize> = (0..N)
            .filter(|e| e % 7 == 0 || (64..80).contains(e))
            .collect();
        for step in [37usize, 1] {
            for &e in present.iter().step_by(step) {
                column.set(e, ScalarValue::Utf8(format!("value {e}")), "t").unwrap();
            }
        }
        let expected: Vec<(usize, String)> = present
            .iter()
            .map(|&e| (e, format!("value {e}")))
            .collect();
        for count in [1usize, 2, 7, 64, 4_096] {
            let windows = column.arena_windows(count);
            assert!(!windows.is_empty(), "count {count} produced no window");
            let mut walked: Vec<(usize, String)> = Vec::new();
            for (lo, hi) in windows {
                column
                    .for_each_record_in(lo, hi, &mut |entity, value| {
                        walked.push((entity, value.to_owned()));
                        Ok(())
                    })
                    .unwrap();
            }
            walked.sort_unstable();
            assert_eq!(
                walked, expected,
                "window count {count} did not partition the records"
            );
        }
    }

    /// A value larger than the window's read buffer, and one that straddles its boundary.
    ///
    /// **The window reads through the descriptor into a fixed buffer**, so a record longer than
    /// that buffer is the one shape the walk cannot take a shortcut on: it has to compact what it
    /// holds, grow, and refill. A corpus of abstracts never reaches it and a corpus with one long
    /// document does, which is exactly the kind of value that reaches a build and not a fixture.
    #[test]
    fn a_value_larger_than_the_window_buffer_still_walks() {
        let (scratch, _dir) = scratch();
        const N: usize = 8;
        const HUGE: usize = 9 << 20;
        let mut column = EntityColumn::filled(&scratch, ScalarType::Text, N).unwrap();
        for e in 0..N {
            let len = if e == 3 { HUGE } else { 5 };
            column
                .set(e, ScalarValue::Utf8("x".repeat(len)), "t")
                .unwrap();
        }
        let mut walked: Vec<(usize, usize)> = Vec::new();
        for (lo, hi) in column.arena_windows(4) {
            column
                .for_each_record_in(lo, hi, &mut |entity, value| {
                    walked.push((entity, value.len()));
                    Ok(())
                })
                .unwrap();
        }
        walked.sort_unstable();
        let expected: Vec<(usize, usize)> =
            (0..N).map(|e| (e, if e == 3 { HUGE } else { 5 })).collect();
        assert_eq!(walked, expected);
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

    /// **The render placeholder is exactly the zero a fresh mapping reads as**, for every type the
    /// hot column can hold.
    ///
    /// The segment's row-order tail relies on it: an absent value is left alone rather than
    /// written, because the column is non-nullable on the wire (contracts R4) and the byte a
    /// non-nullable column must carry for an absent value is
    /// [`ScalarValue::or_render_placeholder`]'s. If the two ever parted, every absent slot of
    /// every rendered column would carry a different value with no other symptom — the presence
    /// bitmap beside it would still say *nothing here*, and the bundle would still verify.
    #[test]
    fn the_render_placeholder_is_the_zero_a_mapping_reads_as() {
        let (scratch, _dir) = scratch();
        // Every renderable type: the string family is refused `render` at the declaration and
        // `utf8` is not declarable at all, so the tail never holds one.
        let types = [
            ScalarType::Bool,
            ScalarType::U8,
            ScalarType::U16,
            ScalarType::U32,
            ScalarType::U64,
            ScalarType::I8,
            ScalarType::I16,
            ScalarType::I32,
            ScalarType::I64,
            ScalarType::F32,
            ScalarType::F64,
            ScalarType::TimestampUs,
        ];
        for ty in types {
            let column = EntityColumn::filled(&scratch, ty, 1).unwrap();
            assert!(!column.is_present(0), "{ty:?}: a fresh slot is absent");
            let ColumnStorage::ByEntity { data, .. } = &column.storage else {
                panic!("{ty:?}: a filled column holds its values in entity order");
            };
            assert_eq!(
                data.get(0),
                ScalarValue::Null.or_render_placeholder(ty),
                "{ty:?}: the zero a mapping reads as is not the render placeholder"
            );
        }
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

    /// **A spilled column reserves nothing and answers absent at every entity.**
    ///
    /// The bitmap it used to carry was `n/8` bytes of `posix_fallocate`d blocks with no writer —
    /// the join's extent lane never reaches the slot — and no reader but the three answers below.
    /// Both halves are asserted together: the files, because reserved blocks are what the change
    /// is for, and the answers, because a caller that stopped seeing absence would be reading a
    /// column that is not there.
    #[test]
    fn a_spilled_column_reserves_no_blocks_and_reads_absent() {
        for ty in [ScalarType::Text, ScalarType::Keyword, ScalarType::Utf8] {
            let dir = tempfile::TempDir::new().unwrap();
            let scratch = ColumnScratch::new(dir.path());
            let column = EntityColumn::spilled(&scratch, ty, 1_000_000).unwrap();
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                0,
                "{ty:?}: a spilled column owns no file"
            );
            assert_eq!(column.len(), 1_000_000, "{ty:?}: the length is kept");
            assert!(!column.is_present(0), "{ty:?}");
            assert!(!column.is_present(999_999), "{ty:?}");
            assert_eq!(column.str_at(0), None, "{ty:?}");
            assert_eq!(column.value_at(0), ScalarValue::Null, "{ty:?}");
            assert_eq!(column.present_entities().next(), None, "{ty:?}");
            assert_eq!(column.arena_windows(8), Vec::new(), "{ty:?}");
        }
    }

    /// **A write to a spilled column is refused, not dropped.** Its values are its extents, so a
    /// value placed here would be stored where no reader looks for it.
    #[test]
    fn a_spilled_column_refuses_a_write() {
        let (scratch, _dir) = scratch();
        let mut column = EntityColumn::spilled(&scratch, ScalarType::Keyword, 8).unwrap();
        let refused = column.set(0, ScalarValue::Utf8("key".into()), "k");
        assert!(refused.is_err(), "a spilled column took a value");
        assert!(!column.is_present(0));
        let mut staged = EntityColumn::filled(&scratch, ScalarType::Keyword, 1).unwrap();
        staged.set(0, ScalarValue::Utf8("key".into()), "k").unwrap();
        assert!(
            column.take_from(0, &mut staged, 0, "k").is_err(),
            "a spilled column took a value from a staging buffer"
        );
    }

    /// **Both record shapes round-trip, in an arena filled the way the join fills one.**
    ///
    /// The values are written out of entity order, an entity is written twice, and one value is
    /// empty — which is the shape that has no bytes in the arena at all and still is not absence.
    /// Asserted for `keyword`, whose length rides in the offset word, and for `text`, whose
    /// records carry a header a walk reads.
    #[test]
    fn a_value_reads_back_whatever_shape_its_record_takes() {
        for ty in [ScalarType::Keyword, ScalarType::Utf8, ScalarType::Text] {
            let (scratch, _dir) = scratch();
            let mut column = EntityColumn::filled(&scratch, ty, 6).unwrap();
            for (entity, value) in [(5, "last"), (0, ""), (3, "middle"), (0, "first")] {
                column
                    .set(entity, ScalarValue::Utf8(value.into()), "v")
                    .unwrap();
            }
            assert_eq!(column.str_at(0), Some("first"), "{ty:?}: rewritten");
            assert_eq!(column.str_at(1), None, "{ty:?}: never written");
            assert_eq!(column.str_at(3), Some("middle"), "{ty:?}");
            assert_eq!(column.str_at(5), Some("last"), "{ty:?}");
            assert_eq!(
                column.present_entities().collect::<Vec<_>>(),
                vec![0, 3, 5],
                "{ty:?}"
            );
        }
    }

    /// **A value longer than the packed length is refused rather than truncated.** A length that
    /// wrapped would name a prefix of the record and serve the neighbour's bytes after it, with
    /// nothing to say so.
    #[test]
    fn a_value_past_the_packed_length_is_refused() {
        let (scratch, _dir) = scratch();
        let mut column = EntityColumn::filled(&scratch, ScalarType::Keyword, 1).unwrap();
        let long = "x".repeat(PACKED_LENGTH_LIMIT);
        let refused = column.set(0, ScalarValue::Utf8(long), "k");
        assert!(refused.is_err(), "a 16 MiB value was packed into 24 bits");
        // One byte under the limit is a value, and reads back whole.
        let held = "y".repeat(PACKED_LENGTH_LIMIT - 1);
        column
            .set(0, ScalarValue::Utf8(held.clone()), "k")
            .unwrap();
        assert_eq!(column.str_at(0), Some(held.as_str()));
    }
}
