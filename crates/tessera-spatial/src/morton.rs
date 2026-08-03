//! Quantisation and Morton interleaving (contracts §2.5).
//!
//! Grid is 2^16 x 2^16; Morton codes are 32-bit, low-aligned in a `u64` on disk.
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
    /// Reject degenerate/non-finite extents. `cell`/`morton_of`/`tiles_for_bbox` are defined
    /// only over a valid extent (all four bounds finite, both axes non-empty) — a `min == max`
    /// or infinite bound would make `cell`'s division produce NaN/±inf silently, diverging from
    /// the Python oracle, which raises instead.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.x_min.is_finite()
            && self.x_max.is_finite()
            && self.y_min.is_finite()
            && self.y_max.is_finite())
        {
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

/// Quantise a coordinate value into a 16-bit cell index.
///
/// `cell(v) = clamp( floor( (v - min) / (max - min) * 65536 ), 0, 65535 )`, computed in f64.
/// Cells are half-open; `v = max` lands in cell 65535 (contracts §2.5).
///
/// Behaviour is defined only for finite `v` over a valid extent (`min < max`, both finite) —
/// callers that quantise against user-controlled bounds must validate the [`Bounds`] first via
/// [`Bounds::validate`]. This function only asserts in debug builds; it does not itself reject
/// degenerate input, since it takes bare `min`/`max` rather than an `Bounds`.
pub fn cell(v: f64, min: f64, max: f64) -> u16 {
    debug_assert!(v.is_finite(), "cell(): v must be finite, got {v}");
    debug_assert!(
        min.is_finite() && max.is_finite() && max > min,
        "cell(): invalid extent [{min}, {max})"
    );
    let scaled = (v - min) / (max - min) * 65536.0;
    let floored = scaled.floor();
    if floored <= 0.0 {
        0
    } else if floored >= 65535.0 {
        65535
    } else {
        floored as u16
    }
}

/// Quantise a coordinate value into a 32-bit fixed-point position: [`cell`] widened by 16 bits.
///
/// `fixed32(v) = clamp( floor( (v - min) / (max - min) * 2^32 ), 0, 2^32 - 1 )`, computed in f64.
///
/// **`fixed32(v) >> 16 == cell(v)`, exactly and at both clamps**, which is the property the whole
/// cell-plus-residual representation rests on: the cell a point lands in is the same whether it is
/// derived from the coarse quantiser or from the fine one, so a residual is always the remainder
/// within *that* cell rather than a separately-rounded quantity. Flooring and then shifting equals
/// flooring at the coarser scale, and the clamp `2^32 - 1` shifts to exactly 65535. Pinned by
/// [`tests::fixed32_high_half_is_cell`].
///
/// This is why the quantiser must be a *widening* of `cell` rather than any other 32-bit mapping.
/// A scale factor of `2^32 - 1`, or round-to-nearest instead of floor, each disagree with `cell`
/// about which cell a coordinate belongs to for a large fraction of inputs — measured at ~25% of
/// uniformly distributed values for `round(t × (2^16 - 1))`, the mapping the corpus generator used
/// before this function existed. Nothing detected it, because a corpus that stores Morton codes is
/// read back through the code and never re-quantised.
///
/// Same domain conditions as [`cell`]: finite `v`, valid extent, debug-asserted only.
pub fn fixed32(v: f64, min: f64, max: f64) -> u32 {
    debug_assert!(v.is_finite(), "fixed32(): v must be finite, got {v}");
    debug_assert!(
        min.is_finite() && max.is_finite() && max > min,
        "fixed32(): invalid extent [{min}, {max})"
    );
    const SCALE: f64 = 4_294_967_296.0; // 2^32
    let scaled = (v - min) / (max - min) * SCALE;
    let floored = scaled.floor();
    if floored <= 0.0 {
        0
    } else if floored >= (u32::MAX as f64) {
        u32::MAX
    } else {
        floored as u32
    }
}

/// Split a pair of 32-bit fixed-point axes into the stored `(cell code, sub-cell residual)`.
///
/// The two words concatenate to the 64-bit interleave of the inputs — `(morton << 32) | residual`
/// — because interleaving is bit-local: bit *i* of an axis lands at a fixed position of the code
/// regardless of the other bits, so the high half of the 64-bit form is exactly the 32-bit
/// interleave of the two high halves. The residual uses the same axis convention as the cell code,
/// which is what makes that concatenation meaningful rather than merely well-typed.
pub fn split32(qx: u32, qy: u32) -> (MortonCode, u32) {
    let cell = interleave((qx >> 16) as u16, (qy >> 16) as u16);
    let residual = spread(qx as u16) | (spread(qy as u16) << 1);
    (cell, residual)
}

/// Recover `(qx, qy)` from the stored `(cell code, sub-cell residual)` — the exact inverse of
/// [`split32`].
///
/// **Exact, and that is what it is for.** A merge re-emits the rows of several segments as one, and
/// a segment stores the *code*, never the axes it was built from. Recovering the axes through
/// coordinates — dequantise to floats, re-quantise — would move points by up to a quantisation
/// step, silently, on every merge. Going back through the interleave moves nothing: it is a bit
/// permutation, so `unsplit32(split32(qx, qy)) == (qx, qy)` for every input, which the round-trip
/// test asserts rather than argues.
pub fn unsplit32(cell: MortonCode, residual: u32) -> (u32, u32) {
    let code = cell.raw();
    let qx = (compact(code) << 16) | compact(residual);
    let qy = (compact(code >> 1) << 16) | compact(residual >> 1);
    (qx, qy)
}

/// Gather the even bit positions of a 32-bit value back into the low 16 bits — the inverse of
/// [`spread`].
fn compact(v: u32) -> u32 {
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

/// Interleave `d`-bit tile coordinates `(tx, ty)` directly into a `2d`-bit prefix.
///
/// Unlike [`interleave`], which always spreads over the full 32-bit code space, this spreads
/// only the low `d` bits of each coordinate, producing a prefix in `[0, 4^d)` — the value used
/// directly as [`Tile::prefix`] at depth `d`.
pub fn interleave_bits(tx: u32, ty: u32, d: u8) -> u64 {
    debug_assert!(
        d <= 16,
        "interleave_bits(): depth {d} exceeds grid depth 16"
    );
    debug_assert!(
        tx >> d == 0 && ty >> d == 0,
        "interleave_bits(): tx/ty must fit in {d} bits, got tx={tx}, ty={ty}"
    );
    let mut prefix: u64 = 0;
    for i in 0..d as u64 {
        let xb = ((tx as u64) >> i) & 1;
        let yb = ((ty as u64) >> i) & 1;
        prefix |= xb << (2 * i);
        prefix |= yb << (2 * i + 1);
    }
    prefix
}

/// A tile in the Morton quadtree: a `depth`-deep prefix over the 32-bit code space.
///
/// Fields are public so `Tile { prefix, depth }` literal construction stays available (used
/// throughout `tiles_for_bbox` and by callers), but `depth` must be `<= 16` — the grid is
/// 2^16 x 2^16 and codes are 32-bit, so a deeper prefix has no meaning. Out-of-range depth is
/// caught by `debug_assert!` in [`Tile::code_range`], not by construction, since there is no
/// checked constructor to bypass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub prefix: u64,
    pub depth: u8,
}

impl Tile {
    /// The half-open range of `u64`-widened Morton codes covered by this tile:
    /// `[prefix << (32 - 2*depth), (prefix + 1) << (32 - 2*depth))`.
    ///
    /// Debug-asserts `depth <= 16`: at greater depth `32 - 2*depth` underflows (debug panic) or
    /// silently wraps (release), since the grid has only 32 bits of code space.
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
/// and the resulting `(tx, ty)` grid — inclusive of both corners — is enumerated. The prefix for
/// each `(tx, ty)` is the interleave of the *d-bit* tile coordinates, spread over `2d` bits
/// ([`interleave_bits`]) — not the 16-bit [`interleave`] applied to shifted inputs, which would
/// spread over the wrong number of bits. Depth 0 yields a single tile (prefix 0, whole grid).
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

/// How many tiles [`tiles_for_bbox`] *would* return, **without allocating any of them**.
///
/// Exists so a caller can refuse an over-large request before paying for it. The count is the
/// product of two inclusive tile-coordinate spans, so at depth 16 over the full extent it is
/// `65536² = 4.29×10⁹` — one 16-byte `Tile` each, ~69 GB, which is an out-of-memory abort rather
/// than a slow request. Returning `u64` rather than `usize` is deliberate: the point is to compare
/// against a budget, and a caller must be able to see the real magnitude rather than a value that
/// has already been truncated or has already exhausted the allocator.
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
    fn worked_example_from_contracts_2_5() {
        assert_eq!(interleave(6, 3).raw(), 30); // contracts §2.5
    }

    /// The property the cell-plus-residual representation rests on: the 32-bit quantiser is a
    /// *widening* of the 16-bit one, so both agree on which cell a coordinate belongs to. A
    /// different scale factor or rounding mode passes every other test in this file and fails
    /// this one — which is the whole reason it exists.
    #[test]
    fn fixed32_high_half_is_cell() {
        let (min, max) = (-12.0, 25.5);
        // Both clamps, both boundaries, and a spread of interior values including ones that
        // land exactly on a cell edge.
        let mut vs = vec![min, max, min - 1.0, max + 1.0, 0.0, 0.5, -11.999_999];
        for i in 0..2000 {
            vs.push(min + (max - min) * (i as f64) / 2000.0);
        }
        for v in vs {
            assert_eq!(
                (fixed32(v, min, max) >> 16) as u16,
                cell(v, min, max),
                "fixed32(v) >> 16 must equal cell(v) at v = {v}"
            );
        }
    }

    /// Concatenating the two stored words yields the 64-bit interleave of the two 32-bit axes.
    /// Checked at every single-bit position, which is what a change to the bit ordering breaks.
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

    /// The cell half of `split32` is the code `morton_of` would produce for the same point, so
    /// adding a residual leaves every existing Morton code byte-identical.
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

    /// **`unsplit32 ∘ split32 == identity`, asserted rather than argued.** A merge re-emits several
    /// segments' rows as one, and a segment stores the code, not the axes: this is the only route
    /// back that moves nothing. Going via coordinates — dequantise, re-quantise — would shift
    /// points by up to a quantisation step on every merge, silently.
    #[test]
    fn splitting_and_unsplitting_is_the_identity() {
        // Boundaries and a deterministic spread of interior values, rather than a random sample:
        // the failure mode is a bit-position error, which is exactly what extremes expose.
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

    /// The halves are independent: the cell is the interleave of the high halves and the residual
    /// of the low, which is what makes `(code << 32) | residual` the 64-bit interleave (see
    /// [`split32`]'s doc). A merge relies on that to sort by the code alone.
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
