//! One filterable column's postings: a base tier plus its live delta tiers, addressed by an
//! ordinal local to this column.
//!
//! The read is `base ∪ every live tier`, which is the authorisation index's shape and is the same
//! shape for the same reason: a flush publishes a *sparse* tier carrying only the values its
//! flushed items touched, so an ordinal a tier does not hold is one it contributes nothing for.
//! `None` is an ordinary answer at every level, never a failure.
//!
//! **A resolved set is not a filter result.** What this module returns is the entities carrying a
//! value, over the whole corpus — it has not met `M_auth` and it still contains suppressed and
//! deleted-but-unfolded entities, because a suppression never touches postings (write-path §5.4,
//! Rule S). Composing it with the deny state, and intersecting with the authorised set, belong to
//! the caller and are specified in `filter-surface.md` §5. Returning an unmasked set is deliberate:
//! §8.2 accepts un-intersected intermediates internally and forbids only exposing a cardinality on
//! one.

use std::io;
use std::path::Path;
use std::sync::Arc;

use croaring::{Bitmap, BitmapView};
use tessera_authz::{DeltaTier, PostingRef, PostingsReader};
use tessera_types::AttrLocalId;

/// A column's base tier, in whichever record format its identifier domain wants (index §2.5).
///
/// **The format is a property of the domain, not a tuning choice.** A positional file's record
/// ordinal *is* the identifier, which is right when identifiers are dense — a string column's
/// intern-order ordinals, a bit-sliced column's slice indices. A category's codes are drawn at
/// random over the declared width (per-point-attributes §3.4), so a positional file would need one
/// record per code point: 64 KB of empty records for a `u16` and 4×10⁹ for a `u32`, which is not a
/// format. The keyed format carries the identifier in the record and is found by binary search, so
/// it costs 4 bytes per record and nothing per absent code.
///
/// Reading a `DeltaTier` as a *base* is deliberate and not a borrowed shape: it validates ascending
/// keys and every record at open, mmaps zero-copy and binary-searches, none of which assumes the
/// file is small. The one thing that does is `coalesce_delta_tiers`, which reads its inputs whole
/// because a tier holds one flush's arrivals — so the fold over a keyed base must be a streaming
/// sweep and must not reuse it (index §6.2).
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

/// One column's postings across every live tier.
///
/// Opening is per column rather than per bundle because each column owns its own identifier space
/// (`docs/design/filter-index.md` §2.2) — an `AttrLocalId` means nothing without the column it
/// belongs to, which is why no method here takes a column name: the reader *is* the column.
#[derive(Debug)]
pub struct ColumnPostings {
    base: BaseTier,
    tiers: Vec<Arc<DeltaTier>>,
}

impl ColumnPostings {
    /// Open a column whose identifiers are dense positions — a string column's interned values, a
    /// numeric column's level 0, a bit-sliced column's slices (index §2.5).
    pub fn open(base_path: &Path, mmap: bool) -> io::Result<Self> {
        Ok(ColumnPostings {
            base: BaseTier::Positional(PostingsReader::open(base_path, mmap)?),
            tiers: Vec::new(),
        })
    }

    /// Open a column whose identifiers are scattered — a category, addressed by its vocabulary
    /// code (index §2.5).
    pub fn open_keyed(base_path: &Path) -> io::Result<Self> {
        Ok(ColumnPostings {
            base: BaseTier::Keyed(DeltaTier::open(base_path)?),
            tiers: Vec::new(),
        })
    }

    /// Attach the live delta tiers, in serving order.
    pub fn with_tiers(mut self, tiers: Vec<Arc<DeltaTier>>) -> Self {
        self.tiers = tiers;
        self
    }

    /// The number of records the base tier carries.
    ///
    /// **A record count, and never the column's identifier domain.** For a keyed column the two are
    /// unrelated — a category holding one value with code 40,000 has a count of 1 — and even for a
    /// positional one a value bound since the last build lives only in a delta tier, at an ordinal
    /// at or above this. Nothing may enumerate a column's values by walking `0..record_count()`;
    /// the vocabulary table and the dictionary are what know the domain.
    pub fn record_count(&self) -> u32 {
        self.base.record_count()
    }

    /// The entities carrying `value`, unioned across the base and every live tier.
    pub fn entities(&self, value: AttrLocalId) -> io::Result<Bitmap> {
        resolve_union(self, &[value])
    }
}

/// The entities carrying **any** of `values`, unioned across the base and every live tier.
///
/// This is set membership — `severity IN (high, critical)` — and equality is its one-element case.
/// Composition with other operands is intersection and happens above this (§8.2); a union here and
/// an intersection there is what keeps every filter an order-independent set producer.
pub fn resolve_union(column: &ColumnPostings, values: &[AttrLocalId]) -> io::Result<Bitmap> {
    let mut views: Vec<BitmapView<'_>> = Vec::new();
    let mut small: Vec<u32> = Vec::new();

    // One loop over one `PostingRef` shape for the base and the tiers alike: the two files differ
    // in how a record is *found* — an ordinal index against a binary search — and not in what a
    // posting is. The union does not care which file an entity came from.
    for value in values.iter().copied() {
        let base = column.base.posting_at(value.raw())?;
        // Both formats answer `None` for an identifier they do not hold, and it is an ordinary
        // answer in both — a positional file because a value bound since the build sits past its
        // records, a keyed file because a code with no members has no record at all (index §2.5).
        // Neither is an error, and treating one as such would refuse a legitimate empty operand.
        for posting in base.into_iter().chain(
            column
                .tiers
                .iter()
                .map(|tier| tier.posting_at(value.raw()))
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .flatten(),
        ) {
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
                    for chunk in bytes.chunks_exact(4) {
                        small.push(u32::from_le_bytes(chunk.try_into().unwrap()));
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
    use tessera_authz::{write_delta_tier_at, write_postings};

    const SMALL: u32 = 32;

    fn column_with(per_value: &[Vec<u32>], dir: &Path) -> ColumnPostings {
        let base = dir.join("postings.arrow");
        write_postings(&base, per_value, SMALL).unwrap();
        ColumnPostings::open(&base, false).unwrap()
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

    /// An ordinal past the base's records is an ordinary empty answer, not an error — this is what
    /// lets a value promoted since the last build live only in a delta tier.
    #[test]
    fn an_ordinal_beyond_the_base_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let column = column_with(&[vec![1]], dir.path());

        assert!(column.entities(AttrLocalId::new(50)).unwrap().is_empty());
    }

    /// The read is base ∪ tiers, and a tier holds only what its flush touched.
    #[test]
    fn a_delta_tier_contributes_to_the_union() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_postings(&base, &[vec![1, 2], vec![5]], SMALL).unwrap();

        // The flush touched value 0 (a new entity) and value 3, which the base has no record for.
        let tier_path = dir.path().join("delta.arrow");
        write_delta_tier_at(&tier_path, &[(0, vec![40]), (3, vec![41])], SMALL).unwrap();
        let tier = Arc::new(DeltaTier::open(&tier_path).unwrap());

        let column = ColumnPostings::open(&base, false)
            .unwrap()
            .with_tiers(vec![tier]);

        assert_eq!(
            column.entities(AttrLocalId::new(0)).unwrap().to_vec(),
            vec![1, 2, 40],
            "base and tier both contribute"
        );
        assert_eq!(
            column.entities(AttrLocalId::new(3)).unwrap().to_vec(),
            vec![41],
            "a value the base predates resolves from the tier alone"
        );
        assert_eq!(
            column.entities(AttrLocalId::new(1)).unwrap().to_vec(),
            vec![5],
            "a value the tier does not carry is unaffected by it"
        );
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

    /// A keyed base composes with delta tiers the same way a positional one does: a code minted at a
    /// commit-window close after the build resolves from its tier alone.
    #[test]
    fn a_keyed_base_composes_with_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_delta_tier_at(&base, &[(7, vec![1, 2])], SMALL).unwrap();

        let tier_path = dir.path().join("delta.arrow");
        write_delta_tier_at(&tier_path, &[(7, vec![90]), (12_345, vec![91])], SMALL).unwrap();
        let tier = Arc::new(DeltaTier::open(&tier_path).unwrap());

        let column = ColumnPostings::open_keyed(&base)
            .unwrap()
            .with_tiers(vec![tier]);

        assert_eq!(
            column.entities(AttrLocalId::new(7)).unwrap().to_vec(),
            vec![1, 2, 90]
        );
        assert_eq!(
            column.entities(AttrLocalId::new(12_345)).unwrap().to_vec(),
            vec![91],
            "a code minted since the build resolves from its tier"
        );
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
