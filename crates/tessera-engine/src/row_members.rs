//! **A row-major level's members by artifact, and the coverings that say where each could be** —
//! the reading half of [`tessera_store::row_members`], held inside the
//! [`crate::row_column::RowColumn`] it was written from, so nothing can replace or drop the column
//! without them.
//!
//! Both are over the column's base rows and are viewer-independent: a member bitmap is every base
//! row the artifact holds, whoever is looking, and a covering is at most
//! [`tessera_store::derived::COVERING_RANGES`] row ranges holding them all. Neither is ever an
//! answer to a viewer. A covering proposes candidates, and every candidate is still tested against
//! the viewer's visible rows before anything about it is served.
//!
//! # Between folds
//!
//! A membership only grows between folds, so a growth adds rows and never takes them: the rows it
//! adds below the base are held beside the mapped file, per artifact, and the artifact's covering
//! is widened to hold them. A grown covering keeps at most `COVERING_RANGES` ranges: the new row
//! enters as a range of its own and the two neighbouring ranges with the narrowest gap between them
//! are merged until it fits. That covering holds more non-members than a fresh split would, and the
//! next fold writes the fresh one. Rows above the base are not here: they are the column's tail and
//! amendment, read from the column itself.
//!
//! # The covering index
//!
//! Derived on first use from the coverings, as the tile index's node hierarchy is derived from its
//! extents, and never stored. Ranges are bucketed by length into powers of two and sorted by start
//! within each bucket, so the ranges overlapping `[lo, hi]` in the bucket of lengths below `2ᵏ⁺¹`
//! all start inside `[lo − 2ᵏ⁺¹ + 1, hi]`, one binary search away, and at most half of what that
//! window holds can miss.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use croaring::{Bitmap, BitmapView};

use tessera_store::derived::COVERING_RANGES;
use tessera_store::row_members::RowMembersPack;

/// One level's members and coverings: the mapped file, and what growths have added since.
#[derive(Clone)]
pub struct LevelMembers {
    pack: Arc<RowMembersPack>,
    index: Arc<OnceLock<CoveringIndex>>,
    added: Option<Added>,
}

impl std::fmt::Debug for LevelMembers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelMembers")
            .field("pack", &self.pack)
            .field("grown", &self.added.as_ref().map_or(0, |a| a.rows.len()))
            .finish()
    }
}

/// What growths have added below the base since the file was written.
#[derive(Default)]
struct Added {
    rows: BTreeMap<u32, Bitmap>,
    /// The whole covering of every artifact a growth widened, in place of the file's.
    coverings: BTreeMap<u32, Vec<(u32, u32)>>,
    /// Built on first use and dropped by every growth.
    index: OnceLock<CoveringIndex>,
}

impl Clone for Added {
    fn clone(&self) -> Self {
        Added {
            rows: self.rows.clone(),
            coverings: self.coverings.clone(),
            index: OnceLock::new(),
        }
    }
}

/// One artifact's members: the mapped bitmap and what growths added to it, as one set.
pub struct Members<'a> {
    base: Option<BitmapView<'a>>,
    added: Option<&'a Bitmap>,
}

impl Members<'_> {
    pub fn cardinality(&self) -> u64 {
        self.base.as_ref().map_or(0, |b| b.cardinality())
            + self.added.map_or(0, |a| a.cardinality())
    }

    pub fn is_empty(&self) -> bool {
        self.cardinality() == 0
    }

    /// How many members are in `rows`. The two halves are disjoint, so their counts add.
    pub fn and_cardinality(&self, rows: &Bitmap) -> u64 {
        self.base.as_ref().map_or(0, |b| b.and_cardinality(rows))
            + self.added.map_or(0, |a| a.and_cardinality(rows))
    }

    /// Whether any member is in `rows`.
    pub fn intersects(&self, rows: &Bitmap) -> bool {
        self.base.as_ref().is_some_and(|b| b.intersect(rows))
            || self.added.is_some_and(|a| a.intersect(rows))
    }

    /// The members as one owned bitmap.
    pub fn to_bitmap(&self) -> Bitmap {
        let mut out = self
            .base
            .as_ref()
            .map_or_else(Bitmap::new, |b| b.to_bitmap());
        if let Some(added) = self.added {
            out.or_inplace(added);
        }
        out
    }
}

impl LevelMembers {
    pub fn new(pack: RowMembersPack) -> Self {
        LevelMembers {
            pack: Arc::new(pack),
            index: Arc::new(OnceLock::new()),
            added: None,
        }
    }

    /// The base rows the members are over.
    pub fn base_rows(&self) -> u32 {
        self.pack.rows()
    }

    /// The artifact's members over the base rows.
    pub fn members(&self, ordinal: u32) -> Members<'_> {
        Members {
            base: self.pack.members(ordinal),
            added: self.added.as_ref().and_then(|a| a.rows.get(&ordinal)),
        }
    }

    /// The artifact's covering, ascending. Empty for an artifact with no members.
    pub fn covering(&self, ordinal: u32) -> Vec<(u32, u32)> {
        match self.added.as_ref().and_then(|a| a.coverings.get(&ordinal)) {
            Some(grown) => grown.clone(),
            None => self.pack.covering(ordinal).collect(),
        }
    }

    /// **Every artifact whose covering overlaps `rows`**, ascending — the candidates for a tile
    /// whose rows are `rows`. A superset of the artifacts with a member there, and never an answer:
    /// each one still needs a visible member in the tile before it is served.
    pub fn overlapping(&self, rows: RangeInclusive<u32>) -> Bitmap {
        let (lo, hi) = (*rows.start(), *rows.end());
        let mut out = Vec::new();
        if lo <= hi {
            self.index
                .get_or_init(|| {
                    CoveringIndex::build((0..self.pack.ordinals()).flat_map(|ordinal| {
                        self.pack
                            .covering(ordinal)
                            .map(move |(lo, hi)| (lo, hi, ordinal))
                    }))
                })
                .query(lo, hi, &mut out);
            // A grown covering holds the file's, so a hit from the file's index is still a hit,
            // and only the grown coverings need asking besides.
            if let Some(added) = &self.added {
                added
                    .index
                    .get_or_init(|| {
                        CoveringIndex::build(added.coverings.iter().flat_map(
                            |(&ordinal, ranges)| {
                                ranges.iter().map(move |&(lo, hi)| (lo, hi, ordinal))
                            },
                        ))
                    })
                    .query(lo, hi, &mut out);
            }
        }
        let mut found = Bitmap::new();
        found.add_many(&out);
        found
    }

    /// Every artifact's base members as an owned bitmap, `ordinals` of them: what a level whose
    /// layer derives hulls holds artifact-major. `None` where a growth has added rows, since the
    /// column's own amendment is then what a reader must take them from.
    pub fn base_bitmaps(&self, ordinals: usize) -> Option<Vec<Bitmap>> {
        if self.added.is_some() {
            return None;
        }
        Some(
            (0..ordinals as u32)
                .map(|ordinal| {
                    self.pack
                        .members(ordinal)
                        .map_or_else(Bitmap::new, |view| view.to_bitmap())
                })
                .collect(),
        )
    }

    /// Add `pairs` — `(row, ordinal)` the column did not carry, none of them already a member — to
    /// the members and coverings. Rows at or above the base are the column's to hold, and are passed
    /// over here.
    pub(crate) fn grow(&mut self, pairs: &[(u32, u32)]) {
        let base_rows = self.pack.rows();
        let mut by_ordinal: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &(row, ordinal) in pairs {
            if row < base_rows {
                by_ordinal.entry(ordinal).or_default().push(row);
            }
        }
        if by_ordinal.is_empty() {
            return;
        }
        let pack = Arc::clone(&self.pack);
        let added = self.added.get_or_insert_with(Added::default);
        added.index = OnceLock::new();
        for (ordinal, rows) in by_ordinal {
            added.rows.entry(ordinal).or_default().add_many(&rows);
            let covering = added
                .coverings
                .entry(ordinal)
                .or_insert_with(|| pack.covering(ordinal).collect());
            widen(covering, &rows);
        }
    }
}

/// Widen `covering` to hold `rows`, keeping at most [`COVERING_RANGES`] ranges: each row outside it
/// enters as a range of its own, and the neighbours with the narrowest gap are merged until it
/// fits. Only ever widens, so every row it held before it still holds.
fn widen(covering: &mut Vec<(u32, u32)>, rows: &[u32]) {
    for &row in rows {
        let at = covering.partition_point(|&(_, hi)| hi < row);
        if covering.get(at).is_some_and(|&(lo, _)| lo <= row) {
            continue;
        }
        covering.insert(at, (row, row));
    }
    while covering.len() > COVERING_RANGES {
        let narrowest = (0..covering.len() - 1)
            .min_by_key(|&i| covering[i + 1].0 - covering[i].1)
            .expect("more than one range");
        covering[narrowest].1 = covering[narrowest + 1].1;
        covering.remove(narrowest + 1);
    }
}

/// The coverings' ranges, bucketed by length into powers of two and sorted by start in each.
struct CoveringIndex {
    /// Bucket `k` holds the ranges of `2ᵏ` to `2ᵏ⁺¹ − 1` rows, as `(lo, hi, ordinal)`.
    buckets: Vec<Vec<(u32, u32, u32)>>,
}

impl CoveringIndex {
    fn build(ranges: impl Iterator<Item = (u32, u32, u32)>) -> Self {
        let mut buckets: Vec<Vec<(u32, u32, u32)>> = vec![Vec::new(); 33];
        for range in ranges {
            let length = u64::from(range.1 - range.0) + 1;
            buckets[length.ilog2() as usize].push(range);
        }
        for bucket in &mut buckets {
            bucket.sort_unstable();
        }
        CoveringIndex { buckets }
    }

    /// Push the ordinal of every range overlapping `[lo, hi]` onto `out`, repeats allowed.
    fn query(&self, lo: u32, hi: u32, out: &mut Vec<u32>) {
        for (k, bucket) in self.buckets.iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            // A range here is shorter than 2ᵏ⁺¹ rows, so one reaching `lo` starts after this.
            let reach = (1u64 << (k + 1)) - 1;
            let from = u32::try_from(u64::from(lo).saturating_sub(reach)).unwrap_or(0);
            let start = bucket.partition_point(|&(range_lo, _, _)| range_lo < from);
            for &(range_lo, range_hi, ordinal) in &bucket[start..] {
                if range_lo > hi {
                    break;
                }
                if range_hi >= lo {
                    out.push(ordinal);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The index finds exactly the artifacts a scan of every range finds, for every query window.
    #[test]
    fn the_covering_index_finds_what_a_scan_of_every_range_finds() {
        let mut ranges = Vec::new();
        let mut state = 7u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for ordinal in 0..300u32 {
            for _ in 0..(next() % 5) {
                let lo = next() % 100_000;
                let length = 1u32 << (next() % 17);
                ranges.push((lo, lo.saturating_add(next() % length), ordinal));
            }
        }
        ranges.push((0, u32::MAX - 1, 300));
        let index = CoveringIndex::build(ranges.iter().copied());
        for _ in 0..2_000 {
            let lo = next() % 110_000;
            let hi = lo + next() % (1 << (next() % 16));
            let mut found = Vec::new();
            index.query(lo, hi, &mut found);
            found.sort_unstable();
            found.dedup();
            let mut scanned: Vec<u32> = ranges
                .iter()
                .filter(|&&(rlo, rhi, _)| rlo <= hi && rhi >= lo)
                .map(|&(_, _, o)| o)
                .collect();
            scanned.sort_unstable();
            scanned.dedup();
            assert_eq!(found, scanned, "the window [{lo}, {hi}]");
        }
    }

    /// A widened covering holds every row it held and every row added, in at most
    /// `COVERING_RANGES` ascending, disjoint ranges.
    #[test]
    fn a_widened_covering_holds_old_and_new_rows_within_its_bound() {
        let mut covering: Vec<(u32, u32)> = (0..COVERING_RANGES as u32)
            .map(|i| (i * 100, i * 100 + 10))
            .collect();
        let before = covering.clone();
        let rows = [5u32, 50, 51, 99, 4_000, 7_777, 3_150];
        widen(&mut covering, &rows);
        assert!(covering.len() <= COVERING_RANGES);
        assert!(covering.windows(2).all(|w| w[0].1 < w[1].0));
        let holds = |row: u32| covering.iter().any(|&(lo, hi)| lo <= row && row <= hi);
        for row in rows {
            assert!(holds(row), "row {row}");
        }
        for (lo, hi) in before {
            assert!(holds(lo) && holds(hi));
        }
    }
}
