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
            view: "s0".to_string(),
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

/// Drive `policy` to a fixpoint over `count` adjacent segments of `size` bytes, returning the
/// sizes left. Each round applies the window `select` chose, exactly as the executor's merge
/// dispatch would over successive ticks.
fn merge_to_fixpoint(policy: MergePolicy, count: usize, size: u64) -> Vec<u64> {
    let mut sizes: Vec<u64> = vec![size; count];
    let mut ranges: Vec<(u64, u64)> = (0..count as u64).map(|i| (i * 10, i * 10 + 9)).collect();
    loop {
        let segments = segments_at(&ranges);
        let Some(chosen) = policy.select(&segments, &sizes) else {
            return sizes;
        };
        let start = segments
            .iter()
            .position(|s| s.seg_id == chosen[0])
            .expect("select returns ids from the list it was given");
        let end = start + chosen.len();
        let merged: u64 = sizes[start..end].iter().sum();
        let (lo, hi) = (ranges[start].0, ranges[end - 1].1);
        sizes.splice(start..end, [merged]);
        ranges.splice(start..end, [(lo, hi)]);
    }
}

/// **The size ladder terminates, and where it terminates decides a viewport's cost.**
///
/// Rule 3 caps the *total of the inputs*, and a merge is row-count preserving, so the ladder
/// climbs in ×`tier_width` steps from `segment_floor_bytes` and stops at the last step that does
/// not exceed `max_merged_segment_bytes`. Once segments reach that size, `tier_width` of them
/// overshoot the cap and no further merge qualifies — for ever. Live segment count therefore
/// settles at **corpus bytes ÷ the saturation size** and grows linearly with the corpus from there.
///
/// That is not a defect in this function; it is the price of §11.3's "maximum merged size, so no
/// merge becomes an unbounded rewrite", and it is bounded rather than unbounded. It is pinned here
/// because a viewport pays a *measured* 1.4–1.6 µs per (tile × segment)
/// (`docs/evidence/memos/2026-08-05-write-path-at-scale.md` §3), so the constant this test fixes is
/// a read-path constant, and the 10⁷ soak that settles at 6 segments is too small to show it.
#[test]
fn the_size_ladder_saturates_at_the_cap_and_segment_count_then_tracks_the_corpus() {
    const MIB: u64 = 1 << 20;
    let shipped = MergePolicy {
        tier_width: 4,
        segment_floor_bytes: 16 * MIB,
        max_merged_segment_bytes: 256 * MIB,
    };

    // 1 GiB of corpus as 64 flush segments at the floor.
    let left = merge_to_fixpoint(shipped, 64, 16 * MIB);
    assert_eq!(
        left,
        vec![256 * MIB; 4],
        "64 → 16 → 4 and then nothing: four segments at the cap, which cannot merge with each other"
    );

    // Raising the cap is the lever, and it is close to linear in the count.
    let roomier = MergePolicy {
        max_merged_segment_bytes: 1024 * MIB,
        ..shipped
    };
    assert_eq!(
        merge_to_fixpoint(roomier, 64, 16 * MIB),
        vec![1024 * MIB],
        "a 4× cap takes the same corpus to one segment"
    );

    // **`tier_width` moves the fixpoint too, and in the direction nobody expects.** The ladder
    // climbs in ×`tier_width` steps from the floor, so saturation is the largest such step that
    // does not exceed the cap — *not* the cap itself. At width 4 the ladder lands on 256 MiB
    // exactly; at width 8 it reaches 128 MiB and the next rung (1 GiB) overshoots, so a **wider**
    // tier leaves **twice as many** segments over the same corpus. Anyone tuning `tier_width` for
    // fewer merges is also tuning a read-path constant, in the opposite direction to the one they
    // intend.
    assert_eq!(
        merge_to_fixpoint(
            MergePolicy {
                tier_width: 8,
                ..shipped
            },
            64,
            16 * MIB
        ),
        vec![128 * MIB; 8],
        "width 8 overshoots the cap one rung earlier and settles at half the segment size"
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
