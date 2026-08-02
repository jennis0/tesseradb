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


def fixed32(v: float, vmin: float, vmax: float) -> int:
    """fixed32(v) = clamp( floor( (v - min) / (max - min) * 2^32 ), 0, 2^32 - 1 ), computed in f64.

    The 32-bit widening of :func:`cell`, and the quantiser the whole cell-plus-residual
    representation rests on: ``fixed32(v) >> 16 == cell(v)`` exactly, at both clamps. That is what
    makes a residual the remainder within *the* cell a point is in, rather than a separately
    rounded quantity that might land in a neighbouring one. Contracts §2.5 pins it as precisely as
    it pins ``cell``, and this must match the Rust bit for bit — the point-set differential
    compares integer codes, which admit no tolerance.
    """
    scaled = (v - vmin) / (vmax - vmin) * 4294967296.0
    floored = math.floor(scaled)
    if floored <= 0.0:
        return 0
    if floored >= 0xFFFFFFFF:
        return 0xFFFFFFFF
    return int(floored)


def split32(qx: int, qy: int) -> tuple[int, int]:
    """Split a pair of 32-bit fixed-point axes into the stored ``(cell code, residual)``.

    The two words concatenate to the 64-bit interleave of the inputs — ``(cell << 32) | residual``
    — because interleaving is bit-local, so the high half of the 64-bit form is exactly the 32-bit
    interleave of the two high halves. This is the layout `columns.arrow` and `morton.u32` hold
    between them.
    """
    cell_code = interleave(qx >> 16, qy >> 16)
    residual = (_spread(qx & 0xFFFF) | (_spread(qy & 0xFFFF) << 1)) & 0xFFFFFFFF
    return cell_code, residual


def code_of(x: float, y: float, extent: tuple[float, float, float, float]) -> int:
    """The full 64-bit position code for ``(x, y)`` — what the points batch carries."""
    x_min, x_max, y_min, y_max = extent
    qx = fixed32(x, x_min, x_max)
    qy = fixed32(y, y_min, y_max)
    cell_code, residual = split32(qx, qy)
    return (cell_code << 32) | residual


def deinterleave64(code: int) -> tuple[int, int]:
    """Inverse of the 64-bit interleave: the two 32-bit fixed-point axes a ``code`` holds."""
    qx = 0
    qy = 0
    for bit in range(32):
        qx |= ((code >> (2 * bit)) & 1) << bit
        qy |= ((code >> (2 * bit + 1)) & 1) << bit
    return qx, qy


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


# `priority(entity_id)` is DELETED, not moved (2026-07-30 priority-as-identity-prefix fold).
#
# It was `(splitmix64(entity_id) >> 48) as u16` — an *unkeyed* residue of the entity ID, and a
# second hash construction alongside the identity's own. Contracts §2.6 r6 redefines `priority` as
# `high16(tessera_id)`, a prefix of the keyed bijection, so there is exactly one hash construction
# in the format and `identity.forward()` is the only place it lives. This deletion is on the memo's
# own change list; reintroducing a standalone priority function here would give the oracle a second
# source of truth for the sort order and for selection, which is the whole thing the fold removed.
