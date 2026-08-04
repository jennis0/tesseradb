//! What a merge is allowed to take (write-path §7).

use tessera_store::manifest::SegmentDescriptor;
use tessera_store::merge::MergePolicy;

/// Segments in listed order, with the entity ranges given. Sizes are supplied separately, because
/// a descriptor carries a row count and the policy reasons in bytes.
fn segments_at(ranges: &[(u64, u64)]) -> Vec<SegmentDescriptor> {
    ranges
        .iter()
        .enumerate()
        .map(|(i, (lo, hi))| SegmentDescriptor {
            slice: "s0".to_string(),
            seg_id: format!("s{i}"),
            row_count: (hi - lo + 1) as u32,
            entity_lo: *lo,
            entity_hi: *hi,
        })
        .collect()
}

fn unbounded(tier_width: usize, segment_floor_bytes: u64) -> MergePolicy {
    MergePolicy {
        tier_width,
        segment_floor_bytes,
        max_merged_segment_bytes: u64::MAX,
    }
}

/// **Adjacency is the first rule.** A size-only policy produces merged segments covering
/// discontiguous entity sets, and the extent list then fragments monotonically with nothing but
/// compaction to repair it.
#[test]
fn selection_takes_entity_adjacent_runs_only() {
    let selected = unbounded(3, 0).select(&segments_at(&[(0, 9), (10, 19), (20, 29)]), &[100; 3]);
    assert_eq!(
        selected,
        Some(vec!["s0".into(), "s1".into(), "s2".into()]),
        "three adjacent, same-size segments are exactly what a tier is"
    );
}

/// A gap is not a discontinuity: a deleted entity acquires no row, so a flush's range legitimately
/// has holes. Requiring `hi + 1 == lo` would stop merging entirely on a deployment that deletes.
#[test]
fn a_gap_between_ranges_does_not_block_a_merge() {
    let selected = unbounded(2, 0).select(&segments_at(&[(0, 9), (20, 29)]), &[100, 100]);
    assert_eq!(selected, Some(vec!["s0".into(), "s1".into()]));
}

/// Overlapping or out-of-order ranges are refused: the merged extent must be one contiguous span
/// of entity space, and a window that interleaves with its neighbours is not.
#[test]
fn overlapping_ranges_are_never_selected() {
    let overlapping = segments_at(&[(0, 20), (10, 29)]);
    assert_eq!(unbounded(2, 0).select(&overlapping, &[100, 100]), None);
}

/// **The floor stops a tail of tiny segments dominating selection.** Without it, sizes that differ
/// by a few bytes fall into different tiers, no tier ever reaches `tier_width`, and the policy
/// silently merges nothing — a failure that looks exactly like a policy never triggered.
#[test]
fn segments_below_the_floor_compare_equal() {
    assert!(unbounded(2, 1000)
        .select(&segments_at(&[(0, 1), (2, 3)]), &[1, 999])
        .is_some());
}

/// And above the floor they do not: two segments three octaves apart are not one tier, which is
/// what stops a merge repeatedly rewriting a large segment to absorb a small one.
#[test]
fn segments_in_different_tiers_are_not_selected_together() {
    assert_eq!(
        unbounded(2, 100).select(&segments_at(&[(0, 1), (2, 3)]), &[200, 4000]),
        None
    );
}

/// **The cost of adjacency, stated rather than hidden:** a segment over the cap blocks a merge of
/// its neighbours, because merging across it is what fragments the extent list.
#[test]
fn a_segment_over_the_cap_blocks_its_neighbours() {
    let policy = MergePolicy {
        tier_width: 2,
        segment_floor_bytes: 0,
        max_merged_segment_bytes: 500,
    };
    assert_eq!(
        policy.select(&segments_at(&[(0, 1), (2, 3), (4, 5)]), &[400, 400, 400]),
        None,
        "any adjacent pair totals 800, which is over the bound"
    );
}

/// **This is what keeps the base segment out**, and it is a size bound rather than a rule: a merge
/// that swallowed the base would pay compaction's whole cost and bank none of its benefit.
#[test]
fn a_base_sized_segment_excludes_itself_by_size() {
    let policy = MergePolicy {
        tier_width: 2,
        segment_floor_bytes: 0,
        max_merged_segment_bytes: 10_000,
    };
    let segments = segments_at(&[(0, 999_999), (1_000_000, 1_000_009), (1_000_010, 1_000_019)]);
    assert_eq!(
        policy.select(&segments, &[1_000_000, 100, 100]),
        Some(vec!["s1".into(), "s2".into()]),
        "the two flush segments merge; the base is neither in their tier nor within the bound"
    );
}

/// Fewer segments than the tier width is not a merge, and a width below 2 is not a policy.
#[test]
fn a_tier_that_is_not_full_selects_nothing() {
    assert_eq!(
        unbounded(3, 0).select(&segments_at(&[(0, 9), (10, 19)]), &[100; 2]),
        None
    );
    assert_eq!(
        unbounded(1, 0).select(&segments_at(&[(0, 9), (10, 19)]), &[100; 2]),
        None
    );
    assert_eq!(unbounded(2, 0).select(&[], &[]), None);
}

/// The **first** qualifying window in list order, not the best one. Merging is idempotent work on a
/// cadence — whatever this leaves, the next tick reconsiders — so a search for the best window buys
/// a marginally better choice at the cost of a policy nobody can predict from the manifest.
#[test]
fn the_first_qualifying_window_is_taken() {
    let segments = segments_at(&[(0, 9), (10, 19), (20, 29), (30, 39)]);
    assert_eq!(
        unbounded(2, 0).select(&segments, &[100, 100, 100, 100]),
        Some(vec!["s0".into(), "s1".into()])
    );
}
