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
//! A **string** column is an entity-indexed [`MappedArray<u64>`] of offsets into a
//! [`MappedArena`], each offset naming a record: the entity, a `u32` length, then the bytes.
//! `utf8` and `keyword` columns are stored that way; a `text` column is not.
//!
//! # A text column has no storage here
//!
//! Prose is the corpus's bytes, and placing it at an entity index is a permutation of the source:
//! at the 10⁸ PaperSeek rung that is 128 GiB written to a mapping on a 47 GB box and read back at
//! random. So a `text` column's values are spilled as record-blob extents while the join decodes
//! them ([`crate::prose`], `build-prose-extents.md`), and its slot here is
//! [`EntityColumn::prose`]: the length and the presence bits, and no arena. The two consumers —
//! the text index and the record blob — read the extents.
//!
//! What is left below about the arena is about `keyword` and `utf8` columns. A group-scoped
//! `text` column is the one exception: it has no blob row (`views.md` §5), so the scoped pass
//! builds it as an ordinary string column and indexes it from the arena.
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
//! take extents — `build-prose-extents.md` §6) and no corpus in the ladder ever reached its share.
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
//! **A `keyword` or `utf8` column is never walked, so its records carry no entity.** Every reader
//! of one reaches it at an entity — the keyword dictionary, the filter postings, the record blob —
//! and the walk refuses a column of that shape rather than decoding an entity out of a length. The
//! declared type decides which header a column writes ([`record_header`]), so the two cannot drift
//! apart per call site: 4 B/item, 28.0 GB across the GBIF rung's two keyword columns (modelled,
//! items × 4 B; measured at 0.98 GB over 125,789,091 of them).
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
/// The offset names a record — an optional `u32` little-endian entity, a `u32` little-endian
/// length, then that many bytes — one entity-indexed array rather than an offset array and a
/// length array, because both would be written at the same random entity index and the second
/// would double the page faults the scatter takes to save four bytes an entity it does not need
/// to.
#[derive(Debug)]
struct StringColumn {
    at: MappedArray<u64>,
    arena: MappedArena,
    /// [`RECORD_HEADER_WALKED`] or [`RECORD_HEADER_INDEXED`], by the column's declared type.
    header: usize,
}

/// The header of a record in a column read **in arena order**: the entity, then the length.
///
/// **The entity is in the record and not only in `at`**, because that is what makes the arena
/// readable in its own order — see the module docs. Four bytes an entity, against a walk that
/// otherwise costs a major fault per document on any corpus larger than the box.
const RECORD_HEADER_WALKED: usize = 2 * std::mem::size_of::<u32>();

/// The header of a record in a column read **only through `at`**: the length alone.
///
/// A `keyword` or `utf8` column is reached at an entity and nowhere else — [`EntityColumn::str_at`]
/// from the keyword dictionary, the filter postings and the record blob — so the entity in the
/// record would be four bytes an item written for a walk no consumer makes. That is 14.0 GB per
/// such column at the GBIF rung's 3,495,729,729 items (modelled, items × 4 B).
///
/// **The type decides it, not the caller.** [`EntityColumn::for_each_record_in`] is the arena walk
/// and it refuses a column of this shape; the only column built with an arena and walked in its own
/// order is a group-scoped `text` one, which has no blob row to be read from instead
/// (`views.md` §5).
const RECORD_HEADER_INDEXED: usize = std::mem::size_of::<u32>();

/// Which header a string column of `ty` carries.
fn record_header(ty: ScalarType) -> usize {
    match ty {
        ScalarType::Text => RECORD_HEADER_WALKED,
        _ => RECORD_HEADER_INDEXED,
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
                            header: record_header(ty),
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

    /// A `text` column's slot: `n` entities, every one absent, and **no arena**.
    ///
    /// The prose of a `text` column is never held in entity order (`build-prose-extents.md`): the
    /// join spills it as record-blob extents in its own chunks, and the text index and the record
    /// blob read those. What is left here is the length and the presence bits, so the column keeps
    /// its place in the declaration-indexed vector every later pass indexes by attribute position.
    ///
    /// Nothing marks a presence bit on one of these, so [`Self::str_at`] and [`Self::value_at`]
    /// answer absence at every entity and neither reaches the empty offset array.
    pub(crate) fn prose(scratch: &ColumnScratch, ty: ScalarType, n: usize) -> Result<Self> {
        Ok(EntityColumn {
            ty,
            data: ColumnData::Utf8(StringColumn::empty()),
            present: MappedArray::<u64>::zeroed(&scratch.dir, &scratch.name("present"), n.div_ceil(64))?,
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
        present_entities_of(self.present.as_slice())
    }

    /// Borrow a string value, or `None` where the entity has none. For the readers that only scan
    /// the column — the keyword dictionary and the text index — where [`Self::iter`]'s copy would
    /// be a second copy of every string in the corpus.
    pub(crate) fn str_at(&self, entity: usize) -> Option<&str> {
        self.is_present(entity)
            .then(|| self.data.str_at(entity))
            .flatten()
    }

    /// At most `target` contiguous arena byte ranges, together covering every record this column
    /// wrote. Empty on a non-string column or one with nothing in it.
    ///
    /// **This is what makes a whole-column string pass divisible without making it random.** The
    /// text index used to divide *entity* space, which is what a chunk's postings being ascending
    /// came free from; it divides the arena instead, and pays for that with a sort at the spill
    /// and a merge rather than a concatenation at the fan-in (`pipeline.rs`).
    pub(crate) fn arena_windows(&self, target: usize) -> Vec<(u64, u64)> {
        match &self.data {
            ColumnData::Utf8(col) => col.arena.windows(target),
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
        let ColumnData::Utf8(col) = &self.data else {
            return Ok(());
        };
        if col.header != RECORD_HEADER_WALKED {
            return Err(BuildError::Invalid(format!(
                "a {:?} column is read at an entity, and its records carry a length and no entity \
                 to walk them by",
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
        if let ColumnData::Utf8(col) = &self.data {
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
        if !src.is_present(pos) {
            return Ok(());
        }
        src.clear_present(pos);
        match src.data.str_at(pos) {
            Some(text) => self.set_str(entity, text, name),
            None => self.set(entity, src.data.get(pos), name),
        }
    }

    /// The column's values as the segment writer takes them, **with no copy**: the mapping becomes
    /// the record batch's Arrow values buffer, and the file is unlinked when the batch releases it.
    ///
    /// **This is what keeps the segment's row-order tail off the heap.** The tail was eight
    /// `Vec`s built by `push` — ~2.4 GB of anonymous memory at 7.4×10⁷ rows, allocated immediately
    /// after the entity-order columns moved to `.build-tmp/`, and it does not show in today's peak
    /// only because another stage peaks higher. Filled by index into a mapping and handed over as
    /// the buffer it will be written from, the same bytes are page cache; a conversion at this
    /// boundary would give the memory back at exactly the wrong moment.
    ///
    /// **The string family is refused rather than carried.** `render` on `keyword` and on `text`
    /// is refused at the declaration and `utf8` is not declarable at all, so a string column never
    /// reaches the hot column's tail; reaching here with one is a build defect, and it says so.
    ///
    /// `bool` is the one member Arrow does not take as a flat array of itself: its values buffer
    /// is `rows` bits, so the bytes are packed into a second mapped array on the way out. That
    /// array is an eighth of the column and is unlinked with it.
    pub(crate) fn into_values(
        self,
        scratch: &ColumnScratch,
        name: &str,
    ) -> Result<tessera_store::write::ScalarColumn> {
        let EntityColumn {
            ty,
            data,
            present,
            len,
        } = self;
        // The presence bits went out as the render presence bitmap beside the column
        // (decision 0064); the tail itself is non-nullable (contracts R4) and carries no validity
        // buffer, so this file has no reader left.
        drop(present);
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match data {
                    $(ColumnData::$v(values) => {
                        tessera_store::write::ScalarColumn::of(ty, len, values.into_arrow_buffer())
                    })*
                    ColumnData::Bool(values) => {
                        let mut bits = MappedArray::<u8>::zeroed(
                            &scratch.dir,
                            &scratch.name("bits"),
                            len.div_ceil(8),
                        )?;
                        {
                            // Least significant bit first within each byte, which is Arrow's own
                            // boolean layout.
                            let packed = bits.as_mut_slice();
                            for (row, &value) in values.as_slice().iter().enumerate() {
                                if value != 0 {
                                    packed[row / 8] |= 1u8 << (row % 8);
                                }
                            }
                        }
                        drop(values);
                        tessera_store::write::ScalarColumn::of(
                            ty,
                            len,
                            bits.into_arrow_buffer(),
                        )
                    }
                    ColumnData::Utf8(_) => tessera_store::write::ScalarColumn::of(
                        ty,
                        len,
                        arrow::buffer::Buffer::from_vec(Vec::<u8>::new()),
                    ),
                }
            };
        }
        fixed_width_columns!(arms)
            .map_err(|e| BuildError::Invalid(format!("attribute column '{name}': {e}")))
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
            header: RECORD_HEADER_INDEXED,
        }
    }

    fn set(&mut self, entity: usize, value: &str) -> Result<()> {
        let len = u32::try_from(value.len()).map_err(|_| {
            BuildError::Invalid(format!(
                "a string value of {} bytes exceeds the arena's u32 length",
                value.len()
            ))
        })?;
        // Header and bytes in one write, so a value is never split across a growth and the
        // arena's own record marks land where a record starts.
        let mut record = Vec::with_capacity(self.header + value.len());
        if self.header == RECORD_HEADER_WALKED {
            let tag = u32::try_from(entity).map_err(|_| {
                BuildError::Invalid(format!(
                    "entity {entity} exceeds the u32 an arena record's header carries"
                ))
            })?;
            record.extend_from_slice(&tag.to_le_bytes());
        }
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(value.as_bytes());
        let offset = self.arena.append(&record)?;
        self.at.as_mut_slice()[entity] = offset;
        Ok(())
    }

    fn get(&self, entity: usize) -> &str {
        let offset = self.at.as_slice()[entity];
        // The length is the header's last four bytes, whichever header this column writes.
        let len = u32::from_le_bytes(
            self.arena
                .bytes(offset + (self.header - 4) as u64, 4)
                .try_into()
                .expect("a four-byte slice is four bytes"),
        ) as usize;
        // Checked rather than `from_utf8_unchecked`: the bytes came from a `&str` and cannot be
        // anything else, so the scan costs a second over a corpus's prose and buys a loud failure
        // where a torn mapping or a wrong offset would otherwise be a silently wrong value.
        std::str::from_utf8(self.arena.bytes(offset + self.header as u64, len))
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
            assert_eq!(
                column.data.get(0),
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
}
