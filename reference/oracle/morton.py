"""Quantisation and Morton interleaving — Reference Sheet R2, contracts §2.5.

Independently derived from the byte-level definition, not from the Rust source (the worked
example ``x_cell=6, y_cell=3 -> code 30`` is the cross-check both implementations are pinned to).
"""

from __future__ import annotations

import math

GRID_BITS = 16
GRID_SIZE = 1 << GRID_BITS  # 65536


def cell(v: float, vmin: float, vmax: float) -> int:
    """cell(v) = clamp( floor( (v - min) / (max - min) * 65536 ), 0, 65535 ), computed in f64.

    Cells are half-open; v = max lands in the top cell (65535).
    """
    scaled = (v - vmin) / (vmax - vmin) * 65536.0
    floored = math.floor(scaled)
    if floored <= 0.0:
        return 0
    if floored >= 65535.0:
        return 65535
    return int(floored)


def _spread(v: int) -> int:
    """Spread the low 16 bits of v into the even bit positions of a 32-bit value."""
    x = v & 0xFFFF
    x = (x | (x << 8)) & 0x00FF00FF
    x = (x | (x << 4)) & 0x0F0F0F0F
    x = (x | (x << 2)) & 0x33333333
    x = (x | (x << 1)) & 0x55555555
    return x


def interleave(x_cell: int, y_cell: int) -> int:
    """Bit i of cell(x) -> code bit 2i; bit i of cell(y) -> code bit 2i+1."""
    return (_spread(x_cell) | (_spread(y_cell) << 1)) & 0xFFFFFFFF


def morton_of(x: float, y: float, extent: tuple[float, float, float, float]) -> int:
    """Quantise (x, y) against extent = (x_min, x_max, y_min, y_max) and interleave."""
    x_min, x_max, y_min, y_max = extent
    xc = cell(x, x_min, x_max)
    yc = cell(y, y_min, y_max)
    return interleave(xc, yc)


def deinterleave(code: int) -> tuple[int, int]:
    """Inverse of interleave: bit 2i of code -> bit i of x cell, bit 2i+1 -> bit i of y cell."""
    return (_compact(code), _compact(code >> 1))


def _compact(v: int) -> int:
    x = v & 0x55555555
    x = (x | (x >> 1)) & 0x33333333
    x = (x | (x >> 2)) & 0x0F0F0F0F
    x = (x | (x >> 4)) & 0x00FF00FF
    x = (x | (x >> 8)) & 0x0000FFFF
    return x & 0xFFFF


def interleave_bits(tx: int, ty: int, d: int) -> int:
    """Interleave d-bit tile coordinates (tx, ty) into a 2d-bit prefix (the tile-depth analogue
    of interleave, which always spreads over the full 32-bit code space)."""
    prefix = 0
    for i in range(d):
        xb = (tx >> i) & 1
        yb = (ty >> i) & 1
        prefix |= xb << (2 * i)
        prefix |= yb << (2 * i + 1)
    return prefix


def code_range(prefix: int, depth: int) -> tuple[int, int]:
    """The half-open range of u64-widened Morton codes a depth-`depth` tile `prefix` covers."""
    shift = 32 - 2 * depth
    return (prefix << shift, (prefix + 1) << shift)


def tiles_for_bbox(
    bbox: tuple[float, float, float, float],
    depth: int,
    extent: tuple[float, float, float, float],
) -> list[int]:
    """Enumerate the depth-`depth` tile prefixes overlapping bbox = (x0, y0, x1, y1)."""
    x0, y0, x1, y1 = bbox
    x_min, x_max, y_min, y_max = extent
    if depth == 0:
        return [0]
    shift = 16 - depth

    cx0 = cell(x0, x_min, x_max) >> shift
    cx1 = cell(x1, x_min, x_max) >> shift
    cy0 = cell(y0, y_min, y_max) >> shift
    cy1 = cell(y1, y_min, y_max) >> shift

    tx_lo, tx_hi = min(cx0, cx1), max(cx0, cx1)
    ty_lo, ty_hi = min(cy0, cy1), max(cy0, cy1)

    tiles = []
    for ty in range(ty_lo, ty_hi + 1):
        for tx in range(tx_lo, tx_hi + 1):
            tiles.append(interleave_bits(tx, ty, depth))
    return tiles


def priority(entity_id: int) -> int:
    """priority(e) = (splitmix64(e) >> 48) as u16 — Reference Sheet R3, contracts §2.6."""
    mask64 = (1 << 64) - 1
    z = (entity_id + 0x9E3779B97F4A7C15) & mask64
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & mask64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & mask64
    z = z ^ (z >> 31)
    return (z >> 48) & 0xFFFF
