//! The sparse delta postings tier a flush publishes (§3.1, §5.2).

use tessera_authz::{write_delta_tier, DeltaTier, PostingRef};
use tessera_types::TermId;

fn tier(dir: &std::path::Path, name: &str, entries: &[(u32, &[u32])]) -> DeltaTier {
    let path = dir.join(name);
    let owned: Vec<(TermId, Vec<u32>)> = entries
        .iter()
        .map(|(t, e)| (TermId::new(*t), e.to_vec()))
        .collect();
    write_delta_tier(&path, &owned, 32).unwrap();
    DeltaTier::open(&path).unwrap()
}

/// **Sparse: the file holds one record per term the flushed set carried, and nothing for the
/// gaps.** The ordinal-indexed `postings.arrow` layout cannot express this — record *i* is term
/// *i*, so a flush touching term 1_000_000 would write a million empty records, at nine bytes
/// each, per tier per 90 seconds. This is the property that rules that out.
#[test]
fn a_tier_holds_one_record_per_carried_term_however_high_the_ordinals() {
    let dir = tempfile::TempDir::new().unwrap();
    let t = tier(
        dir.path(),
        "sparse.arrow",
        &[(7, &[1, 2]), (1_000_000, &[3])],
    );

    assert_eq!(t.term_count(), 2, "two records, not a million");
    assert!(
        std::fs::metadata(dir.path().join("sparse.arrow"))
            .unwrap()
            .len()
            < 4096,
        "a tier naming a high ordinal must not be sized by that ordinal"
    );

    assert!(t.posting(TermId::new(7)).unwrap().is_some());
    assert!(t.posting(TermId::new(1_000_000)).unwrap().is_some());
    assert!(
        t.posting(TermId::new(8)).unwrap().is_none(),
        "a term between two carried ones is absent, not empty"
    );
}

/// Round trip through both encodings: the tag rule is `postings.arrow`'s, unchanged, so a term
/// above the small threshold is a Roaring bitmap and one below is a raw `u32` array.
#[test]
fn both_posting_encodings_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let large: Vec<u32> = (0..100).collect();
    let t = tier(
        dir.path(),
        "both.arrow",
        &[(3, &[5, 9]), (4, large.as_slice())],
    );

    let small = t.posting(TermId::new(3)).unwrap().unwrap();
    match small {
        PostingRef::Array(bytes) => assert_eq!(bytes.len(), 2 * 4),
        PostingRef::Roaring(_) => panic!("two entities is below the threshold"),
    }
    let large = t.posting(TermId::new(4)).unwrap().unwrap();
    match large {
        PostingRef::Roaring(view) => assert_eq!(view.cardinality(), 100),
        PostingRef::Array(_) => panic!("a hundred entities is above the threshold"),
    }
}

/// Term ids must arrive ascending and distinct: the lookup is a binary search over them, and a
/// duplicate would make one of the two records unreachable — a silently missing posting, which
/// on the authorisation path means items a viewer is entitled to simply not appearing.
#[test]
fn a_tier_refuses_unordered_or_duplicated_term_ids() {
    let dir = tempfile::TempDir::new().unwrap();
    let descending = vec![(TermId::new(9), vec![1u32]), (TermId::new(3), vec![2u32])];
    assert!(write_delta_tier(&dir.path().join("a.arrow"), &descending, 32).is_err());

    let duplicated = vec![(TermId::new(3), vec![1u32]), (TermId::new(3), vec![2u32])];
    assert!(write_delta_tier(&dir.path().join("b.arrow"), &duplicated, 32).is_err());
}

/// An empty tier is representable and opens: a flush whose items carried no term at all still
/// publishes a segment, and the manifest names a tier for it.
#[test]
fn an_empty_tier_opens_and_carries_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let t = tier(dir.path(), "empty.arrow", &[]);
    assert_eq!(t.term_count(), 0);
    assert!(t.posting(TermId::new(0)).unwrap().is_none());
}
