//! Per-flush extents: the values one flush publishes for one column, and how they are named.
//!
//! A value column is written once by the batch build and covers `[0, entity_id_high_water)`; every
//! flush since has published entities above it, and each appends its own values here
//! (`filter-index.md` §2.1, §2.5). The reader composes base and extents into one column, which is
//! `tessera-engine`'s `filter` module rather than this crate's: an extent is a [`ValueColumn`] like
//! any other, and the layering is a property of a *bundle* rather than of a column.
//!
//! **A separate module from [`crate::values`], and that is measured rather than tidy.** Adding
//! these three functions to that file cost the scan 0.27 → 0.46 ns per candidate entity on the
//! universal-contiguous arm at 10⁹ — a 70% regression in code that never runs during a scan, and
//! the fourth time this module's history has recorded one. Nothing here belongs in the file whose
//! inner loop the whole design is sized against.

use std::io;
use std::path::{Path, PathBuf};

use croaring::Bitmap;

use crate::values::{write_value_column, Codes, ValueColumn};

/// Where a column's per-flush extents live, under the column's own directory.
pub const EXTENTS_DIR: &str = "extents";

/// One flush's extent for one column: `(values, presence)`, both under `attrs/<column>/extents/`
/// and named for the flush (`filter-index.md` §2.5).
pub fn extent_paths(column_dir: &Path, flush_id: &str) -> (PathBuf, PathBuf) {
    let dir = column_dir.join(EXTENTS_DIR);
    (
        dir.join(format!("{flush_id}.arrow")),
        dir.join(format!("{flush_id}.roaring")),
    )
}

/// Write one flush's values for one column as an extent, and return the two paths written.
///
/// **The presence bitmap is written unconditionally**, where a base column writes one only when
/// presence is partial. That is not symmetry for its own sake: a base column's missing presence
/// file *means* the entity id is the array index, and an extent's entities are a set allocated
/// above the build's high-water — never starting at zero, and not contiguous where a commit window
/// interleaved slices (write-path §4.2). An extent read positionally would answer entity 0 with the
/// first flushed entity's value and every entity after it with somebody else's, with no error
/// anywhere. So the one file whose absence carries meaning is always present here, and
/// [`open_extent`] takes its path rather than probing for it.
///
/// An extent for a column no flushed entity carried a value in is still written, empty: the file
/// set is then a function of the schema rather than of the data, which is what lets an operator
/// predict what a flush produces.
pub fn write_extent(
    column_dir: &Path,
    flush_id: &str,
    codes: &Codes,
    presence: &Bitmap,
) -> io::Result<(PathBuf, PathBuf)> {
    if presence.cardinality() != codes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "filter extent: presence has {} entities but {} values were supplied",
                presence.cardinality(),
                codes.len()
            ),
        ));
    }
    let (values_path, presence_path) = extent_paths(column_dir, flush_id);
    std::fs::create_dir_all(column_dir.join(EXTENTS_DIR))?;
    write_value_column(&values_path, &presence_path, codes, Some(presence))?;
    Ok((values_path, presence_path))
}

/// Read one extent written by [`write_extent`].
///
/// **Both paths are required, and a missing one is an error rather than an absence.** The manifest
/// names and digests them, so a file that is not there means the bundle is not what its manifest
/// says — and the failure that would follow from treating it as "those entities carry no value" is
/// a wrong answer wearing a correct one's clothes, which is the same reasoning the refusal this
/// composition replaced was built on.
pub fn open_extent(
    values_path: &Path,
    presence_path: &Path,
    mmap: bool,
) -> io::Result<ValueColumn> {
    ValueColumn::open(values_path, Some(presence_path), mmap)
}
