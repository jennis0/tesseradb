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
//! extent. `docs/design/build-prose-extents.md` carries the design.
//!
//! # An entity written twice
//!
//! An attribute source may carry two rows for one entity, and the last one written is the value.
//! Within a chunk that is the last row of the stable sort, collapsed before the extent is written.
//! Across chunks the earlier row is in an earlier extent, so the **live set** of extent *i* is its
//! has-row bitmap less the union of every later extent's ([`OpenExtents::live`]). Both readers skip
//! a row outside it, so the index holds terms for exactly the values the blob holds.

use std::path::{Path, PathBuf};

use croaring::Bitmap;
use tessera_filter::{Access, RecordBlob, RecordField, RecordValue, RECORD_BLOCK_TARGET};
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
}

impl ExtentColumn {
    pub(crate) fn new(dir: &Path, column: usize, name: &str) -> Self {
        ExtentColumn {
            column,
            name: name.to_string(),
            dir: dir.to_path_buf(),
            extents: Vec::new(),
            folds: 0,
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
        let mut writer = RecordBlobWriter::create(
            &paths.blocks,
            &paths.hasrow,
            &paths.directory,
            RECORD_BLOCK_TARGET,
        )
        .map_err(|e| BuildError::io(&paths.blocks, e))?;
        for &(entity, value) in rows {
            writer
                .push_row(
                    entity,
                    &[RecordField {
                        tag,
                        value: RecordValue::Utf8(value.to_string()),
                    }],
                )
                .map_err(|e| BuildError::io(&paths.blocks, e))?;
        }
        writer
            .finish()
            .map_err(|e| BuildError::io(&paths.blocks, e))?;
        self.extents.push(paths);
        Ok(())
    }

    /// How many extents one merge holds open, and the number above which they are folded into
    /// intermediates first: **128**.
    ///
    /// What an open extent costs the merge is one uncompressed block, 256 KiB, so 128 of them is
    /// 32 MB. The cascade above that is a second write of the group's characters, which is why
    /// the bound is not tighter: a join chunk is `JOIN_STAGE_BYTES` of staged rows, so a corpus
    /// reaches 128 extents of one column only at ten times the 10⁸ rung's prose.
    pub(crate) const MERGE_FAN_IN: usize = 128;

    /// Fold the extents in groups until at most [`Self::MERGE_FAN_IN`] are left.
    ///
    /// A group is a contiguous run in write order and is merged by the same row merge the blob
    /// itself is written by, so an entity written twice inside one group comes out carrying the
    /// later value and the ordering the live sets rest on survives.
    pub(crate) fn cascade(&mut self) -> Result<()> {
        while self.extents.len() > Self::MERGE_FAN_IN {
            let groups = self.extents.len().div_ceil(Self::MERGE_FAN_IN);
            let per_group = self.extents.len().div_ceil(groups);
            let taken: Vec<ExtentPaths> = self.extents.drain(..).collect();
            let mut folded: Vec<ExtentPaths> = Vec::with_capacity(groups);
            for (group, extents) in taken.chunks(per_group).enumerate() {
                let stem = format!("extent-{}-fold{}-{group:05}", self.column, self.folds);
                let out = ExtentPaths {
                    blocks: self.dir.join(format!("{stem}.blocks.bin")),
                    hasrow: self.dir.join(format!("{stem}.hasrow.roaring")),
                    directory: self.dir.join(format!("{stem}.directory.arrow")),
                };
                let blobs = open_all(extents)?;
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

    /// Open every extent, mapped, with each one's live set beside it.
    pub(crate) fn open(&self) -> Result<OpenExtents> {
        let blobs = open_all(&self.extents)?;
        // Later extents were written later, so a repeated entity's value is the last extent's.
        // Walked backwards, `seen` is the union of every later extent's rows.
        let mut live = vec![Bitmap::new(); blobs.len()];
        let mut seen = Bitmap::new();
        for (i, blob) in blobs.iter().enumerate().rev() {
            let mut mine = blob.hasrow().clone();
            mine.andnot_inplace(&seen);
            seen.or_inplace(blob.hasrow());
            live[i] = mine;
        }
        Ok(OpenExtents {
            column: self.column,
            blobs,
            live,
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

/// One column's extents, open, with each one's live set.
pub(crate) struct OpenExtents {
    /// The declaration position, which is the field tag the rows carry.
    pub(crate) column: usize,
    pub(crate) blobs: Vec<RecordBlob>,
    pub(crate) live: Vec<Bitmap>,
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
    /// A row outside the extent's live set is a value a later chunk overwrote, and it is skipped
    /// here for the same reason the record blob does not write it.
    pub(crate) fn for_each_record_in(
        &self,
        window: ExtentWindow,
        visit: &mut dyn FnMut(usize, &str) -> Result<()>,
    ) -> Result<()> {
        let blob = &self.blobs[window.extent];
        let live = &self.live[window.extent];
        let mut cursor = blob.rows_cursor_over(window.lo, window.hi);
        while let Some((entity, fields)) = cursor.next_row().map_err(record_error)? {
            if !live.contains(entity) {
                continue;
            }
            let value = field_value(&fields, self.column)?;
            visit(entity as usize, value)?;
        }
        Ok(())
    }
}

/// The one field an extent's row carries, as a string.
fn field_value(fields: &[RecordField], column: usize) -> Result<&str> {
    let tag = column as u16;
    match fields.iter().find(|field| field.tag == tag) {
        Some(RecordField {
            value: RecordValue::Utf8(value),
            ..
        }) => Ok(value),
        _ => Err(BuildError::Invalid(format!(
            "an extent's row carries no utf8 value at field tag {tag}; the extent was written \
             by this build and holds one field per row"
        ))),
    }
}

fn record_error(e: tessera_filter::RecordError) -> BuildError {
    BuildError::Invalid(format!("an extent does not read back: {e}"))
}

/// One extent's live rows as a stream the record blob's merge reads.
pub(crate) struct ExtentRows<'a> {
    cursor: tessera_filter::RecordRowCursor<'a>,
    live: &'a Bitmap,
}

impl<'a> ExtentRows<'a> {
    /// Every stream of one column's extents, in the order the join wrote them.
    pub(crate) fn over(open: &'a OpenExtents) -> Vec<ExtentRows<'a>> {
        open.blobs
            .iter()
            .zip(&open.live)
            .map(|(blob, live)| ExtentRows {
                cursor: blob.rows_cursor(),
                live,
            })
            .collect()
    }
}

impl tessera_filter_write::RecordRows for ExtentRows<'_> {
    fn next_row(&mut self) -> std::io::Result<Option<(u32, Vec<RecordField>)>> {
        loop {
            let Some((entity, fields)) = self.cursor.next_row().map_err(std::io::Error::from)?
            else {
                return Ok(None);
            };
            if self.live.contains(entity) {
                return Ok(Some((entity, fields)));
            }
        }
    }
}

/// Open a run of extents, mapped.
fn open_all(extents: &[ExtentPaths]) -> Result<Vec<RecordBlob>> {
    let mut blobs = Vec::with_capacity(extents.len());
    for paths in extents {
        blobs.push(
            RecordBlob::open(
                &paths.blocks,
                &paths.hasrow,
                &paths.directory,
                Access::Mapped,
            )
            .map_err(|e| BuildError::io(&paths.blocks, std::io::Error::from(e)))?,
        );
    }
    Ok(blobs)
}
