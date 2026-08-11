//! `row-entity.u32`: the **row→entity** direction of a slice's base permutation, materialised.
//!
//! `permutation.bin` is entity→row and [`crate::permutation`] is emphatic that it is the only
//! legal path in that direction (I4). This is the other direction, for the one caller that needs
//! it: a **filtered viewport**, which walks a tile's contiguous row range and asks, per row,
//! whether that row's entity is in the filter result (`filter-surface.md` §4's per-tile route).
//!
//! # Why a file rather than the inversion that was already available
//!
//! The mapping is recoverable without storing anything — `columns.arrow` carries `tessera_id` at
//! the row, the identity is a keyed bijection, and `MANIFEST.json` carries the key, which is
//! exactly how [`crate::permutation::SegmentExtent`] recovers a *segment's* rows at open. That
//! route was measured and is too dear for a per-row path:
//! `probes/2026-08-11-viewport-crossing/` attributes **~17.5 ns per row** to `IdentityKey::invert`
//! against ~0.4 ns for reading the `tessera_id` beside it, and batching a whole tile's inversions
//! into a scratch buffer before testing recovers none of it (6.22 ms against 6.00 ms interleaved —
//! it is the four Feistel rounds, not a stalled pipeline). Over a 300,000-row viewport that is the
//! difference between 6.0 ms and 0.7 ms on a clumped result, and 18.5 ms against 11.1 ms on a
//! scattered one.
//!
//! The cost is **4 bytes per row per slice**, and it is *shared across every filter column* — this
//! is a property of the slice's geometry, not of any attribute, so sixteen filterable columns need
//! no more of it than one does. Mapped rather than read, a viewport touches only the rows it draws:
//! ~1.2 MB for 300 tiles of 1,000 rows, sequential within each tile.
//!
//! # Two producers, because row space has two producers
//!
//! Written wherever `permutation.bin` is written and nowhere else — the batch build and the
//! compaction fold. **A merge does not write either**: it consumes and emits *segments*, whose
//! row mapping lives in a [`SegmentExtent`](crate::permutation::SegmentExtent) recovered at open,
//! so the base table is untouched by the operation that renumbers rows most often. That is what
//! makes this affordable to keep current; a base-plus-extent arrangement, fused at the fold, was
//! considered against a merge that did rewrite the base and is not needed.
//!
//! # Layout
//!
//! Headerless: `row_count` little-endian `u32`s, the entity at each row, indexed by row id. No
//! magic and no version, on the precedent contracts §0.2 sets for `morton.u32` and
//! `ext-locator.u32` — the length is the file's, and the manifest's digest is what makes a
//! truncated or substituted file a refusal rather than a misread. **There is no sentinel and no
//! hole**: a row exists only because an entity occupies it, so every slot is meaningful, which is
//! the asymmetry with `permutation.bin` (an entity may have no row; a row always has an entity).

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use tessera_types::{EntityId, RowId};

use crate::error::{Result, StoreError};

/// The file name, beside `permutation.bin` in a slice directory.
pub const ROW_ENTITY_FILE: &str = "row-entity.u32";

/// A memory-mapped `row-entity.u32`.
#[derive(Debug)]
pub struct RowToEntity {
    mmap: Mmap,
    row_count: u32,
    path: PathBuf,
}

impl RowToEntity {
    /// Open and validate `path`: the file must be a whole number of `u32` slots and no longer than
    /// row space can address.
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let len = file
            .metadata()
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        if len % 4 != 0 {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!(
                    "{ROW_ENTITY_FILE} is {len} bytes, not a whole number of u32 slots"
                ),
            });
        }
        let slots = len / 4;
        let row_count = u32::try_from(slots).map_err(|_| StoreError::InvalidPermutation {
            path: path.to_path_buf(),
            detail: format!("{ROW_ENTITY_FILE} holds {slots} rows, which exceeds u32 row space"),
        })?;

        // SAFETY: the same argument `Permutation::load` makes for its own mapping — the file is
        // named and digested by a manifest, and a bundle publisher never mutates a file after
        // naming it (contracts §2.1). A concurrent truncation is the operational hazard that doc
        // records, not one this call can close.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(RowToEntity {
            mmap,
            row_count,
            path: path.to_path_buf(),
        })
    }

    pub fn row_count(&self) -> u32 {
        self.row_count
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn slots(&self) -> &[u8] {
        &self.mmap
    }

    /// The entity occupying `row`, or `None` if `row` is outside this table.
    ///
    /// Out of range is `None` rather than a panic because the caller is a viewport walking a tile's
    /// row range, and a range that runs past the base into a segment's rows is ordinary — the
    /// caller falls through to the extents, exactly as `RowSpace::row_of` falls through in the
    /// other direction.
    #[inline]
    pub fn entity_of(&self, row: RowId) -> Option<EntityId> {
        let raw = row.raw();
        if raw >= self.row_count {
            return None;
        }
        let at = raw as usize * 4;
        let bytes = &self.slots()[at..at + 4];
        Some(EntityId::new(
            u32::from_le_bytes(bytes.try_into().expect("a 4-byte window is 4 bytes")) as u64,
        ))
    }
}

/// Write `row-entity.u32` from a slice's row order.
///
/// `row_order[row]` is the entity at that row — the same vector the build already has in hand as
/// its tiler output, and the same one the fold builds when it rewrites row space, so neither
/// producer computes anything new to call this.
pub fn write_row_entity(path: &Path, row_order: &[u32]) -> std::io::Result<()> {
    use std::io::Write;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut out = std::io::BufWriter::new(file);
    // Chunked rather than one `Vec<u8>` of the whole table: at 10⁹ rows that would be a 4 GB
    // transient beside a build already sized against its peak.
    let mut buffer: Vec<u8> = Vec::with_capacity(1 << 16);
    for chunk in row_order.chunks(1 << 14) {
        buffer.clear();
        for &entity in chunk {
            buffer.extend_from_slice(&entity.to_le_bytes());
        }
        out.write_all(&buffer)?;
    }
    out.flush()?;
    Ok(())
}
