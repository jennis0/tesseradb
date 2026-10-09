//! One filterable column's postings, addressed by an ordinal local to this column.
//!
//! A value with no record is an ordinary empty answer at every level, never a failure.
//!
//! **A resolved set is not a filter result.** What this module returns is the entities carrying a
//! value over the whole corpus: it has not been intersected with the viewer's authorised set and
//! it still contains suppressed and deleted-but-unfolded entities, because a suppression never
//! touches postings. The caller composes it with the deny state and the authorised set, and
//! nothing here exposes a cardinality over an unmasked set.

use std::io;
use std::path::Path;

use croaring::{Bitmap, BitmapView};
use mosaica_authz::{DeltaTier, PostingRef, PostingsReader};
use mosaica_types::AttrLocalId;

/// A column's postings file, in whichever record format its identifier domain wants.
///
/// A positional file's record ordinal is the identifier, which suits dense identifiers: a string
/// column's intern-order ordinals, a bit-sliced column's slice indices. A category's codes are
/// drawn at random over the declared width, so a positional file would need one record per code
/// point: 64 KB of empty records for a `u16` and 4×10⁹ for a `u32`. The keyed format carries the
/// identifier in the record and is found by binary search, so it costs 4 bytes per record and
/// nothing per absent code.
///
/// The keyed format is read through `DeltaTier`, which validates ascending keys and every record
/// at open, maps the file without copying and binary-searches; none of that assumes the file is
/// small.
#[derive(Debug)]
enum BaseTier {
    /// Record ordinal is the identifier. Dense domains.
    Positional(PostingsReader),
    /// `(identifier, posting)` ascending, found by binary search. Scattered domains.
    Keyed(DeltaTier),
}

impl BaseTier {
    fn posting_at(&self, ordinal: u32) -> io::Result<Option<PostingRef<'_>>> {
        match self {
            BaseTier::Positional(reader) => reader.posting_at(ordinal),
            BaseTier::Keyed(tier) => tier.posting_at(ordinal),
        }
    }

    fn record_count(&self) -> u32 {
        match self {
            BaseTier::Positional(reader) => reader.term_count(),
            BaseTier::Keyed(tier) => tier.term_count(),
        }
    }
}

/// One column's postings.
///
/// Each column owns its own identifier space, so an `AttrLocalId` means nothing without the column
/// it belongs to, and no method here takes a column name: the reader is the column.
#[derive(Debug)]
pub struct ColumnPostings {
    base: BaseTier,
}

impl ColumnPostings {
    /// Open a column whose identifiers are dense positions: a string column's interned values, a
    /// numeric column's level 0, a bit-sliced column's slices.
    pub fn open(base_path: &Path, mmap: bool) -> io::Result<Self> {
        Ok(ColumnPostings {
            base: BaseTier::Positional(PostingsReader::open(base_path, mmap)?),
        })
    }

    /// Open a column whose identifiers are scattered: a category, addressed by its vocabulary
    /// code.
    ///
    /// The file is opened with its bucket table ([`DeltaTier::open_indexed`]): a suggestion walk
    /// searches this array up to `max_suggestion_walk` times per keystroke, and without the table
    /// the search was 68–72% of the walk at 10⁷ records.
    pub fn open_keyed(base_path: &Path) -> io::Result<Self> {
        Ok(ColumnPostings {
            base: BaseTier::Keyed(DeltaTier::open_indexed(base_path)?),
        })
    }

    /// The number of records the file carries.
    ///
    /// A record count and never the column's identifier domain. For a keyed column the two are
    /// unrelated (a category holding one value with code 40,000 has a count of 1), and for a
    /// positional one a value bound since the last build sits at an ordinal at or above this.
    /// Nothing may enumerate a column's values by walking `0..record_count()`; the vocabulary
    /// table and the dictionary know the domain.
    pub fn record_count(&self) -> u32 {
        self.base.record_count()
    }

    /// The entities carrying `value`.
    pub fn entities(&self, value: AttrLocalId) -> io::Result<Bitmap> {
        resolve_union(self, &[value])
    }

    /// `candidate ∩ (the entities carrying value)`, without materialising the posting.
    ///
    /// A posting is a corpus-wide set, and at 10⁹ entities a common term's posting is hundreds of
    /// megabytes. The postings file is mapped and `croaring` reads the portable format as a view
    /// over those bytes, so intersecting against the view leaves the corpus-wide set in the page
    /// cache and allocates only the answer, which is bounded by `candidate`. A tag-0 record is a
    /// sorted array of at most `small_term_threshold` entities and is tested one by one.
    ///
    /// Neither this nor [`Self::narrow_inplace`] run-optimises: they are chained, and the caller
    /// optimises once at the end if the result goes anywhere that cares.
    pub fn narrow(&self, value: AttrLocalId, candidate: &Bitmap) -> io::Result<Bitmap> {
        // A principal who can see none of this view reads no posting bytes at all.
        if candidate.is_empty() {
            return Ok(Bitmap::new());
        }
        Ok(match self.base.posting_at(value.raw())? {
            None => Bitmap::new(),
            Some(PostingRef::Roaring(view)) => candidate.and(&view),
            Some(PostingRef::Array(bytes)) => {
                let mut out = Bitmap::new();
                for chunk in bytes.as_chunks::<4>().0 {
                    let entity = u32::from_le_bytes(*chunk);
                    if candidate.contains(entity) {
                        out.add(entity);
                    }
                }
                out
            }
        })
    }

    /// Whether any entity in `candidate` carries `value`, stopping at the first container the two
    /// share and never materialising the posting.
    ///
    /// `/v1/categories` and suggest ask this of a `derived` column once per value walked.
    /// `Bitmap::intersect` against the mapped view answers without allocating, so a hidden value
    /// costs the container keys the two sets share and nothing more: 0.15–16.6 µs against 0.1–0.6
    /// ms through a materialising route, measured on a visible head value under a scattered
    /// candidate at 10⁷ values over 10⁸ entities.
    pub fn intersects(&self, value: AttrLocalId, candidate: &Bitmap) -> io::Result<bool> {
        if candidate.is_empty() {
            return Ok(false);
        }
        Ok(self
            .base
            .posting_at(value.raw())?
            .is_some_and(|posting| hits(&posting, candidate)))
    }

    /// `|members(value) ∩ candidate|` against the mapped view, without materialising the
    /// intersection.
    pub fn intersection_cardinality(
        &self,
        value: AttrLocalId,
        candidate: &Bitmap,
    ) -> io::Result<u64> {
        if candidate.is_empty() {
            return Ok(0);
        }
        Ok(match self.base.posting_at(value.raw())? {
            None => 0,
            Some(PostingRef::Roaring(view)) => candidate.and_cardinality(&view),
            Some(PostingRef::Array(bytes)) => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|chunk| candidate.contains(u32::from_le_bytes(**chunk)))
                .count() as u64,
        })
    }

    /// [`Self::narrow`] with the running set narrowed in place: `live ∩= the value's entities`.
    ///
    /// Chaining `narrow` allocates a fresh bitmap at every step, and over a conjunction of common
    /// terms that allocation measured 19–51% slower on a full-coverage principal intersecting head
    /// terms. `and_inplace` mutates the running set's containers and allocates nothing.
    pub fn narrow_inplace(&self, value: AttrLocalId, live: &mut Bitmap) -> io::Result<()> {
        if live.is_empty() {
            return Ok(());
        }
        match self.base.posting_at(value.raw())? {
            None => live.clear(),
            Some(PostingRef::Roaring(view)) => live.and_inplace(&view),
            Some(PostingRef::Array(bytes)) => {
                let mut small = Bitmap::new();
                for chunk in bytes.as_chunks::<4>().0 {
                    small.add(u32::from_le_bytes(*chunk));
                }
                live.and_inplace(&small);
            }
        }
        Ok(())
    }
    /// [`Self::intersects`]' first stage alone, over the base tier: the binary search that turns a
    /// scattered code into a record ordinal. `None` for a code with no base record, and `None`
    /// for a positional base, which has no search to do.
    ///
    /// The three `bench_*` entries exist so the walk's constant can be decomposed without
    /// transcribing `intersects` — each calls the same code the shipped path calls
    /// (`probes/2026-09-02-value-suggestion/`, the decomposition arm).
    #[cfg(feature = "bench-timing")]
    pub fn bench_base_record_index(&self, value: AttrLocalId) -> Option<usize> {
        match &self.base {
            BaseTier::Keyed(tier) => tier.bench_record_index(value.raw()),
            BaseTier::Positional(_) => None,
        }
    }

    /// The second stage alone: the borrowed view over the mapped record's bytes.
    #[cfg(feature = "bench-timing")]
    pub fn bench_base_posting_at_index(&self, idx: usize) -> io::Result<PostingRef<'_>> {
        match &self.base {
            BaseTier::Keyed(tier) => tier.bench_posting_at_index(idx),
            BaseTier::Positional(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "bench_base_posting_at_index is for a keyed base",
            )),
        }
    }

    /// The third stage alone: the existential test against the candidate.
    #[cfg(feature = "bench-timing")]
    pub fn bench_hits(posting: &PostingRef<'_>, candidate: &Bitmap) -> bool {
        hits(posting, candidate)
    }
}

/// Does one source share an entity with `candidate`?
///
/// The Roaring arm is a view over the mapped file's bytes, so this reads the containers the two
/// sets have keys in common and stops at the first coincidence. The tag-0 arm is bounded by
/// `small_term_threshold` — a handful of `contains` probes rather than a bitmap of its own.
fn hits(posting: &PostingRef<'_>, candidate: &Bitmap) -> bool {
    match posting {
        PostingRef::Roaring(view) => candidate.intersect(view),
        PostingRef::Array(bytes) => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .any(|chunk| candidate.contains(u32::from_le_bytes(*chunk))),
    }
}

/// The entities carrying any of `values`.
///
/// This is set membership — `severity IN (high, critical)` — and equality is its one-element case.
/// Composition with other operands is intersection and happens above this (§8.2); a union here and
/// an intersection there is what keeps every filter an order-independent set producer.
pub fn resolve_union(column: &ColumnPostings, values: &[AttrLocalId]) -> io::Result<Bitmap> {
    let mut views: Vec<BitmapView<'_>> = Vec::new();
    let mut small: Vec<u32> = Vec::new();

    for value in values.iter().copied() {
        // `None` is a value bound since the build (positional) or a code with no members (keyed).
        if let Some(posting) = column.base.posting_at(value.raw())? {
            match posting {
                PostingRef::Roaring(view) => views.push(view),
                PostingRef::Array(bytes) => {
                    // Tag-0 payload lengths are validated as a multiple of 4 once, at
                    // `PostingsReader::open`. A violation here would mean that validation was
                    // bypassed, not that this site needs its own fail-closed handling.
                    debug_assert!(
                        bytes.len() % 4 == 0,
                        "tag-0 posting payload length must be a multiple of 4 (validated at \
                         PostingsReader::open)"
                    );
                    for chunk in bytes.as_chunks::<4>().0 {
                        small.push(u32::from_le_bytes(*chunk));
                    }
                }
            }
        }
    }

    let refs: Vec<&Bitmap> = views.iter().map(|view| &**view).collect();
    let mut out = if refs.is_empty() {
        Bitmap::new()
    } else {
        Bitmap::fast_or(&refs)
    };

    small.sort_unstable();
    out.add_many(&small);
    out.run_optimize();

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaica_authz::{write_delta_tier_at, write_postings};

    const SMALL: u32 = 32;

    fn column_with(per_value: &[Vec<u32>], dir: &Path) -> ColumnPostings {
        let base = dir.join("postings.arrow");
        write_postings(&base, per_value, SMALL).unwrap();
        ColumnPostings::open(&base, false).unwrap()
    }

    /// **`narrow` answers exactly what intersecting afterwards answers**, across every shape the
    /// two encodings produce.
    ///
    /// The whole point of `narrow` is that it never assembles the corpus-wide posting, so its
    /// agreement with the obvious construction is the thing that has to be checked rather than
    /// assumed — and checked at the boundary between the encodings, since the two are read by
    /// different code (`small_term_threshold` decides which, and a value either side of it takes a
    /// different arm).
    ///
    /// **Mutations this kills:** unioning the tag-0 array without testing membership.
    #[test]
    fn narrowing_is_the_same_answer_as_intersecting_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        // Value 0 is tag-0 (small, an array); value 1 is over the threshold and tag-1 (a Roaring
        // view); value 2 is empty; value 3 sits exactly on the boundary.
        let wide: Vec<u32> = (0..500).map(|i| i * 3).collect();
        let boundary: Vec<u32> = (0..SMALL).collect();
        let column = column_with(
            &[vec![1, 2, 3, 900], wide.clone(), vec![], boundary.clone()],
            dir.path(),
        );

        let candidates = [
            Bitmap::new(),
            Bitmap::of(&[2]),
            Bitmap::of(&[1, 3, 900, 6, 12, 4_000]),
            Bitmap::from_range(0..1_500),
            Bitmap::of(&[999_999]),
        ];
        for (value, expected_source) in [
            (0u32, vec![1u32, 2, 3, 900]),
            (1, wide.clone()),
            (2, vec![]),
            (3, boundary.clone()),
        ] {
            let whole = Bitmap::of(&expected_source);
            for candidate in &candidates {
                let narrowed = column.narrow(AttrLocalId::new(value), candidate).unwrap();
                assert_eq!(
                    narrowed,
                    whole.and(candidate),
                    "value {value} against a candidate of {} entities",
                    candidate.cardinality()
                );
                // And the answer never names an entity the candidate did not, which is the
                // property the request path leans on.
                assert!(narrowed.andnot(candidate).is_empty());
            }
        }
    }

    /// **In-place narrowing agrees with out-of-place**, over the same shapes.
    ///
    /// The two take different code — one `and_inplace` against the mapped view, the other the
    /// distributive union — and only one of them is on the conjunction's hot path, so a divergence
    /// would show as a wrong answer for multi-word queries and a right one for single-word.
    #[test]
    fn narrowing_in_place_agrees_with_narrowing_out_of_place() {
        let dir = tempfile::tempdir().unwrap();
        let wide: Vec<u32> = (0..500).map(|i| i * 3).collect();
        let column = column_with(&[vec![1, 2, 3, 900], wide, vec![]], dir.path());

        for value in [0u32, 1, 2, 7] {
            for candidate in [
                Bitmap::new(),
                Bitmap::of(&[2]),
                Bitmap::of(&[1, 3, 900, 6, 12, 4_000]),
                Bitmap::from_range(0..1_500),
            ] {
                let mut live = candidate.clone();
                column
                    .narrow_inplace(AttrLocalId::new(value), &mut live)
                    .unwrap();
                assert_eq!(
                    live,
                    column.narrow(AttrLocalId::new(value), &candidate).unwrap(),
                    "value {value} against {} entities",
                    candidate.cardinality()
                );
            }
        }

        // And a chain of them is the conjunction — the property the request path is built on.
        let mut live = Bitmap::from_range(0..1_500);
        column.narrow_inplace(AttrLocalId::new(0), &mut live).unwrap();
        column.narrow_inplace(AttrLocalId::new(1), &mut live).unwrap();
        let a = column.entities(AttrLocalId::new(0)).unwrap();
        let b = column.entities(AttrLocalId::new(1)).unwrap();
        assert_eq!(live, a.and(&b).and(&Bitmap::from_range(0..1_500)));
    }

    /// `intersects` answers exactly `!narrow(..).is_empty()`, and `intersection_cardinality`
    /// answers `narrow(..).cardinality()`, across both encodings, an absent record and an empty
    /// candidate.
    #[test]
    fn intersects_and_counts_agree_with_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        let wide: Vec<u32> = (0..500).map(|i| i * 3).collect();
        let boundary: Vec<u32> = (0..SMALL).collect();
        let column = column_with(&[vec![1, 2, 3, 900], wide, vec![], boundary], dir.path());

        let candidates = [
            Bitmap::new(),
            Bitmap::of(&[2]),
            Bitmap::of(&[20]),
            Bitmap::of(&[1, 3, 900, 6, 12, 4_000]),
            Bitmap::from_range(0..1_500),
            Bitmap::of(&[999_999]),
        ];
        for value in [0u32, 1, 2, 3, 50] {
            for candidate in &candidates {
                let id = AttrLocalId::new(value);
                let narrowed = column.narrow(id, candidate).unwrap();
                assert_eq!(
                    column.intersects(id, candidate).unwrap(),
                    !narrowed.is_empty(),
                    "value {value} against {} entities",
                    candidate.cardinality()
                );
                assert_eq!(
                    column.intersection_cardinality(id, candidate).unwrap(),
                    narrowed.cardinality(),
                    "cardinality, value {value} against {} entities",
                    candidate.cardinality()
                );
            }
        }
    }

    /// The keyed base — a category's own shape — takes the same agreement, including the code with
    /// no record at all, which is what "no members" is spelled as (index §2.5).
    #[test]
    fn intersects_agrees_over_a_keyed_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_delta_tier_at(
            &base,
            &[(7, vec![1, 2]), (40_000, vec![3]), (65_535, vec![4, 5])],
            SMALL,
        )
        .unwrap();
        let column = ColumnPostings::open_keyed(&base).unwrap();

        for code in [0u32, 7, 8, 40_000, 65_535] {
            for candidate in [
                Bitmap::new(),
                Bitmap::of(&[3]),
                Bitmap::of(&[1, 4]),
                Bitmap::from_range(0..10),
                Bitmap::of(&[99]),
            ] {
                let id = AttrLocalId::new(code);
                assert_eq!(
                    column.intersects(id, &candidate).unwrap(),
                    !column.narrow(id, &candidate).unwrap().is_empty(),
                    "code {code} against {} entities",
                    candidate.cardinality()
                );
            }
        }
    }

    #[test]
    fn equality_returns_the_values_members() {
        let dir = tempfile::tempdir().unwrap();
        let column = column_with(&[vec![1, 2, 3], vec![7], vec![]], dir.path());

        assert_eq!(
            column.entities(AttrLocalId::new(0)).unwrap().to_vec(),
            vec![1, 2, 3]
        );
        assert_eq!(column.entities(AttrLocalId::new(1)).unwrap().to_vec(), vec![7]);
        assert!(column.entities(AttrLocalId::new(2)).unwrap().is_empty());
    }

    #[test]
    fn set_membership_is_the_union() {
        let dir = tempfile::tempdir().unwrap();
        let column = column_with(&[vec![1, 2], vec![2, 9], vec![100]], dir.path());

        let got = resolve_union(&column, &[AttrLocalId::new(0), AttrLocalId::new(1)]).unwrap();
        assert_eq!(got.to_vec(), vec![1, 2, 9], "union, not concatenation");
    }

    /// An ordinal past the base's records is an ordinary empty answer, not an error.
    #[test]
    fn an_ordinal_beyond_the_base_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let column = column_with(&[vec![1]], dir.path());

        assert!(column.entities(AttrLocalId::new(50)).unwrap().is_empty());
    }

    /// A category's base is keyed by its vocabulary code, and codes are drawn at random over the
    /// declared width — so the identifiers are scattered, not dense. This is the case a positional
    /// base cannot represent at all: `u16` code 40,000 would need 40,001 records.
    #[test]
    fn a_keyed_base_addresses_scattered_codes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_delta_tier_at(
            &base,
            &[(7, vec![1, 2]), (40_000, vec![3]), (65_535, vec![4, 5])],
            SMALL,
        )
        .unwrap();
        let column = ColumnPostings::open_keyed(&base).unwrap();

        assert_eq!(
            column.entities(AttrLocalId::new(40_000)).unwrap().to_vec(),
            vec![3]
        );
        assert_eq!(
            column.entities(AttrLocalId::new(65_535)).unwrap().to_vec(),
            vec![4, 5]
        );
        assert_eq!(
            column.record_count(),
            3,
            "three records, not a domain spanning the width"
        );
    }

    /// A code with no members has no record, and index §2.5 turns on that meaning "no members" and
    /// nothing else — so an unheld code must answer empty rather than error, exactly as a positional
    /// file's out-of-range ordinal does.
    #[test]
    fn an_unheld_code_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_delta_tier_at(&base, &[(7, vec![1])], SMALL).unwrap();
        let column = ColumnPostings::open_keyed(&base).unwrap();

        assert!(column.entities(AttrLocalId::new(8)).unwrap().is_empty());
        assert!(column.entities(AttrLocalId::new(0)).unwrap().is_empty());
    }

    /// Set membership over scattered codes — the shape `severity IN (high, critical)` takes when the
    /// two codes are nowhere near each other in the width.
    #[test]
    fn set_membership_spans_scattered_codes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_delta_tier_at(&base, &[(3, vec![1]), (50_000, vec![2, 3])], SMALL).unwrap();
        let column = ColumnPostings::open_keyed(&base).unwrap();

        let got =
            resolve_union(&column, &[AttrLocalId::new(3), AttrLocalId::new(50_000)]).unwrap();
        assert_eq!(got.to_vec(), vec![1, 2, 3]);
    }

    /// Tag-1 (Roaring) and tag-0 (raw array) postings union together, so the union must not depend
    /// on which side of `small_term_threshold` a value's cardinality lands.
    #[test]
    fn the_union_spans_both_record_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let big: Vec<u32> = (0..(SMALL + 10)).collect();
        let column = column_with(&[big.clone(), vec![9_999]], dir.path());

        let got = resolve_union(&column, &[AttrLocalId::new(0), AttrLocalId::new(1)]).unwrap();
        let mut want = big;
        want.push(9_999);
        assert_eq!(got.to_vec(), want);
    }
}
