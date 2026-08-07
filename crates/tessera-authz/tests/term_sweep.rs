//! Compaction's pass 2 (compaction §3) — the term sweep.
//!
//! **Not covered here, and stated rather than implied**: peak RSS at scale. This module's own
//! working set per term is one accumulator `Bitmap` plus two scratch `Vec`s that are discarded
//! every iteration, which is an architectural property (no corpus-sized or dictionary-sized buffer
//! is allocated by `sweep_term_postings` itself), not something the tests below measure. The
//! terms that *do* scale — `PostingsSpool`'s offsets buffer and the widest term's own encode — are
//! `crate::postings`'s to measure, and `probes/results.md` §4.2 is where that figure lives.

use std::sync::Arc;

use croaring::Bitmap;
use tessera_authz::{
    sweep_term_postings, write_delta_tier, write_postings, DeltaTier, PostingRef, PostingsReader,
    PostingsSpool,
};
use tessera_types::TermId;

const THRESHOLD: u32 = 4;

fn base_postings(dir: &std::path::Path, per_term: &[Vec<u32>], threshold: u32) -> PostingsReader {
    let path = dir.join("base-postings.arrow");
    write_postings(&path, per_term, threshold).unwrap();
    PostingsReader::open(&path, false).unwrap()
}

fn tier(
    dir: &std::path::Path,
    name: &str,
    entries: &[(u32, &[u32])],
    threshold: u32,
) -> Arc<DeltaTier> {
    let path = dir.join(name);
    let owned: Vec<(TermId, Vec<u32>)> = entries
        .iter()
        .map(|(t, e)| (TermId::new(*t), e.to_vec()))
        .collect();
    write_delta_tier(&path, &owned, threshold).unwrap();
    Arc::new(DeltaTier::open(&path).unwrap())
}

fn entities_of(posting: PostingRef<'_>) -> Vec<u32> {
    match posting {
        PostingRef::Roaring(view) => view.iter().collect(),
        PostingRef::Array(bytes) => bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    }
}

/// Sweep `base`/`tiers` over `dict_len` and `tombstones`, collecting the `on_term` callback's
/// pairs alongside the finished `postings.arrow`'s own answers, so a test can compare both against
/// one expectation. Returns `(term -> entities from the finished file, term -> entities the
/// callback saw)`.
fn run_sweep(
    dir: &std::path::Path,
    dict_len: u32,
    base: &PostingsReader,
    tiers: &[Arc<DeltaTier>],
    tombstones: &Bitmap,
) -> (PostingsReader, Vec<(u32, Vec<u32>)>) {
    let spool_path = dir.join("sweep.spool");
    let out_path = dir.join("swept-postings.arrow");
    let mut spool = PostingsSpool::create(&spool_path).unwrap();

    let mut callback_pairs: Vec<(u32, Vec<u32>)> = Vec::new();
    sweep_term_postings(
        dict_len,
        base,
        tiers,
        tombstones,
        THRESHOLD,
        &mut spool,
        |term, bitmap| {
            callback_pairs.push((term.raw(), bitmap.iter().collect()));
            Ok(())
        },
    )
    .unwrap();
    spool.finish(&out_path).unwrap();

    (
        PostingsReader::open(&out_path, false).unwrap(),
        callback_pairs,
    )
}

/// **A folded entity disappears from every term it was posted under — base-only, tier-only, and a
/// term carried by both — in one sweep.** The three categories are exercised because they are read
/// through different code paths inside the sweep: a base-only term unions nothing but
/// `base.posting`, a tier-only term (ordinal past the base file's own length, so `base.posting`
/// answers `None`) unions nothing but a tier's, and the "both" term unions the two together before
/// subtracting. An extra tombstoned entity (999) that appears in no posting at all is included to
/// pin that subtracting an operand with unmatched members is harmless, not an error.
///
/// The callback's pairs are asserted against the same expectation as the finished file, which is
/// the sweep-level half of "pairs.parquet agrees with the new base postings about every folded
/// deletion" (compaction §3): both outputs come from the one bitmap this test does not know is
/// shared except by their agreeing here.
///
/// **Mutation:** delete `union -= tombstones` and every one of 2, 12, 21 survives somewhere.
/// Apply the subtraction only inside the base-only arm and 12 (tier-only contribution to the
/// "both" term) and 21 (the tier-only term) survive while 2 does not.
#[test]
fn a_folded_entity_vanishes_from_base_only_tier_only_and_both_terms() {
    let dir = tempfile::TempDir::new().unwrap();

    // Base carries terms 0 and 1 only (2 records) — term 2 is entirely beyond the base file's own
    // length, so `base.posting(2)` answers `None`, not an empty set: the "tier-only" case as the
    // module doc describes it, not merely "tier contributes more".
    let base = base_postings(dir.path(), &[vec![1, 2, 3], vec![10, 11]], THRESHOLD);
    let t = tier(
        dir.path(),
        "t.arrow",
        &[(1, &[12]), (2, &[20, 21])],
        THRESHOLD,
    );

    let tombstones = Bitmap::of(&[2, 12, 21, 999]);
    let (reader, callback_pairs) = run_sweep(dir.path(), 3, &base, &[t], &tombstones);

    assert_eq!(reader.term_count(), 3);
    let got = |t: u32| entities_of(reader.posting(TermId::new(t)).unwrap().unwrap());
    assert_eq!(
        got(0),
        vec![1, 3],
        "base-only term: 2 is gone, 1 and 3 remain"
    );
    assert_eq!(
        got(1),
        vec![10, 11],
        "term carried by both base and tier: 12 (the tier's contribution) is gone"
    );
    assert_eq!(
        got(2),
        vec![20],
        "tier-only term (absent from the base file entirely): 21 is gone"
    );

    for (term, entities) in [(0u32, vec![1, 3]), (1, vec![10, 11]), (2, vec![20])] {
        assert_eq!(
            callback_pairs
                .iter()
                .find(|(t, _)| *t == term)
                .map(|(_, e)| e.clone()),
            Some(entities),
            "on_term's pairs must agree with the finished file for term {term}"
        );
    }
}

/// **`dict_len` is honoured verbatim, never inferred from the base file's own length or from which
/// terms had data.** Two ordinals here (2, 3) have no record in the base file and no tier
/// contribution at all — nothing supplies them — and one (0) is emptied down to nothing by the
/// fold. All four still get a record at their original ordinal, so a term after an emptied one
/// does not shift down to fill the gap.
///
/// **Mutation:** loop `0..base.term_count()` instead of `0..dict_len` and this produces 2 records,
/// not 4 — every ordinal at or past the base file's length silently vanishes rather than reading
/// as empty. Skip appending a record for an empty union and term 1 lands at ordinal 0.
#[test]
fn dict_len_is_honoured_and_an_emptied_term_keeps_its_ordinal() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = base_postings(dir.path(), &[vec![5], vec![6, 7]], THRESHOLD);
    let tombstones = Bitmap::of(&[5]);

    let (reader, callback_pairs) = run_sweep(dir.path(), 4, &base, &[], &tombstones);

    assert_eq!(
        reader.term_count(),
        4,
        "dict_len must be written through verbatim"
    );
    let got = |t: u32| entities_of(reader.posting(TermId::new(t)).unwrap().unwrap());
    assert_eq!(
        got(0),
        Vec::<u32>::new(),
        "term 0 is emptied by the fold, not dropped"
    );
    assert_eq!(
        got(1),
        vec![6, 7],
        "term 1's ordinal must not shift down to fill term 0's gap"
    );
    assert_eq!(
        got(2),
        Vec::<u32>::new(),
        "term 2 has no data anywhere but still gets a record"
    );
    assert_eq!(got(3), Vec::<u32>::new(), "term 3 likewise");

    assert_eq!(
        callback_pairs.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "on_term must fire once per ordinal, in ascending order, including the empty ones"
    );
}

/// **The swept file is byte-for-byte what `write_postings` would produce from the same final
/// entity sets — including where the tag-0/tag-1 boundary falls.** Term 0's final cardinality
/// (after union and subtraction) lands exactly at `THRESHOLD`; term 1's lands one past it. If the
/// sweep's union, subtraction or encode step disagreed with `write_postings`'s own tag rule even
/// by one entity at the boundary, this catches it as a tag mismatch, not just a wrong count.
///
/// **Mutation:** swap `encode_posting_bitmap`'s `<=` for `<` against the threshold (term 0 would
/// come out tag 1), or drop `run_optimize` from its Roaring arm (term 1's bytes would differ).
#[test]
fn output_is_byte_identical_to_write_postings_over_the_same_final_sets() {
    let dir = tempfile::TempDir::new().unwrap();

    // Term 0: base {1,2,3,4,5}, no tier; tombstone 5 -> final {1,2,3,4}, cardinality == THRESHOLD.
    // Term 1: base {10,11,12}, tier adds {13,14}; no tombstone -> final {10..14}, cardinality ==
    // THRESHOLD + 1.
    let base = base_postings(
        dir.path(),
        &[vec![1, 2, 3, 4, 5], vec![10, 11, 12]],
        THRESHOLD,
    );
    let t = tier(dir.path(), "t.arrow", &[(1, &[13, 14])], THRESHOLD);
    let tombstones = Bitmap::of(&[5]);

    let (reader, _) = run_sweep(dir.path(), 2, &base, &[t], &tombstones);

    let expected_path = dir.path().join("expected.arrow");
    write_postings(
        &expected_path,
        &[vec![1, 2, 3, 4], vec![10, 11, 12, 13, 14]],
        THRESHOLD,
    )
    .unwrap();

    let actual_bytes = std::fs::read(dir.path().join("swept-postings.arrow")).unwrap();
    let expected_bytes = std::fs::read(&expected_path).unwrap();
    assert_eq!(actual_bytes, expected_bytes);

    // Cross-check via the reader too, so a failure names which term disagreed rather than just
    // "bytes differ".
    assert_eq!(
        entities_of(reader.posting(TermId::new(0)).unwrap().unwrap()),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        entities_of(reader.posting(TermId::new(1)).unwrap().unwrap()),
        vec![10, 11, 12, 13, 14]
    );
}

/// A dictionary with no terms at all sweeps to zero records and touches neither `base` nor
/// `tombstones` — the degenerate case `0..dict_len` must not special-case to avoid.
///
/// **Mutation:** an off-by-one on the loop bound (`0..=dict_len`) would panic on `TermId::new`
/// resolution against an empty base file instead of simply doing nothing.
#[test]
fn an_empty_dictionary_sweeps_to_zero_records() {
    let dir = tempfile::TempDir::new().unwrap();
    let base = base_postings(dir.path(), &[], THRESHOLD);
    let tombstones = Bitmap::new();

    let (reader, callback_pairs) = run_sweep(dir.path(), 0, &base, &[], &tombstones);
    assert_eq!(reader.term_count(), 0);
    assert!(callback_pairs.is_empty());
}
