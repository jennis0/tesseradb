//! A `text` column's prose, spilled as record-blob extents while the join decodes it.
//!
//! # Why the prose is not a column
//!
//! Entity ids are assigned in signature-then-Morton order and an attribute source is read in its
//! own row order, so placing a value at its entity index is a permutation of the source. Every
//! other declared family survives that: a fixed-width value is a slot, and a keyword's characters
//! are a fraction of a corpus's bytes. Prose is the corpus's bytes. At the 10⁸ PaperSeek rung it
//! is 128 GiB written to a mapping on a 47 GB box, read back once at random by the text index and
//! once more at random by the record blob.
//!
//! So it is never permuted. Each chunk the join stages is already sorted by entity — the scatter
//! into the fixed-width columns needs that — and each chunk of each `text` column is written here
//! as one **blob extent**: the same three files, the same block format and the same addressing as
//! the base blob (`records-and-search.md` §3). The text index reads the extents in block windows
//! and the record blob merges them. Every byte moves sequentially, and the working set is one
//! chunk plus one block per extent. `docs/design/build-prose-extents.md` carries the design.
//!
//! # An entity written twice
//!
//! An attribute source may carry two rows for one entity, and the last one written is the value.
//! Within a chunk that is the last row of the stable sort, collapsed before the extent is written.
//! Across chunks the earlier row is in an earlier extent, so the **live set** of extent *i* is its
//! has-row bitmap less the union of every later extent's ([`OpenProse::live`]). Both readers skip
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

/// One `text` column's extents, in the order the join wrote them.
#[derive(Debug)]
pub(crate) struct ProseColumn {
    /// The column's position in the declaration, which is its blob field tag.
    pub(crate) column: usize,
    name: String,
    dir: PathBuf,
    extents: Vec<ExtentPaths>,
}

impl ProseColumn {
    pub(crate) fn new(dir: &Path, column: usize, name: &str) -> Self {
        ProseColumn {
            column,
            name: name.to_string(),
            dir: dir.to_path_buf(),
            extents: Vec::new(),
        }
    }

    /// Write one chunk's rows as an extent. `rows` ascends strictly in the entity, which
    /// [`RecordBlobWriter::push_row`] refuses to take on trust.
    pub(crate) fn push_extent(&mut self, rows: &[(u32, &str)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let serial = self.extents.len();
        let stem = format!("prose-{}-{serial:05}", self.column);
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

    /// Open every extent, mapped, with each one's live set beside it.
    pub(crate) fn open(&self) -> Result<OpenProse> {
        let mut blobs = Vec::with_capacity(self.extents.len());
        for paths in &self.extents {
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
        Ok(OpenProse {
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
pub(crate) struct OpenProse {
    /// The declaration position, which is the field tag the rows carry.
    pub(crate) column: usize,
    pub(crate) blobs: Vec<RecordBlob>,
    pub(crate) live: Vec<Bitmap>,
}

/// One window of one extent: a contiguous run of blocks, which is a contiguous run of entities.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProseWindow {
    pub(crate) extent: usize,
    pub(crate) lo: usize,
    pub(crate) hi: usize,
}

impl OpenProse {
    /// At most `target` windows over the extents' blocks, none spanning two extents.
    ///
    /// The text index divides its work by these, exactly as it divided the arena's byte ranges:
    /// a worker reads one window front to back and decompresses a block at a time.
    pub(crate) fn windows(&self, target: usize) -> Vec<ProseWindow> {
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
                windows.push(ProseWindow { extent, lo, hi });
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
        window: ProseWindow,
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

/// The one field a prose extent's row carries, as a string.
fn field_value(fields: &[RecordField], column: usize) -> Result<&str> {
    let tag = column as u16;
    match fields.iter().find(|field| field.tag == tag) {
        Some(RecordField {
            value: RecordValue::Utf8(value),
            ..
        }) => Ok(value),
        _ => Err(BuildError::Invalid(format!(
            "a prose extent's row carries no utf8 value at field tag {tag}; the extent was \
             written by this build and holds one field per row"
        ))),
    }
}

fn record_error(e: tessera_filter::RecordError) -> BuildError {
    BuildError::Invalid(format!("a prose extent does not read back: {e}"))
}

/// One extent's live rows as a stream the record blob's merge reads.
pub(crate) struct ExtentRows<'a> {
    cursor: tessera_filter::RecordRowCursor<'a>,
    live: &'a Bitmap,
}

impl<'a> ExtentRows<'a> {
    /// Every stream of one column's extents, in the order the join wrote them.
    pub(crate) fn over(open: &'a OpenProse) -> Vec<ExtentRows<'a>> {
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
