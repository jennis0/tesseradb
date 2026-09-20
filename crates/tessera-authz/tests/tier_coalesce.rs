//! Coalescing delta tiers is a content-preserving re-encode.

use std::path::PathBuf;

use tessera_authz::{coalesce_delta_tiers, write_delta_tier, DeltaTier};
use tessera_types::TermId;

const SMALL_TERM_THRESHOLD: u32 = 32;

fn tier_at(dir: &std::path::Path, name: &str, entries: &[(u32, &[u32])]) -> PathBuf {
    let path = dir.join(name);
    let owned: Vec<(TermId, Vec<u32>)> = entries
        .iter()
        .map(|(t, e)| (TermId::new(*t), e.to_vec()))
        .collect();
    write_delta_tier(&path, &owned, SMALL_TERM_THRESHOLD).unwrap();
    path
}

fn posting(path: &std::path::Path, term: u32) -> Option<Vec<u32>> {
    let tier = DeltaTier::open(path).unwrap();
    tier.posting(TermId::new(term)).unwrap().map(|p| match p {
        tessera_authz::PostingRef::Roaring(view) => view.iter().collect(),
        tessera_authz::PostingRef::Array(bytes) => bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    })
}

/// The same pairs, concatenated, deduplicated, re-sorted. The dedup is required: `encode_posting`
/// hard-fails on a non-strictly-ascending entity list.
#[test]
fn coalescing_deduplicates_and_drops_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = tier_at(dir.path(), "a.arrow", &[(3, &[1, 2])]);
    let b = tier_at(dir.path(), "b.arrow", &[(3, &[2, 5])]);
    let out = dir.path().join("merged.arrow");

    coalesce_delta_tiers(&[a, b], &out, SMALL_TERM_THRESHOLD).unwrap();
    assert_eq!(posting(&out, 3), Some(vec![1, 2, 5]));
}

/// Terms present in only one input survive, and the result is ordered by term.
#[test]
fn every_term_from_every_input_survives_in_term_order() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = tier_at(dir.path(), "a.arrow", &[(2, &[20]), (7, &[10])]);
    let b = tier_at(dir.path(), "b.arrow", &[(4, &[30])]);
    let out = dir.path().join("merged.arrow");

    coalesce_delta_tiers(&[a, b], &out, SMALL_TERM_THRESHOLD).unwrap();
    assert_eq!(posting(&out, 2), Some(vec![20]));
    assert_eq!(posting(&out, 4), Some(vec![30]));
    assert_eq!(posting(&out, 7), Some(vec![10]));
}

/// A merge retires nothing. Coalescing consults no overlay, so an entity that has been deleted or
/// suppressed keeps its posting.
#[test]
fn coalescing_retires_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = tier_at(dir.path(), "a.arrow", &[(1, &[100, 101, 102])]);
    let b = tier_at(dir.path(), "b.arrow", &[(1, &[103])]);
    let out = dir.path().join("merged.arrow");

    coalesce_delta_tiers(&[a, b], &out, SMALL_TERM_THRESHOLD).unwrap();
    assert_eq!(
        posting(&out, 1),
        Some(vec![100, 101, 102, 103]),
        "every entity the inputs carried is still here, whatever its disposition"
    );
}

/// A term wide enough to cross the threshold coalesces through the Roaring arm.
#[test]
fn a_wide_term_coalesces_through_the_roaring_arm() {
    let dir = tempfile::TempDir::new().unwrap();
    let first: Vec<u32> = (0..40).collect();
    let second: Vec<u32> = (30..70).collect();
    let a = tier_at(dir.path(), "a.arrow", &[(5, &first)]);
    let b = tier_at(dir.path(), "b.arrow", &[(5, &second)]);
    let out = dir.path().join("merged.arrow");

    coalesce_delta_tiers(&[a, b], &out, SMALL_TERM_THRESHOLD).unwrap();
    assert_eq!(posting(&out, 5), Some((0..70).collect::<Vec<u32>>()));
}

/// One input is a no-op re-encode.
#[test]
fn coalescing_one_tier_preserves_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = tier_at(dir.path(), "a.arrow", &[(1, &[1, 2, 3]), (9, &[4])]);
    let out = dir.path().join("merged.arrow");

    coalesce_delta_tiers(&[a], &out, SMALL_TERM_THRESHOLD).unwrap();
    assert_eq!(posting(&out, 1), Some(vec![1, 2, 3]));
    assert_eq!(posting(&out, 9), Some(vec![4]));
}
