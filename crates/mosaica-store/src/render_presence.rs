//! Which rows of a render column mean anything — decision 0064's bitmap, on the render side.
//!
//! `columns.arrow` is contractually non-nullable (contracts R4) and the reader refuses a nullable
//! column outright, which is what lets the hot path hand back flat zero-copy slices and gather per
//! viewport with no per-value branch. So an absent number has nowhere to go *in* the column and is
//! written as the type's zero, which is a legal value: a range containing zero would match every
//! item that has no value at all. 0064 declines the validity buffer, the declared sentinel and the
//! NaN payload, and rules that absence lives in a bitmap **beside** the column. This is that
//! bitmap for a segment's rows.
//!
//! **Row-indexed and dense, unlike the filter column's.** `attrs/<column>/presence.roaring`
//! compacts — an absent entity occupies no slot — while a render column keeps every row's slot and
//! the slot's contents are meaningless rather than zero-as-a-value. The bitmap is therefore over
//! `0..row_count` of the segment that owns it, and it is the segment's own row numbering: a merge
//! permutes rows within the merged span, so a bitmap carried across one without being permuted
//! with its rows describes the wrong rows entirely, silently, and in the direction that shows
//! absent items as present.
//!
//! **A category has no bitmap here, deliberately.** Its vocabulary reserves code 0 out of the
//! value space before any data exists, so it can already say "nothing" in the column itself;
//! giving it a second mechanism is the sentinel-and-bitmap muddle 0064 declines. The row-space
//! scan reads a category's absence from code 0 and a number's from this file, and those are the
//! only two rules.
//!
//! **An absent file means every row is present**, exactly as it does for a filter column — the
//! common case costs no bytes. The distinction that matters is between a file this segment never
//! needed and one the manifest names and cannot be found: the first is "all present", the second
//! refuses at open, like any digested artefact.

use std::path::Path;

use croaring::Bitmap;

use crate::error::{Result, StoreError};

/// The per-segment directory a render column's presence bitmap lives in, relative to the
/// segment's own directory. One file per column, named for the column.
pub const RENDER_PRESENCE_DIR: &str = "presence";

/// The prefix-relative path of one render column's presence bitmap within a segment directory.
pub fn render_presence_path(segment_dir: &Path, column: &str) -> std::path::PathBuf {
    segment_dir
        .join(RENDER_PRESENCE_DIR)
        .join(format!("{column}.roaring"))
}

/// One render column's presence over a segment's rows: which of `0..row_count` carry a value.
///
/// Constructed either from a file or from [`RenderPresence::all_present`], and the two are
/// deliberately indistinguishable to a reader — "no file" is a representation of "every row",
/// not a missing artefact, so nothing downstream branches on which it got.
#[derive(Debug, Clone)]
pub struct RenderPresence {
    rows: Option<Bitmap>,
}

impl RenderPresence {
    /// Every row carries a value — what a column with no absences, and therefore no file, means.
    pub fn all_present() -> Self {
        Self { rows: None }
    }

    /// Read a bitmap written by [`Self::serialise`]. A file that exists and does not parse is
    /// corruption and refuses; it is never read as "all present", which would turn a damaged
    /// artefact into an answer.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let rows = Bitmap::try_deserialize::<croaring::Portable>(&bytes).ok_or_else(|| {
            StoreError::MalformedBundle {
                detail: format!(
                    "{}: not a portable Roaring bitmap (decision 0064's render presence)",
                    path.display()
                ),
            }
        })?;
        Ok(Self { rows: Some(rows) })
    }

    /// Build from the rows that carry a value. Returns `None` when every row in `0..row_count`
    /// does — the caller writes no file in that case, which is what makes the common column free.
    pub fn from_present_rows(present: Bitmap, row_count: u32) -> Option<Self> {
        if present.cardinality() == u64::from(row_count) {
            return None;
        }
        Some(Self {
            rows: Some(present),
        })
    }

    /// The portable serialisation, stable across architectures for the same reason every other
    /// Roaring artefact here uses it.
    pub fn serialise(&self) -> Option<Vec<u8>> {
        self.rows
            .as_ref()
            .map(|b| b.serialize::<croaring::Portable>())
    }

    /// Whether `row` carries a value. A row outside the segment is not present, which is the
    /// fail-closed direction: a caller that has lost track of the segment's extent filters the
    /// row out rather than reading a neighbour's meaning into it.
    #[inline]
    pub fn contains(&self, row: u32) -> bool {
        match &self.rows {
            None => true,
            Some(bitmap) => bitmap.contains(row),
        }
    }

    /// The bitmap itself, for a scan that wants to intersect rather than test row by row; `None`
    /// when every row is present.
    pub fn bitmap(&self) -> Option<&Bitmap> {
        self.rows.as_ref()
    }

    /// The permutation a merge owes this bitmap: `new_row_of[old_row]` for every row of the
    /// segment being merged, producing the bitmap in the merged segment's numbering.
    ///
    /// **Merge permutes row space within the merged span** (`geometry-pinning.md` §4), so a
    /// bitmap carried across a merge unchanged describes rows that have moved — and the failure
    /// is silent and in the wrong direction, showing absent items as present wherever a present
    /// row's index lands on an absent one's. This exists so the permutation is one named
    /// operation rather than an open-coded loop in the merge, and so the test that checks it has
    /// something to name.
    pub fn permuted(&self, new_row_of: impl Fn(u32) -> u32) -> Self {
        match &self.rows {
            None => Self::all_present(),
            Some(bitmap) => {
                let mut moved = Bitmap::new();
                for old in bitmap.iter() {
                    moved.add(new_row_of(old));
                }
                Self { rows: Some(moved) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "No file" and "every row" are the same statement, and a reader must not be able to tell
    /// them apart — a column with no absences costs no bytes and no branch.
    #[test]
    fn a_column_with_no_absence_has_no_bitmap_and_every_row_is_present() {
        let mut all = Bitmap::new();
        all.add_range(0..8);
        assert!(
            RenderPresence::from_present_rows(all, 8).is_none(),
            "every row present is representable as the absence of a file"
        );

        let every = RenderPresence::all_present();
        assert!(every.serialise().is_none());
        assert!((0..8).all(|row| every.contains(row)));
        assert!(
            every.contains(9_999),
            "with no file there is no row to be absent from; the segment's extent bounds this, \
             not the bitmap"
        );
    }

    /// The round trip, and the rule that an absent row is absent — the whole point of the file.
    #[test]
    fn a_bitmap_round_trips_and_an_absent_row_is_absent() {
        let mut present = Bitmap::new();
        for row in [0u32, 1, 5, 7] {
            present.add(row);
        }
        let presence = RenderPresence::from_present_rows(present, 8).expect("row 2 is absent");
        let bytes = presence
            .serialise()
            .expect("a bitmap with absences serialises");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("score.roaring");
        std::fs::write(&path, bytes).expect("write");

        let read = RenderPresence::load(&path).expect("load");
        for row in [0u32, 1, 5, 7] {
            assert!(read.contains(row), "row {row} carries a value");
        }
        for row in [2u32, 3, 4, 6] {
            assert!(!read.contains(row), "row {row} carries none");
        }
    }

    /// A file that exists and does not parse is corruption, and must refuse rather than read as
    /// "all present" — which would turn a damaged artefact into an answer, and the answer would
    /// be that every absent item has a value.
    #[test]
    fn a_corrupt_bitmap_refuses_rather_than_reading_as_all_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("score.roaring");
        std::fs::write(&path, b"not a roaring bitmap").expect("write");
        assert!(RenderPresence::load(&path).is_err());
    }

    /// **The merge trap.** A merge permutes rows within the merged span, so a bitmap carried
    /// across one unchanged describes rows that have moved. The failure is silent and fails
    /// *open* — wherever a present row's old index lands on an absent row's new one, an item with
    /// no value starts matching ranges again, which is the August 11th defect by another route.
    #[test]
    fn a_permuted_bitmap_follows_its_rows_rather_than_their_indices() {
        // Rows 0 and 3 carry values; 1 and 2 do not. The merge reverses the segment.
        let mut present = Bitmap::new();
        present.add(0);
        present.add(3);
        let presence = RenderPresence::from_present_rows(present, 4).expect("two rows are absent");

        let reversed = presence.permuted(|old| 3 - old);
        assert!(reversed.contains(0), "old row 3's value is now at row 0");
        assert!(reversed.contains(3), "old row 0's value is now at row 3");
        assert!(
            !reversed.contains(1) && !reversed.contains(2),
            "the absent rows stay absent, at their new indices"
        );

        // Carried across unchanged, the same bitmap would still say rows 0 and 3 — which happens
        // to be right under this particular permutation and is the reason the test does not stop
        // here. Under a rotation it is wrong, and wrong in the fail-open direction.
        let rotated = presence.permuted(|old| (old + 1) % 4);
        assert!(rotated.contains(1) && rotated.contains(0));
        assert!(
            !rotated.contains(3),
            "row 3 is absent after the rotation; a bitmap that had not been permuted would still \
             call it present, and an item with no value would match a range containing zero"
        );
    }
}
