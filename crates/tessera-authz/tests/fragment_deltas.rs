//! A fragment build unions the delta postings tiers (§5.2).
//!
//! Flush produces one delta postings tier per segment, and a fragment build unions across every
//! live tier. A tier is **sparse** — only the terms present in its flushed set — so a term a tier
//! does not carry contributes nothing, which is also what makes a promoted term readable: its
//! ordinal is at or above the base's term count, and the base simply has nothing for it.

use std::collections::BTreeSet;
use std::sync::Arc;

use tempfile::TempDir;
use tessera_authz::{build_fragment, write_postings, PostingsReader};
use tessera_types::TermId;

const SMALL_TERM_THRESHOLD: u32 = 32;

fn postings_with(dir: &std::path::Path, name: &str, per_term: &[&[u32]]) -> Arc<PostingsReader> {
    let path = dir.join(name);
    let owned: Vec<Vec<u32>> = per_term.iter().map(|t| t.to_vec()).collect();
    write_postings(&path, &owned, SMALL_TERM_THRESHOLD).unwrap();
    Arc::new(PostingsReader::open(&path, false).unwrap())
}

/// The inert case, asserted on bytes rather than on cardinality: no tier changes nothing.
/// This is what makes the union landable ahead of any flush.
#[test]
fn a_build_over_zero_delta_tiers_is_byte_identical() {
    let temp = TempDir::new().unwrap();
    let base = postings_with(temp.path(), "base.arrow", &[&[1, 2, 3], &[2, 4]]);
    let terms = [TermId::new(0), TermId::new(1)];

    let before = build_fragment(&terms, &base).unwrap();
    let after = build_fragment_over_tiers(&terms, &base, &[]).unwrap();

    assert_eq!(
        before.serialize::<croaring::Portable>(),
        after.serialize::<croaring::Portable>()
    );
}

/// A tier contributes only for terms the session already holds — the union is over `satisfied`,
/// never over the tier's whole term set (I2).
#[test]
fn a_delta_tier_contributes_only_satisfied_terms() {
    let temp = TempDir::new().unwrap();
    let base = postings_with(temp.path(), "base.arrow", &[&[1], &[2]]);
    let delta = postings_with(temp.path(), "delta.arrow", &[&[10], &[11]]);

    let fragment = build_fragment_over_tiers(&[TermId::new(0)], &base, &[delta]).unwrap();

    assert!(fragment.contains(1) && fragment.contains(10));
    assert!(!fragment.contains(2) && !fragment.contains(11));
}

/// A tier is sparse: a term it does not carry is a hit of zero cost, not an error. Without this
/// a promoted descriptor — whose ordinal is at or above the base's term count (§3.2) — would make
/// every authorise carrying it fail outright.
#[test]
fn a_term_no_tier_carries_contributes_nothing_rather_than_failing() {
    let temp = TempDir::new().unwrap();
    let base = postings_with(temp.path(), "base.arrow", &[&[1, 2]]);
    let delta = postings_with(temp.path(), "delta.arrow", &[&[], &[7]]);

    // Term 1 is absent from the base and present in the tier; term 9 is in neither.
    let fragment = build_fragment_over_tiers(
        &[TermId::new(0), TermId::new(1), TermId::new(9)],
        &base,
        &[delta],
    )
    .unwrap();

    assert_eq!(fragment.to_vec(), vec![1, 2, 7]);
}

/// Tiers union rather than shadow: an entity a later tier adds for a term joins the ones the base
/// and the earlier tiers already carry. Nothing a merge does to the tiers may change this set
/// (§5.2's content-preserving re-encode).
#[test]
fn every_tier_contributes_and_none_shadows_another() {
    let temp = TempDir::new().unwrap();
    let base = postings_with(temp.path(), "base.arrow", &[&[1]]);
    let t1 = postings_with(temp.path(), "t1.arrow", &[&[5]]);
    let t2 = postings_with(temp.path(), "t2.arrow", &[&[9]]);

    let fragment = build_fragment_over_tiers(&[TermId::new(0)], &base, &[t1, t2]).unwrap();

    let got: BTreeSet<u32> = fragment.iter().collect();
    assert_eq!(got, BTreeSet::from([1, 5, 9]));
}

fn build_fragment_over_tiers(
    terms: &[TermId],
    base: &PostingsReader,
    deltas: &[Arc<PostingsReader>],
) -> std::io::Result<croaring::Bitmap> {
    tessera_authz::build_fragment_with_deltas(terms, base, deltas)
}
