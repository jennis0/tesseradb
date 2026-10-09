//! The four generator properties, each checked by a test.

use proptest::prelude::*;
use tessera_corpus::{Corpus, Grant, TERM_SPACE};
use tessera_spatial::{morton_of, Bounds, Tile};

fn grid() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn corpus(seed: u64, n: u64) -> Corpus {
    Corpus::new(seed, n, grid()).unwrap()
}

/// Every term id, as the passthrough descriptor encoding a grant is parsed from.
fn full_grant() -> Grant {
    let all: Vec<String> = (0..TERM_SPACE).map(|t| t.to_string()).collect();
    Grant::parse(&all.join(",")).unwrap()
}

proptest! {
    /// Prefix-stable: item `e`'s properties depend on `e` and the seed, never on `n`. Two corpora
    /// differing only in `n` agree on every item and every term list.
    #[test]
    fn n_appears_in_no_derivation(
        seed in any::<u64>(),
        e in 0u64..1u64 << 40,
        n1 in 0u64..1u64 << 32,
        n2 in 0u64..1u64 << 32,
    ) {
        let (a, b) = (corpus(seed, n1), corpus(seed, n2));
        prop_assert_eq!(a.item(e), b.item(e));
        prop_assert_eq!(a.terms(e), b.terms(e));
    }

    /// Prefix-stable, the partition value included: entity `e` is in the same artifact at every
    /// `n`, on every layer.
    #[test]
    fn the_partition_arm_is_prefix_stable_too(
        seed in any::<u64>(),
        layer in 0u64..6,
        e in 0u64..1u64 << 20,
        n1 in 0u64..1u64 << 22,
        n2 in 0u64..1u64 << 22,
    ) {
        let (a, b) = (corpus(seed, n1), corpus(seed, n2));
        prop_assert_eq!(
            a.partition_artifact_of(layer, e),
            b.partition_artifact_of(layer, e)
        );
    }

    /// Named in its own row: `fx_key` is a bijection of `e` under the seed. It inverts exactly, in
    /// both directions, for every value.
    #[test]
    fn fx_key_inverts_exactly(seed in any::<u64>(), e in any::<u64>(), fx in any::<u64>()) {
        let c = corpus(seed, 0);
        prop_assert_eq!(c.item_of_fx_key(c.item(e).fx_key), e);
        prop_assert_eq!(c.item(c.item_of_fx_key(fx)).fx_key, fx);
    }
}

/// Spread at any size: 100,000 items must occupy essentially 100,000 distinct positions.
#[test]
fn positions_spread_at_any_size() {
    let c = corpus(1, 0);
    for window in [0u64, 1_000_000_000] {
        let mut seen = std::collections::HashSet::new();
        let mut per_position_max = std::collections::HashMap::new();
        for e in window..window + 100_000 {
            let item = c.item(e);
            let key = (item.x.to_bits(), item.y.to_bits());
            seen.insert(key);
            *per_position_max.entry(key).or_insert(0u32) += 1;
        }
        assert!(
            seen.len() >= 99_900,
            "window at {window}: {} distinct positions in 100,000 items, a modulus is back",
            seen.len()
        );
        let worst = per_position_max.values().max().copied().unwrap_or(0);
        assert!(
            worst <= 3,
            "window at {window}: one position holds {worst} of 100,000 items"
        );
    }
}

/// Decorrelated across dimensions: independent axes put an item on the `cell_x == cell_y`
/// diagonal about once per 65,536 items.
#[test]
fn the_two_axes_are_independent() {
    let c = corpus(2, 0);
    let e_bounds = grid();
    let on_diagonal = (0u64..65_536)
        .filter(|&e| {
            let item = c.item(e);
            tessera_spatial::cell(item.x, e_bounds.x_min, e_bounds.x_max)
                == tessera_spatial::cell(item.y, e_bounds.y_min, e_bounds.y_max)
        })
        .count();
    assert!(
        on_diagonal < 32,
        "{on_diagonal} of 65,536 items share an x/y cell, the axes are correlated"
    );
}

/// Position does not track the grant structure: a single term's carriers land uniformly across
/// the four depth-1 quadrants.
#[test]
fn terms_do_not_track_position() {
    for seed in [3u64, 4, 5] {
        let c = corpus(seed, 65_536);
        let per_tile = c.census(1, &Grant::parse("0").unwrap());
        let total: u64 = per_tile.iter().map(|(_, count)| count).sum();
        assert!(total > 512, "seed {seed}: term 0 has only {total} carriers");
        assert_eq!(
            per_tile.len(),
            4,
            "seed {seed}: a quadrant holds no carriers at all"
        );
        for &(tile, count) in &per_tile {
            let expected = total / 4;
            let deviation = count.abs_diff(expected);
            assert!(
                deviation < 120,
                "seed {seed}: quadrant {tile} holds {count} of {total} carriers, position is \
                 correlated with the grant structure"
            );
        }
    }
}

/// `fx_key` is not an affine encoding of `e`: consecutive keys must not differ by a near-constant
/// amount.
#[test]
fn fx_keys_are_not_affine_in_e() {
    let c = corpus(6, 0);
    let keys: Vec<u64> = (0..1_000).map(|e| c.item(e).fx_key).collect();
    let diffs: std::collections::HashSet<u64> =
        keys.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
    assert!(
        diffs.len() >= 990,
        "{} distinct consecutive differences in 1,000 keys",
        diffs.len()
    );
}

/// The term-width spectrum the level construction promises: some terms are two orders of
/// magnitude wider than others.
#[test]
fn term_widths_span_a_spectrum() {
    let c = corpus(7, 0);
    let mut width = vec![0u64; TERM_SPACE as usize];
    for e in 0..65_536 {
        for t in c.terms(e) {
            width[t.raw() as usize] += 1;
        }
    }
    let max = width.iter().max().copied().unwrap();
    let min_nonzero = width.iter().filter(|w| **w > 0).min().copied().unwrap();
    assert!(
        max >= 100 * min_nonzero,
        "widest term {max}, narrowest non-empty {min_nonzero}: no spectrum"
    );
}

/// Zoom 0 is one tile holding exactly the visible count, and the full grant sees every item.
#[test]
fn census_totals_agree_with_direct_visibility() {
    let c = corpus(8, 10_000);
    let grant = Grant::parse("0,1,2,64,65,700").unwrap();
    let direct = (0..c.n()).filter(|&e| c.visible(e, &grant)).count() as u64;
    let at_zoom_0 = c.census(0, &grant);
    assert_eq!(at_zoom_0.len(), 1);
    assert_eq!(at_zoom_0[0], (0, direct));

    let full: u64 = c.census(0, &full_grant()).iter().map(|(_, n)| n).sum();
    assert_eq!(full, c.n(), "the full grant must see every item");

    assert!(
        c.census(4, &Grant::parse("").unwrap()).is_empty(),
        "the empty grant sees nothing"
    );
}

/// Each depth-`z` tile's count is the sum of its four depth-`z+1` children.
#[test]
fn census_counts_nest_across_zooms() {
    let c = corpus(9, 8_192);
    let grant = Grant::parse("0,1,2,3,4,5,6,7").unwrap();
    let coarse = c.census(3, &grant);
    let fine = c.census(4, &grant);
    let mut rolled: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
    for (tile, count) in fine {
        *rolled.entry(tile >> 2).or_insert(0) += count;
    }
    let rolled: Vec<(u64, u64)> = rolled.into_iter().collect();
    assert_eq!(coarse, rolled);
}

/// The census's tile assignment agrees with the Morton code route: every counted tile holds
/// exactly the visible items whose `morton_of` code falls in its `code_range`.
#[test]
fn census_tiles_agree_with_morton_code_ranges() {
    let c = corpus(10, 4_096);
    let grant = Grant::parse("0,1,64").unwrap();
    let zoom = 5u8;
    let e_bounds = grid();
    for (tile, count) in c.census(zoom, &grant) {
        let (lo, hi) = Tile {
            prefix: tile,
            depth: zoom,
        }
        .code_range();
        let by_code = (0..c.n())
            .filter(|&e| {
                if !c.visible(e, &grant) {
                    return false;
                }
                let item = c.item(e);
                let code = u64::from(morton_of(item.x, item.y, &e_bounds).raw());
                code >= lo && code < hi
            })
            .count() as u64;
        assert_eq!(by_code, count, "tile {tile} at zoom {zoom}");
    }
}

/// The partition truncates, exhaustively over a prefix: every entity of the smaller corpus, its
/// artifact and its artifact's membership, checked against the larger one.
#[test]
fn a_smaller_corpus_is_the_partition_truncated() {
    let small = corpus(0x5EED, 4_000);
    let large = corpus(0x5EED, 400_000);
    let layer = 3;

    for e in 0..small.n() {
        assert_eq!(
            small.partition_artifact_of(layer, e),
            large.partition_artifact_of(layer, e),
            "entity {e} was re-assigned by the corpus growing"
        );
    }

    let count = small.partition_count(layer);
    assert!(
        count > 1,
        "the fixture is too small to have a tail to truncate"
    );
    for a in 0..count {
        let want: Vec<u64> = large
            .partition_members(layer, a)
            .into_iter()
            .filter(|e| *e < small.n())
            .collect();
        assert_eq!(
            small.partition_members(layer, a),
            want,
            "artifact {a}'s membership is not the larger corpus's clipped to n"
        );
    }
    assert!(
        small.partition_members(layer, count).is_empty(),
        "an artifact past the truncation point still holds members"
    );
}

/// Total over `u64`: a served `fx_key` inverts to an arbitrary entity id, so the partition lookup
/// is asked about entities far past any corpus, the top of the range included.
#[test]
fn the_partition_lookup_answers_at_the_top_of_entity_space() {
    let c = corpus(0x5EED, 0);
    for e in [0, 1, u64::MAX / 2, u64::MAX - 100, u64::MAX - 1, u64::MAX] {
        let a = c.partition_artifact_of(1, e);
        assert!(
            e / 100 <= a + 1 && a <= e / 100 + 1,
            "entity {e} landed in artifact {a}, outside the stride's own bound"
        );
    }
}
