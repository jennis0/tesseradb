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

use crate::dict::write_sorted_dict;
use crate::values::{Access, Codes, ValueColumn};
use crate::values_writer::write_value_column;

/// Where a column's per-flush extents live, under the column's own directory.
pub const EXTENTS_DIR: &str = "extents";

/// One flush's extent for one column: `(values, presence, dictionary)`, all under
/// `attrs/<column>/extents/` and named for the flush (`filter-index.md` §2.5, `records-and-search`
/// §7).
///
/// The dictionary path is returned for every column and written only by the family that has one.
/// It is the artefact's *name*, not a claim that it exists: what a reader opens is what the
/// manifest record names, never a path this convention reconstructs — the rule `AttrExtent` states
/// for the values and presence files, and the same reason applies here twice over, because an
/// extent's ordinals resolved against another layer's dictionary are a recolouring with no symptom.
pub fn extent_paths(column_dir: &Path, flush_id: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = column_dir.join(EXTENTS_DIR);
    (
        dir.join(format!("{flush_id}.arrow")),
        dir.join(format!("{flush_id}.roaring")),
        dir.join(format!("{flush_id}.dict")),
    )
}

/// Write one flush's values for one column as an extent, and return the two paths written.
///
/// **The presence bitmap is written unconditionally**, where a base column writes one only when
/// presence is partial. That is not symmetry for its own sake: a base column's missing presence
/// file *means* the entity id is the array index, and an extent's entities are a set allocated
/// above the build's high-water — never starting at zero, and not contiguous where a commit window
/// interleaved views (write-path §4.2). An extent read positionally would answer entity 0 with the
/// first flushed entity's value and every entity after it with somebody else's, with no error
/// anywhere. So the one file whose absence carries meaning is always present here, and
/// [`open_extent`] takes its path rather than probing for it.
///
/// An extent for a column no flushed entity carried a value in is still written, empty: the file
/// set is then a function of the schema rather than of the data, which is what lets an operator
/// predict what a flush produces. That holds for `dict_keys` too — a keyword column whose batch
/// carried nothing gets a dictionary of no keys, not no dictionary.
///
/// **One call writes one column's whole extent, and that is the atomicity argument in code**
/// (records §7). An extent's ordinals are meaningful only against that extent's own dictionary, so
/// the two are one artefact; splitting the write across two functions would make it possible to
/// publish one without the other, which is the failure that has no symptom. `dict_keys` is
/// `Some` exactly for the keyword family, and its keys must already be **sorted and distinct** —
/// [`write_sorted_dict`] refuses anything else rather than repairing it, because de-duplicating
/// here would shift every ordinal `codes` already holds.
pub fn write_extent(
    column_dir: &Path,
    flush_id: &str,
    codes: &Codes,
    presence: &Bitmap,
    dict_keys: Option<&[&str]>,
) -> io::Result<(PathBuf, PathBuf, Option<PathBuf>)> {
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
    // A dictionary beside anything but an ordinal column would be a pair that cannot be read
    // together — refused here rather than discovered by a scan that reads a `u64` as an ordinal.
    if dict_keys.is_some() && !matches!(codes, Codes::U32(_)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filter extent: a dictionary was supplied for a column whose values are not u32 \
             ordinals",
        ));
    }
    let (values_path, presence_path, dict_path) = extent_paths(column_dir, flush_id);
    std::fs::create_dir_all(column_dir.join(EXTENTS_DIR))?;
    let dict_path = match dict_keys {
        Some(keys) => {
            write_sorted_dict(&dict_path, keys.iter().copied())?;
            Some(dict_path)
        }
        None => None,
    };
    write_value_column(&values_path, &presence_path, codes, Some(presence))?;
    Ok((values_path, presence_path, dict_path))
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
    access: Access,
) -> io::Result<ValueColumn> {
    ValueColumn::open(values_path, Some(presence_path), access)
}
