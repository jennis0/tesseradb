//! Quantisation and Morton interleaving (contracts §2.5, Reference Sheet R2).
//!
//! Grid is 2^16 x 2^16; Morton codes are 32-bit, low-aligned in a `u64` on disk.
//! Cells are half-open: `v = max` lands in the top cell (65535), clamped otherwise.

use tessera_types::MortonCode;

/// The spatial extent (bounding box) used to quantise `(x, y)` coordinates into cells.
pub struct Extent {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

/// Quantise a coordinate value into a 16-bit cell index.
///
/// `cell(v) = clamp( floor( (v - min) / (max - min) * 65536 ), 0, 65535 )`, computed in f64.
/// Cells are half-open; `v = max` lands in cell 65535 (contracts §2.5, Reference Sheet R2).
pub fn cell(v: f64, min: f64, max: f64) -> u16 {
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
pub fn morton_of(x: f64, y: f64, e: &Extent) -> MortonCode {
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
pub struct Tile {
    pub prefix: u64,
    pub depth: u8,
}

impl Tile {
    /// The half-open range of `u64`-widened Morton codes covered by this tile:
    /// `[prefix << (32 - 2*depth), (prefix + 1) << (32 - 2*depth))`.
    pub fn code_range(&self) -> (u64, u64) {
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
pub fn tiles_for_bbox(bbox: [f64; 4], depth: u8, e: &Extent) -> Vec<Tile> {
    let [x0, y0, x1, y1] = bbox;
    if depth == 0 {
        return vec![Tile {
            prefix: 0,
            depth: 0,
        }];
    }
    let shift = 16 - depth as u32;

    let cx0 = cell(x0, e.x_min, e.x_max) >> shift;
    let cx1 = cell(x1, e.x_min, e.x_max) >> shift;
    let cy0 = cell(y0, e.y_min, e.y_max) >> shift;
    let cy1 = cell(y1, e.y_min, e.y_max) >> shift;

    let (tx_lo, tx_hi) = (cx0.min(cx1), cx0.max(cx1));
    let (ty_lo, ty_hi) = (cy0.min(cy1), cy0.max(cy1));

    let mut tiles = Vec::new();
    for ty in ty_lo..=ty_hi {
        for tx in tx_lo..=tx_hi {
            let prefix = interleave_bits(tx as u32, ty as u32, depth);
            tiles.push(Tile { prefix, depth });
        }
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worked_example_from_contracts_2_5() {
        assert_eq!(interleave(6, 3).raw(), 30); // contracts §2.5
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
}
