//! Quantisation and Morton interleaving.
//!
//! The grid is 2^16 x 2^16; Morton codes are 32-bit, low-aligned in a `u64` on disk.
//! Cells are half-open: `v = max` lands in the top cell (65535), clamped otherwise.

use tessera_types::MortonCode;

/// The spatial extent (bounding box) used to quantise `(x, y)` coordinates into cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

impl Bounds {
    pub fn is_finite(&self) -> bool {
        self.x_min.is_finite()
            && self.x_max.is_finite()
            && self.y_min.is_finite()
            && self.y_max.is_finite()
    }

    /// Reject degenerate or non-finite extents. `cell`, `morton_of` and `tiles_for_bbox` require
    /// a valid extent (finite bounds, non-empty axes); over a degenerate one the quantiser divides by zero.
    pub fn validate(&self) -> Result<(), String> {
        if !self.is_finite() {
            return Err("Bounds bounds must be finite".to_string());
        }
        if self.x_max <= self.x_min {
            return Err("Bounds requires x_max > x_min".to_string());
        }
        if self.y_max <= self.y_min {
            return Err("Bounds requires y_max > y_min".to_string());
        }
        Ok(())
    }
}

/// Positions per axis on the 32-bit grid: 2^32.
pub(crate) const FIXED_SPAN: f64 = 4_294_967_296.0;

/// Quantise a coordinate value into a 16-bit cell index: the high 16 bits of [`fixed32`].
///
/// `cell(v) = clamp( floor( (v - min) / (max - min) * 65536 ), 0, 65535 )`, computed in f64.
/// Cells are half-open; `v = max` lands in cell 65535.
///
/// Domain conditions are [`fixed32`]'s.
pub fn cell(v: f64, min: f64, max: f64) -> u16 {
    (fixed32(v, min, max) >> 16) as u16
}

/// Quantise a coordinate value into a 32-bit fixed-point position.
///
/// `fixed32(v) = clamp( floor( (v - min) / (max - min) * 2^32 ), 0, 2^32 - 1 )`, computed in f64,
/// so `fixed32(v) >> 16 == cell(v)` exactly at both clamps.
///
/// Domain: finite `v` over a valid extent (`min < max`, both finite), debug-asserted only.
pub fn fixed32(v: f64, min: f64, max: f64) -> u32 {
    debug_assert!(v.is_finite(), "fixed32(): v must be finite, got {v}");
    debug_assert!(
        min.is_finite() && max.is_finite() && max > min,
        "fixed32(): invalid extent [{min}, {max})"
    );
    let scaled = (v - min) / (max - min) * FIXED_SPAN;
    let floored = scaled.floor();
    if floored <= 0.0 {
        0
    } else if floored >= (u32::MAX as f64) {
        u32::MAX
    } else {
        floored as u32
    }
}

/// The coordinate at the centre of a 32-bit fixed-point position's step: within half a step of
/// every value [`fixed32`] maps to `q`, a step being `(max - min) / 2^32`.
pub fn unfixed32(q: u32, min: f64, max: f64) -> f64 {
    min + (f64::from(q) + 0.5) / FIXED_SPAN * (max - min)
}

/// Split a pair of 32-bit fixed-point axes into the stored `(cell code, sub-cell residual)`.
///
/// The two words concatenate to the 64-bit interleave of the inputs: `(morton << 32) | residual`,
/// since bit *i* of an axis lands at a fixed position of the code regardless of the other bits.
pub fn split32(qx: u32, qy: u32) -> (MortonCode, u32) {
    let cell = interleave((qx >> 16) as u16, (qy >> 16) as u16);
    let residual = spread(qx as u16) | (spread(qy as u16) << 1);
    (cell, residual)
}

/// Recover `(qx, qy)` from the stored `(cell code, sub-cell residual)`: the exact inverse of
/// [`split32`].
///
/// A segment stores the code, not the axes it was built from, so a merge that re-emits several
/// segments' rows as one recovers axes by going back through the interleave, a bit permutation
/// that moves nothing: `unsplit32(split32(qx, qy)) == (qx, qy)` for every input.
pub fn unsplit32(cell: MortonCode, residual: u32) -> (u32, u32) {
    let code = cell.raw();
    let qx = (compact(code) << 16) | compact(residual);
    let qy = (compact(code >> 1) << 16) | compact(residual >> 1);
    (qx, qy)
}

/// Gather the even bit positions of a 32-bit value back into the low 16 bits: the inverse of
/// [`spread`].
pub(crate) fn compact(v: u32) -> u32 {
    let mut x = v & 0x5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF;
    x
}

/// Spread the low 16 bits of `v` into the even bit positions of a 32-bit value.
///
/// Bit *i* of `v` moves to bit `2*i` of the result; odd bits are zero.
fn spread(v: u16) -> u32 {
    let mut x = v as u32;
    x = (x | (x << 8)) & 0x00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333;
    x = (x | (x << 1)) & 0x5555_5555;
    x
}

/// Interleave two 16-bit cell coordinates into a 32-bit Morton code.
///
/// Bit *i* of `cell(x)` becomes code bit `2*i`; bit *i* of `cell(y)` becomes code bit `2*i+1`.
pub fn interleave(x_cell: u16, y_cell: u16) -> MortonCode {
    MortonCode::new(spread(x_cell) | (spread(y_cell) << 1))
}

/// Quantise `(x, y)` against `extent` and interleave into a Morton code.
pub fn morton_of(x: f64, y: f64, e: &Bounds) -> MortonCode {
    debug_assert!(e.validate().is_ok(), "morton_of(): invalid extent {e:?}");
    let xc = cell(x, e.x_min, e.x_max);
    let yc = cell(y, e.y_min, e.y_max);
    interleave(xc, yc)
}

/// Interleave `d`-bit tile coordinates `(tx, ty)` into a `2d`-bit prefix in `[0, 4^d)`, the value
/// used directly as [`Tile::prefix`] at depth `d`.
pub fn interleave_bits(tx: u32, ty: u32, d: u8) -> u64 {
    debug_assert!(
        d <= 16,
        "interleave_bits(): depth {d} exceeds grid depth 16"
    );
    debug_assert!(
        tx >> d == 0 && ty >> d == 0,
        "interleave_bits(): tx/ty must fit in {d} bits, got tx={tx}, ty={ty}"
    );
    // Bit i of a coordinate lands at bit 2i whatever the depth, so the full-width spread of a
    // d-bit coordinate is already the 2d-bit prefix.
    u64::from(spread(tx as u16) | (spread(ty as u16) << 1))
}

/// A tile in the Morton quadtree: a `depth`-deep prefix over the 32-bit code space.
///
/// `depth` must be `<= 16`: the grid is 2^16 x 2^16 and codes are 32-bit, so a deeper prefix has
/// no meaning. Out-of-range depth is caught by `debug_assert!` in [`Tile::code_range`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub prefix: u64,
    pub depth: u8,
}

impl Tile {
    /// The half-open range of `u64`-widened Morton codes covered by this tile:
    /// `[prefix << (32 - 2*depth), (prefix + 1) << (32 - 2*depth))`.
    ///
    /// Debug-asserts `depth <= 16`: at greater depth `32 - 2*depth` underflows in debug builds or
    /// wraps in release, since the grid has only 32 bits of code space.
    pub fn code_range(&self) -> (u64, u64) {
        debug_assert!(
            self.depth <= 16,
            "Tile::code_range(): depth {} exceeds grid depth 16",
            self.depth
        );
        let shift = 32 - 2 * self.depth as u32;
        (self.prefix << shift, (self.prefix + 1) << shift)
    }
}

/// Enumerate the depth-`d` tiles overlapping `bbox = [x0, y0, x1, y1]` within `extent`.
///
/// Corners are quantised to cells, shifted to depth-`d` tile coordinates (`cell >> (16 - d)`),
/// and the resulting `(tx, ty)` grid, inclusive of both corners, is enumerated. The prefix for
/// each `(tx, ty)` is [`interleave_bits`] applied to the `d`-bit tile coordinates. Depth 0 yields
/// a single tile (prefix 0, whole grid).
pub fn tiles_for_bbox(bbox: [f64; 4], depth: u8, e: &Bounds) -> Vec<Tile> {
    debug_assert!(
        depth <= 16,
        "tiles_for_bbox(): depth {depth} exceeds grid depth 16"
    );
    debug_assert!(
        e.validate().is_ok(),
        "tiles_for_bbox(): invalid extent {e:?}"
    );
    if depth == 0 {
        return vec![Tile {
            prefix: 0,
            depth: 0,
        }];
    }
    let (tx_lo, tx_hi, ty_lo, ty_hi) = tile_corners(bbox, depth, e);

    let mut tiles = Vec::with_capacity(tiles_for_bbox_count(bbox, depth, e) as usize);
    for ty in ty_lo..=ty_hi {
        for tx in tx_lo..=tx_hi {
            let prefix = interleave_bits(tx as u32, ty as u32, depth);
            tiles.push(Tile { prefix, depth });
        }
    }
    tiles
}

/// The inclusive tile-coordinate corners `bbox` spans at `depth`.
fn tile_corners(bbox: [f64; 4], depth: u8, e: &Bounds) -> (u16, u16, u16, u16) {
    let [x0, y0, x1, y1] = bbox;
    let shift = 16 - depth as u32;
    let cx0 = cell(x0, e.x_min, e.x_max) >> shift;
    let cx1 = cell(x1, e.x_min, e.x_max) >> shift;
    let cy0 = cell(y0, e.y_min, e.y_max) >> shift;
    let cy1 = cell(y1, e.y_min, e.y_max) >> shift;
    (cx0.min(cx1), cx0.max(cx1), cy0.min(cy1), cy0.max(cy1))
}

/// How many tiles [`tiles_for_bbox`] would return, without allocating any of them.
///
/// Exists so a caller can refuse an over-large request before paying for it. Returns `u64` rather
/// than `usize` so a caller can compare the true magnitude against a budget rather than a value
/// already truncated to pointer width.
pub fn tiles_for_bbox_count(bbox: [f64; 4], depth: u8, e: &Bounds) -> u64 {
    if depth == 0 {
        return 1;
    }
    let (tx_lo, tx_hi, ty_lo, ty_hi) = tile_corners(bbox, depth, e);
    let wide = (tx_hi - tx_lo) as u64 + 1;
    let high = (ty_hi - ty_lo) as u64 + 1;
    wide * high
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfixed32_is_within_half_a_step_of_every_value_fixed32_maps_there() {
        let (min, max) = (-180.0, 180.0);
        let step = (max - min) / FIXED_SPAN;
        for v in [-180.0, -179.999_999_9, -0.3, 0.0, 12.345_678_9, 179.999_999_9, 180.0] {
            let back = unfixed32(fixed32(v, min, max), min, max);
            assert!((back - v).abs() <= step / 2.0 + 1e-12, "{v} came back as {back}");
        }
    }

    #[test]
    fn worked_example_from_contracts_2_5() {
        assert_eq!(interleave(6, 3).raw(), 30); // contracts §2.5
    }

    #[test]
    fn split32_concatenates_to_the_64_bit_interleave() {
        fn reference_interleave64(qx: u32, qy: u32) -> u64 {
            let mut out = 0u64;
            for i in 0..32 {
                out |= (((qx >> i) & 1) as u64) << (2 * i);
                out |= (((qy >> i) & 1) as u64) << (2 * i + 1);
            }
            out
        }
        let mut cases: Vec<(u32, u32)> = Vec::new();
        for i in 0..32 {
            cases.push((1 << i, 0));
            cases.push((0, 1 << i));
        }
        cases.extend([(0, 0), (u32::MAX, u32::MAX), (0xDEAD_BEEF, 0x0BAD_F00D)]);
        for (qx, qy) in cases {
            let (cell_code, residual) = split32(qx, qy);
            let joined = ((cell_code.raw() as u64) << 32) | residual as u64;
            assert_eq!(
                joined,
                reference_interleave64(qx, qy),
                "split32({qx:#x}, {qy:#x}) must concatenate to the 64-bit interleave"
            );
        }
    }

    #[test]
    fn split32_cell_half_agrees_with_morton_of() {
        let e = Bounds {
            x_min: -3.0,
            x_max: 11.0,
            y_min: 0.25,
            y_max: 9.75,
        };
        for i in 0..500 {
            let t = (i as f64) / 500.0;
            let (x, y) = (-3.0 + 14.0 * t, 0.25 + 9.5 * (1.0 - t));
            let qx = fixed32(x, e.x_min, e.x_max);
            let qy = fixed32(y, e.y_min, e.y_max);
            assert_eq!(split32(qx, qy).0, morton_of(x, y, &e), "at ({x}, {y})");
        }
    }

    #[test]
    fn cell_boundaries() {
        assert_eq!(cell(0.0, 0.0, 1.0), 0);
        assert_eq!(cell(1.0, 0.0, 1.0), 65535); // v = max lands in top cell
        assert_eq!(cell(0.5, 0.0, 1.0), 32768);
        assert_eq!(cell(-4.0, 0.0, 1.0), 0); // clamped
    }

    #[test]
    fn tile_range_nests() {
        let t = Tile {
            prefix: 0b11,
            depth: 1,
        }; // quadrant (1,1) at depth 1
        let (lo, hi) = t.code_range();
        assert_eq!(lo, 3u64 << 30);
        assert_eq!(hi, 4u64 << 30);
    }

    #[test]
    fn tiles_for_bbox_at_max_depth_pinpoints_single_cell() {
        let e = Bounds {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        let (px, py) = (0.31415, 0.27182);
        let code = morton_of(px, py, &e).raw() as u64;

        let tiles = tiles_for_bbox([px, py, px, py], 16, &e);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].prefix, code);
        assert_eq!(tiles[0].code_range(), (code, code + 1));
    }
}

#[cfg(test)]
mod unsplit_tests {
    use super::*;

    /// `unsplit32(split32(x)) == x` for every input.
    #[test]
    fn splitting_and_unsplitting_is_the_identity() {
        // Boundaries plus a deterministic spread: the failure mode is a bit-position error.
        let mut cases: Vec<(u32, u32)> = vec![
            (0, 0),
            (u32::MAX, u32::MAX),
            (u32::MAX, 0),
            (0, u32::MAX),
            (0xFFFF_0000, 0x0000_FFFF),
            (1, 2),
        ];
        let mut v: u32 = 0x9E37_79B9;
        for _ in 0..256 {
            v = v.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let w = v.rotate_left(16) ^ 0x5BF0_3635;
            cases.push((v, w));
        }

        for (qx, qy) in cases {
            let (cell, residual) = split32(qx, qy);
            assert_eq!(
                unsplit32(cell, residual),
                (qx, qy),
                "round trip failed for ({qx:#010x}, {qy:#010x})"
            );
        }
    }

    #[test]
    fn the_cell_carries_the_high_halves_and_the_residual_the_low() {
        let (cell, residual) = split32(0xABCD_1234, 0x5678_9ABC);
        assert_eq!(cell, interleave(0xABCD, 0x5678));
        let (qx, qy) = unsplit32(cell, residual);
        assert_eq!(qx >> 16, 0xABCD);
        assert_eq!(qy >> 16, 0x5678);
        assert_eq!(qx & 0xFFFF, 0x1234);
        assert_eq!(qy & 0xFFFF, 0x9ABC);
    }
}
