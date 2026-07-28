//! Property tests for Morton interleaving and tile enumeration (contracts §2.5).

use proptest::prelude::*;
use tessera_spatial::{interleave, tiles_for_bbox, Extent, Tile};

/// Inverse of `interleave`: split a 32-bit Morton code back into its (x, y) 16-bit cells.
/// Bit `2*i` of the code is bit `i` of x; bit `2*i+1` of the code is bit `i` of y.
fn deinterleave(code: u32) -> (u16, u16) {
    fn compact(mut x: u32) -> u16 {
        x &= 0x5555_5555;
        x = (x | (x >> 1)) & 0x3333_3333;
        x = (x | (x >> 2)) & 0x0F0F_0F0F;
        x = (x | (x >> 4)) & 0x00FF_00FF;
        x = (x | (x >> 8)) & 0x0000_FFFF;
        x as u16
    }
    (compact(code), compact(code >> 1))
}

fn full_extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

proptest! {
    /// (a) `interleave` is injective on random pairs.
    #[test]
    fn interleave_is_injective(
        x1 in any::<u16>(), y1 in any::<u16>(),
        x2 in any::<u16>(), y2 in any::<u16>(),
    ) {
        if (x1, y1) != (x2, y2) {
            prop_assert_ne!(interleave(x1, y1).raw(), interleave(x2, y2).raw());
        }
    }

    /// (b) Deinterleaving (the inverse written above) round-trips through `interleave`.
    #[test]
    fn interleave_round_trips(x in any::<u16>(), y in any::<u16>()) {
        let code = interleave(x, y).raw();
        let (dx, dy) = deinterleave(code);
        prop_assert_eq!((dx, dy), (x, y));
    }

    /// (c) For random points and any depth, the point's code falls inside exactly one
    /// depth-d tile of `tiles_for_bbox` over the full extent.
    ///
    /// Depth is capped at 8 (4^8 = 65,536 tiles) rather than the full 0..=16: `tiles_for_bbox`
    /// over the *full* extent enumerates every tile at the given depth (up to 4^16 ≈ 4.3
    /// billion at the top depth), which is a property of an exhaustive full-grid query, not of
    /// `interleave`/`code_range` themselves — those are covered independently by (a), (b) and
    /// (d). The exactly-one-tile containment property is depth-independent, so this range is
    /// representative without exhausting memory/CPU.
    #[test]
    fn point_falls_in_exactly_one_tile(
        px in 0.0f64..1.0, py in 0.0f64..1.0,
        depth in 0u8..=8u8,
    ) {
        let e = full_extent();
        let code = tessera_spatial::morton_of(px, py, &e).raw() as u64;
        let tiles = tiles_for_bbox([0.0, 0.0, 1.0, 1.0], depth, &e);

        let containing: Vec<&Tile> = tiles.iter()
            .filter(|t| {
                let (lo, hi) = t.code_range();
                code >= lo && code < hi
            })
            .collect();

        prop_assert_eq!(containing.len(), 1, "point code {} matched {} tiles at depth {}", code, containing.len(), depth);
    }

    /// (d) A parent tile's code range contains all four of its children's ranges.
    #[test]
    fn parent_contains_children(
        depth in 0u8..16u8,
        prefix_seed in any::<u64>(),
    ) {
        // Constrain the parent prefix to depth's valid range [0, 4^depth).
        let parent_prefix = if depth == 0 { 0 } else { prefix_seed & ((1u64 << (2 * depth)) - 1) };
        let parent = Tile { prefix: parent_prefix, depth };
        let (plo, phi) = parent.code_range();

        for bit in 0u64..4 {
            let child_prefix = (parent_prefix << 2) | bit;
            let child = Tile { prefix: child_prefix, depth: depth + 1 };
            let (clo, chi) = child.code_range();
            prop_assert!(clo >= plo && chi <= phi,
                "child range [{}, {}) not contained in parent range [{}, {})", clo, chi, plo, phi);
        }
    }
}

#[test]
fn interleave_bits_matches_interleave_at_full_depth() {
    let x: u16 = 0xBEEF;
    let y: u16 = 0x1234;
    assert_eq!(
        tessera_spatial::interleave_bits(x as u32, y as u32, 16),
        tessera_spatial::interleave(x, y).raw() as u64
    );
}
