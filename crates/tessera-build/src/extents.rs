//! A string column's values, spilled as record-blob extents while the join decodes them.
//!
//! # Which columns come here
//!
//! [`crate::pipeline::takes_extents`] decides, and it decides on the column's **readers**: a
//! string column that no pass reads at an entity index takes this route, and one that is read at
//! an entity keeps [`crate::column::EntityColumn`]'s arena. Prose is the case that forced the
//! route and it is not the only one it fits — a `keyword` column declared with neither `index`
//! nor `render` is read by the record blob alone, exactly as an unindexed `text` column is.
//!
//! # Why an arena is a permutation
//!
//! Entity ids are assigned in signature-then-Morton order and an attribute source is read in its
//! own row order, so placing a value at its entity index is a permutation of the source. A
//! fixed-width value survives that, being a slot in an array several passes then read at an
//! entity. A string column's characters are a share of the corpus's bytes, written once at random
//! into a mapping and read back at random by every consumer that walks entity space: at the 10⁸
//! PaperSeek rung one `text` column is 128 GiB of arena on a 47 GB box, and GBIF's
//! `scientificname` is 5.57 GB of arena and offsets over 125,789,091 occurrences to hand the
//! record blob 1.63 GB of extents (`probes/2026-09-10-blob-resident-strings/`).
//!
//! So a column with no reader at an entity is never permuted. Each chunk the join stages is
//! already sorted by entity — the scatter into the fixed-width columns needs that — and each
//! chunk of each such column is written here as one **blob extent**: the same three files, the
//! same block format and the same addressing as the base blob (`records-and-search.md` §3). The
//! record blob merges the extents, and a `text` column's token index reads them in block windows
//! first. Every byte moves sequentially, and the working set is one chunk plus one block per
//! extent. `docs/design/build-column-extents.md` carries the design.
//!
//! # An entity written twice
//!
//! An attribute source may carry two rows for one entity, and the last one written is the value.
//! Within a chunk that is the last row of the stable sort, collapsed before the extent is written.
//! Across chunks the earlier row is in an earlier extent, so a row of extent *i* is **live** only
//! where no later extent holds its entity. Both readers skip a row that is not, so the index holds
//! terms for exactly the values the blob holds.
//!
//! That is one question per row — *does a later extent hold this entity?* — and [`DuplicateMap`]
//! answers it for the whole column out of two bitmaps rather than one per extent. A per-extent live
//! set is the obvious construction and it does not fit: `andnot` over a run-encoded has-row bitmap
//! yields array containers at two bytes an entity, so the 988 extents each of the two string
//! columns of the 3.5×10⁹-row GBIF rung spilled came to about 7 GB of live sets a column, held
//! from the postings stage through the blob. The map is instead the entities that appear in more
//! than one extent, and for each of those the last extent that holds it — near empty for a corpus
//! with about one row an entity, and bounded, whatever the extent count, by four whole-column
//! bitmaps while a column's map is built and eight bytes a repeated entity once it is.

use std::path::{Path, PathBuf};

use croaring::Bitmap;
use tessera_filter::{Access, RecordBlob, RecordFieldRef, RecordValueRef, RECORD_BLOCK_TARGET};
use tessera_filter_write::RecordBlobWriter;

use crate::error::{BuildError, Result};

/// One extent's three files under the build's `.build-tmp/`.
#[derive(Debug, Clone)]
struct ExtentPaths {
    blocks: PathBuf,
    hasrow: PathBuf,
    directory: PathBuf,
}

/// One column's extents, in the order the join wrote them.
#[derive(Debug)]
pub(crate) struct ExtentColumn {
    /// The column's position in the declaration, which is its blob field tag.
    pub(crate) column: usize,
    name: String,
    dir: PathBuf,
    extents: Vec<ExtentPaths>,
    /// How many cascade rounds have run, so a folded extent's name cannot collide with the one it
    /// was folded from.
    folds: usize,
    /// The compressor threads every one of this column's extents is written over.
    ///
    /// **One set per column, not one per extent.** A column spills an extent per join chunk — 964
    /// of them on the widest column of the 3.5×10⁹-row GBIF rung — and a pool started inside each
    /// writer would start and stop its threads that many times. Held here and handed from one
    /// extent's writer to the next, they are started once, by the first extent that seals a block.
    /// The columns are written from one rayon lane each, so what the build holds is one pool a
    /// spilled column ([`tessera_filter_write::BlockPool`]).
    pool: Option<tessera_filter_write::BlockPool>,
}

impl ExtentColumn {
    pub(crate) fn new(dir: &Path, column: usize, name: &str) -> Self {
        ExtentColumn {
            column,
            name: name.to_string(),
            dir: dir.to_path_buf(),
            extents: Vec::new(),
            folds: 0,
            pool: None,
        }
    }

    /// Write one chunk's rows as an extent. `rows` ascends strictly in the entity, which
    /// [`RecordBlobWriter::push_row`] refuses to take on trust.
    pub(crate) fn push_extent(&mut self, rows: &[(u32, &str)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let serial = self.extents.len();
        let stem = format!("extent-{}-{serial:05}", self.column);
        let paths = ExtentPaths {
            blocks: self.dir.join(format!("{stem}.blocks.bin")),
            hasrow: self.dir.join(format!("{stem}.hasrow.roaring")),
            directory: self.dir.join(format!("{stem}.directory.arrow")),
        };
        let tag = u16::try_from(self.column).map_err(|_| {
            BuildError::Invalid(format!(
                "attribute '{}' is declared at position {}, past the u16 field-tag space",
                self.name, self.column
            ))
        })?;
        let mut writer = RecordBlobWriter::create_with(
            &paths.blocks,
            &paths.hasrow,
            &paths.directory,
            RECORD_BLOCK_TARGET,
            self.pool.take(),
        )
        .map_err(|e| BuildError::io(&paths.blocks, e))?;
        for &(entity, value) in rows {
            writer
                .push_row(
                    entity,
                    &[RecordFieldRef {
                        tag,
                        value: RecordValueRef::Utf8(value),
                    }],
                )
                .map_err(|e| BuildError::io(&paths.blocks, e))?;
        }
        self.pool = writer
            .finish()
            .map_err(|e| BuildError::io(&paths.blocks, e))?;
        self.extents.push(paths);
        Ok(())
    }

    /// Fold the extents in groups until at most `max_open` are left ([`merge_fan_in`]).
    ///
    /// A group is a contiguous run in write order and is merged by the same row merge the blob
    /// itself is written by, so an entity written twice inside one group comes out carrying the
    /// later value and the write order [`DuplicateMap`] rests on survives.
    ///
    /// **A folded group's inputs are checked before they are read**, which is the check
    /// [`RecordBlob::open_rows_only`][tessera_filter::RecordBlob::open_rows_only] gives up: the
    /// extents a fold consumes are unlinked at the end of the group and never reach
    /// [`DuplicateMap::over`], which is where the check is made for a column that does not fold.
    ///
    /// A fold buys nothing but the memory bound. Both of a column's readers are themselves
    /// merges over every extent at once, so neither of them reads fewer bytes for having had the
    /// extents folded first, and the fold is a second decompress, decode, re-encode and
    /// recompress of the whole column. Measured on the 125,789,091-row GBIF prefix, the record
    /// blob took 40.6 s over folded extents against 39.1 s over unfolded ones, with every output
    /// byte identical; at rung 6 the fold was 1,951 s of the filter-postings stage's 3,677 s,
    /// 44.8 GB written and 51.3 GB read. So `max_open` is set by the budget rather than by a
    /// constant, and a corpus whose extents fit the budget never folds.
    pub(crate) fn cascade(&mut self, max_open: usize) -> Result<()> {
        let max_open = max_open.max(2);
        while self.extents.len() > max_open {
            let groups = self.extents.len().div_ceil(max_open);
            let per_group = self.extents.len().div_ceil(groups);
            let taken = std::mem::take(&mut self.extents);
            let mut folded: Vec<ExtentPaths> = Vec::with_capacity(groups);
            for (group, extents) in taken.chunks(per_group).enumerate() {
                let stem = format!("extent-{}-fold{}-{group:05}", self.column, self.folds);
                let out = ExtentPaths {
                    blocks: self.dir.join(format!("{stem}.blocks.bin")),
                    hasrow: self.dir.join(format!("{stem}.hasrow.roaring")),
                    directory: self.dir.join(format!("{stem}.directory.arrow")),
                };
                let blobs = open_all(extents)?;
                check_hasrow_totals(extents, &blobs)?;
                let mut cursors: Vec<tessera_filter_write::BlobRows<'_>> = blobs
                    .iter()
                    .map(tessera_filter_write::BlobRows::over)
                    .collect();
                let mut sources: Vec<&mut dyn tessera_filter_write::RecordRows> = cursors
                    .iter_mut()
                    .map(|cursor| cursor as &mut dyn tessera_filter_write::RecordRows)
                    .collect();
                tessera_filter_write::merge_record_rows(
                    &mut sources,
                    &Bitmap::new(),
                    &out.blocks,
                    &out.hasrow,
                    &out.directory,
                    RECORD_BLOCK_TARGET,
                )
                .map_err(|e| BuildError::io(&out.blocks, e))?;
                drop(sources);
                drop(cursors);
                drop(blobs);
                for spent in extents {
                    for path in [&spent.blocks, &spent.hasrow, &spent.directory] {
                        let _ = std::fs::remove_file(path);
                    }
                }
                folded.push(out);
            }
            self.folds += 1;
            self.extents = folded;
        }
        Ok(())
    }

    /// Open every extent, mapped, with the column's duplicate map beside them.
    ///
    /// The blobs are opened for a sequential walk alone
    /// ([`RecordBlob::open_rows_only`][tessera_filter::RecordBlob::open_rows_only]): both readers
    /// take the rows of an extent front to back, and a blob opened that way holds no has-row bitmap
    /// on the heap. The bitmaps are read here instead, one at a time and released, to build
    /// [`DuplicateMap`] — and the one addressing check that open gives up, that an extent's
    /// directory accounts for as many rows as its has-row bitmap holds, is made there against the
    /// same bytes ([`read_checked_hasrow`], which [`ExtentColumn::cascade`] also calls for the
    /// extents a fold consumes and unlinks before this runs).
    pub(crate) fn open(&self) -> Result<OpenExtents> {
        let blobs = open_all(&self.extents)?;
        let duplicates = DuplicateMap::over(&self.extents, &blobs)?;
        Ok(OpenExtents {
            column: self.column,
            blobs,
            duplicates,
        })
    }

    /// Unlink the extents. Called once both readers are done with them; a build that fails
    /// earlier leaves them to [`crate::spill::TmpDir`] with the rest of `.build-tmp/`.
    pub(crate) fn remove(&mut self) {
        for paths in self.extents.drain(..) {
            for path in [paths.blocks, paths.hasrow, paths.directory] {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// How many extents one merge may hold open under `budget`, above which
/// [`ExtentColumn::cascade`] folds them into intermediates first.
///
/// What an open extent costs the merge **per extent** is one uncompressed block,
/// [`RECORD_BLOCK_TARGET`], and the share allowed for them is a sixty-fourth of the budget: 32 MB
/// of blocks, 128 extents, at the smallest budget a build is run under, and 1,281 at the 21.5 GB
/// rung 6 was built under, which is past the 964 extents that build's widest column spilled. The
/// extent count rises with the corpus and the budget does not, so the bound is what keeps the
/// merge's memory off the corpus; the fold below it is the cost of that bound and is paid only
/// where the bound bites.
///
/// **A block buffer is the whole of what an open extent costs.** It used to bring in a has-row
/// bitmap and a live set as well, and neither fell with the fan-in: a join chunk is a run of the
/// attribute source's own order, scattered over entity space rather than a contiguous run of it,
/// so every extent's bitmap spanned the column and folding two extents into one left the entity
/// set the same size. That is why the term is gone rather than bounded —
/// [`ExtentColumn::open`] reads the has-row files one at a time into [`DuplicateMap`], whose two
/// whole-column bitmaps are a cost of the column and not of the extent count.
pub(crate) fn merge_fan_in(budget: u64) -> usize {
    let share = budget / 64;
    usize::try_from(share / RECORD_BLOCK_TARGET as u64)
        .unwrap_or(usize::MAX)
        .max(2)
}

/// One column's extents, open, with the map that says which extent's row wins for a repeated
/// entity.
pub(crate) struct OpenExtents {
    /// The declaration position, which is the field tag the rows carry.
    pub(crate) column: usize,
    pub(crate) blobs: Vec<RecordBlob>,
    pub(crate) duplicates: DuplicateMap,
}

/// Which extent's row survives, for every entity one column's extents hold more than once.
///
/// A row of extent *i* carrying entity *e* is live exactly where no later extent holds *e*. The
/// map states that as one ascending table over the whole column rather than a set per extent:
/// `repeats` carries every entity more than one extent holds, paired with the highest extent index
/// that holds it. An entity absent from it is in one extent only, and that extent's row is live by
/// construction.
///
/// **A row costs a binary search over `repeats`, which is empty for almost every corpus.** The
/// table is a `Vec` rather than the Roaring bitmap it is built from deliberately: a Roaring `rank`
/// walks the container index, and this is asked once of every row of every extent, where the
/// per-extent set it replaced cost one `contains`.
///
/// **What survives the build is eight bytes a repeated entity; the transient, while one column's
/// map is built, is four whole-column bitmaps.** A bitmap over an entity space of *n* is at most
/// *n*/8 bytes — 437 MB apiece at the 3.5×10⁹-row GBIF rung — whatever the extent count. The first
/// pass holds four at its worst moment: the running union, the repeats so far, the extent's own
/// bitmap, and the intersection of the first with the third. The second pass holds three of them
/// and the table. GBIF carries about one row an entity, so the table there is close to empty.
///
/// The cost in time is one extra sequential pass over the has-row files, which the first pass has
/// just warmed: the map cannot be built in one, because which entities repeat is not known until
/// every extent has been read.
pub(crate) struct DuplicateMap {
    /// `(entity, the highest extent index holding it)`, ascending in the entity.
    repeats: Vec<(u32, u32)>,
}

impl DuplicateMap {
    /// Stream the extents' has-row files in write order, twice.
    ///
    /// Each file is deserialised, used, and dropped before the next is read, so what stands through
    /// the passes is the running union, the repeats and one extent's bitmap — never a set an
    /// extent.
    fn over(extents: &[ExtentPaths], blobs: &[RecordBlob]) -> Result<Self> {
        assert_eq!(
            extents.len(),
            blobs.len(),
            "open_all returns one blob an extent, and the two are walked in step here"
        );
        let mut seen = Bitmap::new();
        let mut repeat = Bitmap::new();
        for (paths, blob) in extents.iter().zip(blobs) {
            let hasrow = read_checked_hasrow(paths, blob)?;
            repeat.or_inplace(&seen.and(&hasrow));
            seen.or_inplace(&hasrow);
        }
        drop(seen);
        let mut repeats: Vec<(u32, u32)> = repeat.iter().map(|entity| (entity, 0u32)).collect();
        if !repeats.is_empty() {
            for (i, paths) in extents.iter().enumerate() {
                let index = u32::try_from(i).expect("an extent index is a u32");
                let here = read_hasrow(paths)?.and(&repeat);
                // `here` is a subset of `repeat` and both ascend, so one walk in step reaches every
                // slot this extent holds without a rank. Extents are visited in write order, so the
                // last index written to a slot is the highest extent holding that entity.
                let mut at = 0usize;
                for entity in here.iter() {
                    while at < repeats.len() && repeats[at].0 < entity {
                        at += 1;
                    }
                    match repeats.get_mut(at) {
                        Some(slot) if slot.0 == entity => slot.1 = index,
                        _ => break,
                    }
                }
            }
        }
        drop(repeat);
        Ok(DuplicateMap { repeats })
    }

    /// Whether the row extent `extent` holds for `entity` is the one that survives.
    fn is_live(&self, extent: usize, entity: u32) -> bool {
        match self.repeats.binary_search_by_key(&entity, |&(e, _)| e) {
            Err(_) => true,
            Ok(i) => self.repeats[i].1 as usize == extent,
        }
    }
}

/// Every extent's has-row bitmap against what its directory addresses, read and dropped one at a
/// time. [`ExtentColumn::cascade`] says why the fold makes this check of its own.
fn check_hasrow_totals(extents: &[ExtentPaths], blobs: &[RecordBlob]) -> Result<()> {
    assert_eq!(
        extents.len(),
        blobs.len(),
        "open_all returns one blob an extent, and the two are walked in step here"
    );
    for (paths, blob) in extents.iter().zip(blobs) {
        drop(read_checked_hasrow(paths, blob)?);
    }
    Ok(())
}

/// One extent's has-row bitmap, checked against the rows its directory addresses.
///
/// **This is the check [`RecordBlob::open_rows_only`][tessera_filter::RecordBlob::open_rows_only]
/// gives up**, made wherever this build reads a has-row file: an extent whose directory and bitmap
/// disagree about how many rows it holds refuses here rather than being walked.
fn read_checked_hasrow(paths: &ExtentPaths, blob: &RecordBlob) -> Result<Bitmap> {
    let hasrow = read_hasrow(paths)?;
    if hasrow.cardinality() != blob.rows() {
        return Err(BuildError::Invalid(format!(
            "the extent at {} addresses {} rows but its has-row bitmap holds {} entities",
            paths.blocks.display(),
            blob.rows(),
            hasrow.cardinality()
        )));
    }
    Ok(hasrow)
}

/// One extent's has-row bitmap, read and deserialised on its own.
fn read_hasrow(paths: &ExtentPaths) -> Result<Bitmap> {
    let bytes = std::fs::read(&paths.hasrow).map_err(|e| BuildError::io(&paths.hasrow, e))?;
    Bitmap::try_deserialize::<croaring::Portable>(&bytes).ok_or_else(|| {
        BuildError::Invalid(format!(
            "the extent has-row bitmap at {} is not portable Roaring",
            paths.hasrow.display()
        ))
    })
}

/// One window of one extent: a contiguous run of blocks, which is a contiguous run of entities.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtentWindow {
    pub(crate) extent: usize,
    pub(crate) lo: usize,
    pub(crate) hi: usize,
}

impl OpenExtents {
    /// At most `target` windows over the extents' blocks, none spanning two extents.
    ///
    /// The text index divides its work by these, exactly as it divided the arena's byte ranges:
    /// a worker reads one window front to back and decompresses a block at a time.
    pub(crate) fn windows(&self, target: usize) -> Vec<ExtentWindow> {
        let blocks: usize = self.blobs.iter().map(RecordBlob::block_count).sum();
        if blocks == 0 {
            return Vec::new();
        }
        let per_window = blocks.div_ceil(target.max(1)).max(1);
        let mut windows = Vec::new();
        for (extent, blob) in self.blobs.iter().enumerate() {
            let mut lo = 0usize;
            while lo < blob.block_count() {
                let hi = (lo + per_window).min(blob.block_count());
                windows.push(ExtentWindow { extent, lo, hi });
                lo = hi;
            }
        }
        windows
    }

    /// Every live `(entity, value)` in one window, ascending in the entity.
    ///
    /// A row whose entity a later extent holds is a value a later chunk overwrote, and it is
    /// skipped here for the same reason the record blob does not write it ([`DuplicateMap`]).
    pub(crate) fn for_each_record_in(
        &self,
        window: ExtentWindow,
        visit: &mut dyn FnMut(usize, &str) -> Result<()>,
    ) -> Result<()> {
        let blob = &self.blobs[window.extent];
        let mut cursor = blob.rows_cursor_over(window.lo, window.hi);
        while cursor.advance().map_err(record_error)? {
            let entity = cursor.entity();
            if !self.duplicates.is_live(window.extent, entity) {
                continue;
            }
            let value = field_value(&cursor, self.column)?;
            visit(entity as usize, value)?;
        }
        Ok(())
    }

    /// Every live `(entity, value)` across one column's extents, ascending in the entity.
    ///
    /// The extents each ascend and exactly one of them is live for any entity, so the lowest head
    /// is the next row and no two streams ever offer the same entity. That is the stream the
    /// keyword dictionary pass reads in place of an entity-indexed arena: it wants the column
    /// once, in entity order, and this delivers it without the offset array that ordering used to
    /// cost (`build-column-extents.md`).
    pub(crate) fn for_each_live_record(
        &self,
        visit: &mut dyn FnMut(u32, &str) -> Result<()>,
    ) -> Result<()> {
        let mut cursors: Vec<tessera_filter::RecordRowCursor<'_>> =
            self.blobs.iter().map(RecordBlob::rows_cursor).collect();
        let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u32, usize)>> =
            std::collections::BinaryHeap::with_capacity(cursors.len());
        for (i, cursor) in cursors.iter_mut().enumerate() {
            if next_live(cursor, &self.duplicates, i)? {
                heap.push(std::cmp::Reverse((cursor.entity(), i)));
            }
        }
        while let Some(std::cmp::Reverse((entity, source))) = heap.pop() {
            visit(entity, field_value(&cursors[source], self.column)?)?;
            if next_live(&mut cursors[source], &self.duplicates, source)? {
                let next_entity = cursors[source].entity();
                if next_entity <= entity {
                    return Err(BuildError::Invalid(format!(
                        "an extent yielded entity {next_entity} at or below its predecessor \
                         {entity}; an extent is written in the chunk's entity order"
                    )));
                }
                heap.push(std::cmp::Reverse((next_entity, source)));
            }
        }
        Ok(())
    }
}

/// The next live row of one extent — a row that is not is a value a later chunk overwrote,
/// skipped here for the reason [`OpenExtents::for_each_record_in`] skips it.
fn next_live(
    cursor: &mut tessera_filter::RecordRowCursor<'_>,
    duplicates: &DuplicateMap,
    extent: usize,
) -> Result<bool> {
    loop {
        if !cursor.advance().map_err(record_error)? {
            return Ok(false);
        }
        if duplicates.is_live(extent, cursor.entity()) {
            return Ok(true);
        }
    }
}

/// The one field the row a cursor is at carries, borrowed out of the cursor's own block.
fn field_value<'a>(
    cursor: &'a tessera_filter::RecordRowCursor<'a>,
    column: usize,
) -> Result<&'a str> {
    let tag = column as u16;
    for i in 0..cursor.field_count() {
        let field = cursor.field(i).map_err(record_error)?;
        if field.tag == tag {
            if let RecordValueRef::Utf8(value) = field.value {
                return Ok(value);
            }
        }
    }
    Err(BuildError::Invalid(format!(
        "an extent's row carries no utf8 value at field tag {tag}; the extent was written \
         by this build and holds one field per row"
    )))
}

fn record_error(e: tessera_filter::RecordError) -> BuildError {
    BuildError::Invalid(format!("an extent does not read back: {e}"))
}

/// One extent's live rows as a stream the record blob's merge reads.
pub(crate) struct ExtentRows<'a> {
    cursor: tessera_filter::RecordRowCursor<'a>,
    duplicates: &'a DuplicateMap,
    extent: usize,
}

impl<'a> ExtentRows<'a> {
    /// Every stream of one column's extents, in the order the join wrote them.
    pub(crate) fn over(open: &'a OpenExtents) -> Vec<ExtentRows<'a>> {
        open.blobs
            .iter()
            .enumerate()
            .map(|(extent, blob)| ExtentRows {
                cursor: blob.rows_cursor(),
                duplicates: &open.duplicates,
                extent,
            })
            .collect()
    }
}

impl tessera_filter_write::RecordRows for ExtentRows<'_> {
    fn advance(&mut self) -> std::io::Result<bool> {
        loop {
            if !self.cursor.advance().map_err(std::io::Error::from)? {
                return Ok(false);
            }
            if self.duplicates.is_live(self.extent, self.cursor.entity()) {
                return Ok(true);
            }
        }
    }
    fn entity(&self) -> u32 {
        self.cursor.entity()
    }
    fn field_count(&self) -> usize {
        self.cursor.field_count()
    }
    fn field(&self, i: usize) -> std::io::Result<RecordFieldRef<'_>> {
        self.cursor.field(i).map_err(std::io::Error::from)
    }
}

/// Open a run of extents, mapped, for a sequential walk alone.
///
/// Every reader of an extent — the fold's merge, the keyword dictionary pass, the text index's
/// windows and the record blob's merge — takes its rows front to back, so none of them addresses a
/// row by entity and none of them needs the has-row bitmap on the heap
/// ([`RecordBlob::open_rows_only`][tessera_filter::RecordBlob::open_rows_only] states what that
/// gives up and what stands instead).
fn open_all(extents: &[ExtentPaths]) -> Result<Vec<RecordBlob>> {
    let mut blobs = Vec::with_capacity(extents.len());
    for paths in extents {
        blobs.push(
            RecordBlob::open_rows_only(&paths.blocks, &paths.directory, Access::Mapped)
                .map_err(|e| BuildError::io(&paths.blocks, std::io::Error::from(e)))?,
        );
    }
    Ok(blobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three extents over one column, with two entities written in more than one of them and two
    /// written in exactly one.
    fn three_extents(dir: &Path) -> ExtentColumn {
        let mut column = ExtentColumn::new(dir, 0, "note");
        column
            .push_extent(&[(1, "0:1"), (5, "0:5"), (9, "0:9")])
            .expect("an extent");
        column
            .push_extent(&[(5, "1:5"), (7, "1:7")])
            .expect("an extent");
        column
            .push_extent(&[(9, "2:9"), (11, "2:11")])
            .expect("an extent");
        column
    }

    /// **The last extent to hold an entity is the one whose row survives**, and an entity only one
    /// extent holds survives from it — which is the whole of what the duplicate map answers. Entity
    /// 5 is in extents 0 and 1, entity 9 in extents 0 and 2, and 1, 7 and 11 in one apiece.
    #[test]
    fn the_last_extent_to_hold_a_repeated_entity_is_the_live_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let column = three_extents(dir.path());
        let open = column.open().expect("the extents open");
        let map = &open.duplicates;

        // The repeated entities: live in their highest extent and nowhere else.
        assert!(!map.is_live(0, 5), "extent 0's entity 5 was overwritten");
        assert!(map.is_live(1, 5), "extent 1 holds the surviving entity 5");
        assert!(!map.is_live(0, 9), "extent 0's entity 9 was overwritten");
        assert!(map.is_live(2, 9), "extent 2 holds the surviving entity 9");

        // A single-extent entity is live wherever it is asked for, being outside `repeat`.
        assert!(map.is_live(0, 1), "entity 1 is in extent 0 alone");
        assert!(map.is_live(1, 7), "entity 7 is in extent 1 alone");
        assert!(map.is_live(2, 11), "entity 11 is in extent 2 alone");
    }

    /// The stream the keyword dictionary pass reads: every live row once, ascending in the entity,
    /// each carrying the value of the last extent that wrote it.
    #[test]
    fn the_live_stream_is_one_row_an_entity_carrying_the_last_value_written() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let column = three_extents(dir.path());
        let open = column.open().expect("the extents open");

        let mut seen: Vec<(u32, String)> = Vec::new();
        open.for_each_live_record(&mut |entity, value| {
            seen.push((entity, value.to_string()));
            Ok(())
        })
        .expect("the stream reads");
        assert_eq!(
            seen,
            vec![
                (1, "0:1".to_string()),
                (5, "1:5".to_string()),
                (7, "1:7".to_string()),
                (9, "2:9".to_string()),
                (11, "2:11".to_string()),
            ]
        );
    }

    /// The same rows through the windowed reader the text index divides its work by. A window is
    /// inside one extent, so the rows arrive extent by extent rather than in entity order, but the
    /// set of them is the same.
    #[test]
    fn the_windowed_reader_yields_the_same_live_rows() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let column = three_extents(dir.path());
        let open = column.open().expect("the extents open");

        let mut seen: Vec<(usize, String)> = Vec::new();
        for window in open.windows(16) {
            open.for_each_record_in(window, &mut |entity, value| {
                seen.push((entity, value.to_string()));
                Ok(())
            })
            .expect("a window reads");
        }
        seen.sort();
        assert_eq!(
            seen,
            vec![
                (1, "0:1".to_string()),
                (5, "1:5".to_string()),
                (7, "1:7".to_string()),
                (9, "2:9".to_string()),
                (11, "2:11".to_string()),
            ]
        );
    }

    /// **The fold is a rewrite of the extents, and the live answer has to survive it.** Folding
    /// with `max_open = 2` puts extents 0 and 1 into one intermediate — where the merge itself
    /// resolves entity 5 to the later value — and leaves extent 2 beside it, so the map afterwards
    /// answers over two extents where it answered over three, and the stream is the same either
    /// way.
    #[test]
    fn a_folded_column_answers_the_same_live_rows() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let plain = three_extents(dir.path());
        let expected = live_rows(&plain.open().expect("the extents open"));

        let folded_dir = tempfile::tempdir().expect("a temp dir");
        let mut folded = three_extents(folded_dir.path());
        folded.cascade(2).expect("the fold runs");
        assert!(
            folded.extents.len() <= 2,
            "the cascade folds until the fan-in holds, got {}",
            folded.extents.len()
        );
        assert_eq!(
            live_rows(&folded.open().expect("the extents open")),
            expected
        );
    }

    /// **The fold changes no output byte, which is the claim that lets `merge_fan_in` set the
    /// fan-in by the budget rather than by what a reader would prefer.** A bundle-level proof of it
    /// is not reachable from a build here: the extent count is `JOIN_STAGE_BYTES` over the staged
    /// row width, a constant of the code, while the fan-in is the memory budget over sixty-four
    /// 256 KiB blocks — so folding a 10⁷-row corpus wants a budget under 82 MB and its entity-order
    /// stages refuse under 1,082 MiB. The two meet at the 3.5×10⁹-row rung and nowhere a test can
    /// go. So the claim is made here, over the one artefact a fold can change: the record blob its
    /// extents merge into, written from a folded column and an unfolded one and compared byte for
    /// byte.
    #[test]
    fn a_fold_changes_no_byte_of_the_blob_its_extents_merge_into() {
        let plain_dir = tempfile::tempdir().expect("a temp dir");
        let plain = three_extents(plain_dir.path());
        let folded_dir = tempfile::tempdir().expect("a temp dir");
        let mut folded = three_extents(folded_dir.path());
        folded.cascade(2).expect("the fold runs");
        assert_eq!(folded.extents.len(), 2, "three extents fold into two");

        let plain_blob = blob_of(&plain, plain_dir.path());
        let folded_blob = blob_of(&folded, folded_dir.path());
        for (a, b) in plain_blob.iter().zip(&folded_blob) {
            assert_eq!(
                std::fs::read(a).expect("the file reads"),
                std::fs::read(b).expect("the file reads"),
                "{} and {} differ",
                a.display(),
                b.display()
            );
        }
    }

    /// The record blob one column's extents merge into, as `write_record_blob` writes it: the three
    /// files, in the order [`ExtentPaths`] names them.
    fn blob_of(column: &ExtentColumn, dir: &Path) -> Vec<PathBuf> {
        let open = column.open().expect("the extents open");
        let out = ExtentPaths {
            blocks: dir.join("merged.blocks.bin"),
            hasrow: dir.join("merged.hasrow.roaring"),
            directory: dir.join("merged.directory.arrow"),
        };
        let mut rows = ExtentRows::over(&open);
        let mut sources: Vec<&mut dyn tessera_filter_write::RecordRows> = rows
            .iter_mut()
            .map(|r| r as &mut dyn tessera_filter_write::RecordRows)
            .collect();
        tessera_filter_write::merge_record_rows(
            &mut sources,
            &Bitmap::new(),
            &out.blocks,
            &out.hasrow,
            &out.directory,
            RECORD_BLOCK_TARGET,
        )
        .expect("the merge writes");
        vec![out.blocks, out.hasrow, out.directory]
    }

    /// The check `RecordBlob::open_rows_only` gives up, made where a fold consumes its inputs: the
    /// extents a group takes in are unlinked before `open` ever sees them, so a has-row file that
    /// disagrees with its directory has to refuse here or never.
    #[test]
    fn a_folded_groups_input_whose_hasrow_disagrees_with_its_directory_refuses() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut column = three_extents(dir.path());
        // Well-formed portable Roaring, and the wrong cardinality for the three rows extent 0's
        // directory addresses.
        let wrong: Bitmap = [1u32, 5, 9, 11].into_iter().collect();
        std::fs::write(
            &column.extents[0].hasrow,
            wrong.serialize::<croaring::Portable>(),
        )
        .expect("the has-row file rewrites");
        let err = column.cascade(2).expect_err("the fold refuses its input");
        let message = format!("{err}");
        assert!(
            message.contains("addresses 3 rows but its has-row bitmap holds 4 entities"),
            "the refusal names both counts, got: {message}"
        );
    }

    /// Every live `(entity, value)` of a column, in entity order.
    fn live_rows(open: &OpenExtents) -> Vec<(u32, String)> {
        let mut seen = Vec::new();
        open.for_each_live_record(&mut |entity, value| {
            seen.push((entity, value.to_string()));
            Ok(())
        })
        .expect("the stream reads");
        seen
    }

    /// A column no entity of which is written twice carries an empty duplicate map, and every row
    /// of every extent is live.
    #[test]
    fn a_column_that_repeats_no_entity_carries_an_empty_map() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut column = ExtentColumn::new(dir.path(), 0, "note");
        column
            .push_extent(&[(1, "a"), (2, "b")])
            .expect("an extent");
        column
            .push_extent(&[(3, "c"), (4, "d")])
            .expect("an extent");
        let open = column.open().expect("the extents open");
        assert!(open.duplicates.repeats.is_empty());
        let mut rows = 0usize;
        open.for_each_live_record(&mut |_, _| {
            rows += 1;
            Ok(())
        })
        .expect("the stream reads");
        assert_eq!(rows, 4, "every row of both extents is live");
    }
}
