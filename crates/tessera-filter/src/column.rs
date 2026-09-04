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
    ///
    /// **The base is opened with its bucket table** ([`DeltaTier::open_indexed`],
    /// `value-suggestion.md` §6.2 **(b′)**): a suggestion walk searches this array up to
    /// `max_suggestion_walk` times per keystroke and the search was 68–72% of a probe at 10⁷
    /// records. The delta tiers attached by [`Self::with_tiers`] are not — a flush's tier is small
    /// and the table would be many times the array it indexes.
    pub fn open_keyed(base_path: &Path) -> io::Result<Self> {
        Ok(ColumnPostings {
            base: BaseTier::Keyed(DeltaTier::open_indexed(base_path)?),
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

    /// Whether any delta tier is stacked over the base.
    ///
    /// Asked by a caller whose arithmetic distributes over a *single* source and not over a union —
    /// `CategoryMembership::count`, whose `and_cardinality` is exact per record and wrong across
    /// one. Existence distributes and never needs this.
    pub fn has_tiers(&self) -> bool {
        !self.tiers.is_empty()
    }

    /// The entities carrying `value`, unioned across the base and every live tier.
    pub fn entities(&self, value: AttrLocalId) -> io::Result<Bitmap> {
        resolve_union(self, &[value])
    }

    /// `candidate ∩ (the entities carrying value)` — **without ever materialising the posting**.
    ///
    /// # Why this exists beside [`Self::entities`]
    ///
    /// A posting is a *corpus-wide* set: every entity carrying the value, including every one the
    /// asking principal may not see. `entities` returns it as an owned bitmap, so a caller that
    /// wanted only the visible part paid a heap allocation the size of the whole thing and then
    /// threw most of it away. At 10⁹ entities a common term's posting is hundreds of megabytes of
    /// **anonymous** memory — the kind the kernel cannot reclaim under pressure — held per value
    /// being read, on the request path, with nothing in the request bounding how many values it
    /// names.
    ///
    /// The postings file is mapped, and `croaring` deserialises the portable format as a *view*
    /// over those bytes with no copy. Intersecting against the view leaves the corpus-wide set
    /// where it already is — file-backed page cache the kernel may drop — and allocates only the
    /// answer, which is bounded by `candidate` rather than by the posting.
    ///
    /// **The distributive step is what makes this exact across tiers**: a value's entities are the
    /// *union* of its base record and every tier's, and `(A ∪ B) ∩ C = (A ∩ C) ∪ (B ∩ C)`. So each
    /// source is narrowed as it is read and the results unioned, which never assembles the
    /// unnarrowed union at all. The tag-0 case materialises, and may: it is a sorted `u32` array of
    /// at most `small_term_threshold` entities — 32 by default — so "materialising" it is a handful
    /// of words.
    ///
    /// The answer is identical to `entities(value).and(candidate)`, and
    /// `narrowing_is_the_same_answer_as_intersecting_afterwards` holds it to that.
    ///
    /// **Neither this nor [`Self::narrow_inplace`] run-optimises**, where [`resolve_union`] does.
    /// A run-optimise is a storage choice about a bitmap's containers, and these two exist to be
    /// chained — optimising a set the next intersection is about to shrink is work thrown away.
    /// The caller does it once, at the end, if the result is going anywhere that cares.
    pub fn narrow(&self, value: AttrLocalId, candidate: &Bitmap) -> io::Result<Bitmap> {
        // A candidate with nothing in it intersects to nothing, whatever the posting holds — and
        // taking that here means a principal who can see none of this view reads no posting bytes
        // at all rather than reading them to intersect them away.
        if candidate.is_empty() {
            return Ok(Bitmap::new());
        }
        let mut out = Bitmap::new();
        for posting in self.sources(value)? {
            match posting {
                // The view derefs to a `Bitmap` whose storage is the mapped file's bytes, so this
                // reads them and writes only the intersection.
                PostingRef::Roaring(view) => out |= candidate.and(&view),
                PostingRef::Array(bytes) => {
                    // Bounded by `small_term_threshold`, so the membership test is over a handful
                    // of entities and needs no bitmap of its own.
                    for chunk in bytes.chunks_exact(4) {
                        let entity = u32::from_le_bytes(chunk.try_into().unwrap());
                        if candidate.contains(entity) {
                            out.add(entity);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// **Does any entity in `candidate` carry `value`** — as a boolean, short-circuiting at the
    /// first container the two share, and never materialising the posting.
    ///
    /// # Why this exists beside [`Self::narrow`]
    ///
    /// The question `/v1/categories` and `/v1/categories/{column}/suggest` ask of a `derived`
    /// column is `members(v) ∩ candidate ≠ ∅` — a *bit*, asked once per value walked
    /// (`value-suggestion.md` §6.2). Answering it through [`Self::entities`] materialises the
    /// value's corpus-wide posting and intersects it afterwards; answering it through
    /// [`Self::narrow`] allocates the intersection and then throws it away. Neither can stop early,
    /// and the enumeration runs one of them per value in the whole vocabulary.
    ///
    /// `Bitmap::intersect` against the mapped view answers the bit without allocating and returns
    /// at the first coinciding container, so a hidden value costs the container keys the two sets
    /// share and nothing more. The measured gap on a visible head value under a scattered candidate
    /// at 10⁷ values over 10⁸ entities is **0.1–0.6 ms** median through the materialising route
    /// against **0.15–16.6 µs** through this one (`probes/2026-09-02-value-suggestion/`).
    ///
    /// **Existential questions distribute over the union, which is what makes this exact across
    /// tiers**: a value's entities are its base record ∪ every live tier's, and
    /// `(A ∪ B) ∩ C ≠ ∅ ⟺ (A ∩ C ≠ ∅) ∨ (B ∩ C ≠ ∅)`. So each source is tested as it is read and
    /// the first `true` returns — the union is never assembled at all. Cardinality does **not**
    /// distribute this way, which is why [`crate::ColumnPostings`] has no `count` beside this one
    /// and why `CategoryMembership::count` asserts the tier stack is empty before it counts.
    ///
    /// An empty candidate is `false` without reading a byte, on [`Self::narrow`]'s reasoning: a
    /// principal who can see nothing reads no posting.
    pub fn intersects(&self, value: AttrLocalId, candidate: &Bitmap) -> io::Result<bool> {
        if candidate.is_empty() {
            return Ok(false);
        }
        // **Written against the sources one at a time rather than through [`Self::sources`]**, which
        // collects them into a `Vec`. That allocation is nothing beside a materialised posting and
        // it is not nothing beside a boolean: the suggestion walk runs this up to
        // `max_suggestion_walk` times per keystroke, and measured at 10⁷ values the allocation was
        // a visible share of the walk (`crates/tessera-bench/src/bin/suggest_walk.rs`).
        if let Some(posting) = self.base.posting_at(value.raw())? {
            if hits(&posting, candidate) {
                return Ok(true);
            }
        }
        for tier in &self.tiers {
            if let Some(posting) = tier.posting_at(value.raw())? {
                if hits(&posting, candidate) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// **`|members(value) ∩ candidate|`** against the mapped view, without materialising the
    /// intersection — C8's `and_cardinality`, which is what `?counts=true` serves beside a
    /// suggestion (`value-suggestion.md` §3).
    ///
    /// # The one thing this is not allowed to assume, and does not
    ///
    /// **Cardinality does not distribute over a union.** `|(A ∪ B) ∩ C|` is *not*
    /// `|A ∩ C| + |B ∩ C|` where `A` and `B` share entities, and a value's entities are its base
    /// record unioned with every live tier's — a flush republishing a value the base already holds
    /// puts the same entity in both. So the several-source case unions the narrowed sets and counts
    /// the result, which is [`Self::narrow`]'s allocation and is correct; only the single-source
    /// case, which is every category column today (categories have no delta postings tiers — the
    /// flush writes extents, not postings), takes the allocation-free route.
    pub fn intersection_cardinality(
        &self,
        value: AttrLocalId,
        candidate: &Bitmap,
    ) -> io::Result<u64> {
        if candidate.is_empty() {
            return Ok(0);
        }
        let sources = self.sources(value)?;
        match sources.len() {
            0 => Ok(0),
            1 => Ok(match &sources[0] {
                PostingRef::Roaring(view) => candidate.and_cardinality(view),
                PostingRef::Array(bytes) => bytes
                    .chunks_exact(4)
                    .filter(|chunk| {
                        candidate.contains(u32::from_le_bytes((*chunk).try_into().unwrap()))
                    })
                    .count() as u64,
            }),
            _ => Ok(self.narrow(value, candidate)?.cardinality()),
        }
    }

    /// [`Self::narrow`] with the running set narrowed **in place** — `live ∩= the value's entities`.
    ///
    /// # Why a second entry point rather than `live = narrow(value, &live)`
    ///
    /// It was measured. Chaining `narrow` allocates a fresh bitmap at every step, and over a
    /// conjunction of common terms that allocation dominates: the out-of-place chain measured
    /// **19–51% slower** than the route it replaced on a full-coverage principal intersecting head
    /// terms, wiping out the gain it made everywhere else
    /// (`probes/2026-08-14-hidden-vs-absent/`). `and_inplace` mutates the running set's containers
    /// and allocates nothing, which is what makes the chain cheaper *as well as* smaller.
    ///
    /// **The in-place path needs a single source, and that is a property of the caller's family
    /// rather than an assumption.** A value's entities are the union of its base record and every
    /// live tier, and `live ∩ (A ∪ B)` cannot be done by intersecting `live` with each in turn —
    /// the first would delete the entities only the second holds. Where there are several sources
    /// this falls back to the distributive form, which allocates once and is exactly what `narrow`
    /// does. A **text** column has no tiers at all, so it always takes the in-place path; a
    /// category column with live tiers takes the other and is no worse off than before.
    pub fn narrow_inplace(&self, value: AttrLocalId, live: &mut Bitmap) -> io::Result<()> {
        if live.is_empty() {
            return Ok(());
        }
        let sources = self.sources(value)?;
        match sources.len() {
            // No record at all: nothing carries the value, so nothing survives.
            0 => live.clear(),
            1 => match &sources[0] {
                PostingRef::Roaring(view) => live.and_inplace(view),
                PostingRef::Array(bytes) => {
                    let mut small = Bitmap::new();
                    for chunk in bytes.chunks_exact(4) {
                        small.add(u32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                    live.and_inplace(&small);
                }
            },
            _ => {
                let mut out = Bitmap::new();
                for posting in &sources {
                    match posting {
                        PostingRef::Roaring(view) => out |= live.and(view),
                        PostingRef::Array(bytes) => {
                            for chunk in bytes.chunks_exact(4) {
                                let entity = u32::from_le_bytes(chunk.try_into().unwrap());
                                if live.contains(entity) {
                                    out.add(entity);
                                }
                            }
                        }
                    }
                }
                *live = out;
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

    /// Every record holding `value`, base first then the tiers in serving order.
    ///
    /// The value's entities are the **union** of these; nothing here unions them, which is the
    /// whole point — both narrowing entries intersect each source instead.
    fn sources(&self, value: AttrLocalId) -> io::Result<Vec<PostingRef<'_>>> {
        let mut out = Vec::with_capacity(1 + self.tiers.len());
        out.extend(self.base.posting_at(value.raw())?);
        for tier in &self.tiers {
            out.extend(tier.posting_at(value.raw())?);
        }
        Ok(out)
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
            .chunks_exact(4)
            .any(|chunk| candidate.contains(u32::from_le_bytes(chunk.try_into().unwrap()))),
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

    /// **`narrow` answers exactly what intersecting afterwards answers**, across every shape the
    /// two encodings and the tier stack produce.
    ///
    /// The whole point of `narrow` is that it never assembles the corpus-wide posting, so its
    /// agreement with the obvious construction is the thing that has to be checked rather than
    /// assumed — and checked at the boundary between the encodings, since the two are read by
    /// different code (`small_term_threshold` decides which, and a value either side of it takes a
    /// different arm).
    ///
    /// **Mutations this kills:** intersecting only the base and dropping the tiers; unioning the
    /// tag-0 array without testing membership; taking the union of the *narrowed* sets as an
    /// intersection.
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

    /// The same agreement with a **delta tier** stacked over the base, which is where the
    /// distributive step earns its keep: the value's entities are a union across sources, and
    /// `narrow` intersects each source rather than the union.
    #[test]
    fn narrowing_agrees_across_a_tier_stack() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        write_postings(&base, &[vec![1, 5, 9], vec![2]], SMALL).unwrap();
        let tier_path = dir.path().join("tier.arrow");
        write_delta_tier_at(&tier_path, &[(0, vec![4, 5, 20])], SMALL).unwrap();
        let column = ColumnPostings::open(&base, false)
            .unwrap()
            .with_tiers(vec![std::sync::Arc::new(
                tessera_authz::DeltaTier::open(&tier_path).unwrap(),
            )]);

        for candidate in [
            Bitmap::from_range(0..100),
            Bitmap::of(&[5]),
            Bitmap::of(&[4, 9]),
            Bitmap::of(&[7]),
            Bitmap::new(),
        ] {
            let want = column.entities(AttrLocalId::new(0)).unwrap().and(&candidate);
            assert_eq!(
                column.narrow(AttrLocalId::new(0), &candidate).unwrap(),
                want,
                "a value whose entities span the base and a tier"
            );
            // **The in-place path must take the distributive fallback here**, two sources being
            // present: narrowing against each in turn would delete the entities only the other
            // holds, which is the one way this optimisation can be wrong.
            let mut live = candidate.clone();
            column
                .narrow_inplace(AttrLocalId::new(0), &mut live)
                .unwrap();
            assert_eq!(live, want, "in place, across a tier stack");
        }
    }

    /// **`intersects` answers exactly `!narrow(..).is_empty()`**, across both encodings, an absent
    /// record, an empty candidate and a tier stack.
    ///
    /// The point of the boolean route is that it stops early and never assembles the posting, so
    /// its agreement with the materialising construction is the thing to check rather than assume —
    /// and a walk that runs it once per value in a vocabulary turns a disagreement into a value
    /// silently withheld from, or offered to, a principal.
    ///
    /// **Mutations this kills:** returning at the first source instead of at the first *hit*;
    /// testing the tag-0 array's bytes against the wrong endianness; treating an absent record as
    /// `true`; skipping the tiers.
    #[test]
    fn intersects_agrees_with_narrowing_across_a_tier_stack() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("postings.arrow");
        let wide: Vec<u32> = (0..500).map(|i| i * 3).collect();
        let boundary: Vec<u32> = (0..SMALL).collect();
        write_postings(
            &base,
            &[vec![1, 2, 3, 900], wide, vec![], boundary],
            SMALL,
        )
        .unwrap();
        let tier_path = dir.path().join("tier.arrow");
        // Value 0 gains entities the base does not hold, and value 7 exists only in the tier —
        // both shapes a boolean that consulted the base alone would answer wrongly.
        write_delta_tier_at(&tier_path, &[(0, vec![4, 5, 20]), (7, vec![77])], SMALL).unwrap();
        let column = ColumnPostings::open(&base, false)
            .unwrap()
            .with_tiers(vec![Arc::new(DeltaTier::open(&tier_path).unwrap())]);

        let candidates = [
            Bitmap::new(),
            Bitmap::of(&[2]),
            Bitmap::of(&[20]),
            Bitmap::of(&[77]),
            Bitmap::of(&[1, 3, 900, 6, 12, 4_000]),
            Bitmap::from_range(0..1_500),
            Bitmap::of(&[999_999]),
        ];
        for value in [0u32, 1, 2, 3, 7, 50] {
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
