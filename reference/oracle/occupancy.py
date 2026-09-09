"""`N_occ(d)` — θ's second anchor — as the engine computes it: a HyperLogLog sketch.

§7.2 anchors the selection threshold at `θ_d = m_target · N_occ(d) / V_total`, where `N_occ(d)` is
the number of depth-*d* tiles holding at least one row the viewer may see. The engine does not
count them. It estimates them, with a sketch of `2^SKETCH_PRECISION` one-byte registers, because
the two properties §7.2 actually needs — θ monotone in depth, and both factors computed inside the
viewer's own composed mask — are properties an estimate has as readily as a count.

**This module exists so the differential stays exact rather than moving to a tolerance.** The
served set depends on θ and θ depends on `N_occ`, so an approximate `N_occ` that this side could
not reproduce would force `conformance/tests/test_i7_selection.py` to compare within a band — and a
band is a weaker guarantee than the suite has today. Every step below is therefore integer
arithmetic: a fixed hash seed, an exact `u128`-equivalent harmonic sum, α as an exact rational, and
a fixed-point logarithm. No float appears, so there is no libm, no summation order and no rounding
mode for the two implementations to disagree over. `crates/tessera-engine/src/occupancy.rs` is the
same arithmetic in Rust.

**The independence this module still has.** It was never in the sketch — reproducing a hash bit for
bit is transcription, and this file says so rather than pretending otherwise. It is in *what is
hashed*: `Selection` derives each row's tile from the source geometry, where the engine gallops the
stored Morton column, so a build that wrote a wrong Morton column still fails the comparison. The
sketch is downstream of that and cannot hide it: two different tile sets give two different
register arrays.
"""

from __future__ import annotations

from typing import Iterable

MASK64 = (1 << 64) - 1

SKETCH_PRECISION = 14
"""`log2` of the register count. 2¹⁴ one-byte registers is 16 KiB per depth and a relative standard
error of `1.04 / sqrt(2^14)` = 0.81%. The engine's `SKETCH_PRECISION`, and it must stay equal to
it: the register array's width is part of the answer, not a tuning knob one side may hold alone."""

SKETCH_SEED = 0x9E3779B97F4A7C15
"""The constant SplitMix64 stirs into an input before its finalizer. Fixed, never randomised —
`N_occ` is memoised per `(session, view, generation, depth)` and two processes must answer the same
number for the same mask."""

LN2_Q32 = 2977044472
"""`ln 2 · 2³²`, rounded to nearest."""


def mix64(z: int) -> int:
    """SplitMix64's finalizer: a bijection on 64 bits with full avalanche.

    A bijection rather than a hash with collisions, which is what a cardinality sketch wants: the
    tile indices at one depth are distinct by construction, so the register a tile lands in is
    decided by the mixing alone.
    """
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK64
    return z ^ (z >> 31)


def register_of(tile: int, precision: int = SKETCH_PRECISION) -> tuple[int, int]:
    """Which register a tile index lands in, and the rank it proposes for it.

    The top `precision` bits of the hash choose the register and the run of zeros below them is the
    rank. The `| 1 << (precision - 1)` caps the run at `64 - precision`, so a hash whose low bits
    are all zero produces the largest rank a register can hold rather than one it cannot.
    """
    h = mix64((tile + SKETCH_SEED) & MASK64)
    index = h >> (64 - precision)
    w = ((h << precision) & MASK64) | (1 << (precision - 1))
    rank = 64 - w.bit_length() + 1
    return index, rank


def alpha_q32(precision: int = SKETCH_PRECISION) -> int:
    """`α_m · 2³²`, exactly.

    `α_m = 0.7213 / (1 + 1.079/m)`, so `α_m · 2³² = 7213 · m · 2³² / (10 · (1000m + 1079))`, to
    nearest. Written as the rational rather than as a float literal so neither implementation is
    transcribing the other's rounding.
    """
    m = 1 << precision
    num = 7213 * m * (1 << 32)
    den = 10 * (1000 * m + 1079)
    return (2 * num + den) // (2 * den)


def ln_q32(v: int) -> int:
    """`ln(v) · 2³²` for `v >= 1`, by integer arithmetic alone.

    `v = 2^k · f` with `f` in `[1, 2)`, so `ln v = k · ln 2 + ln f`, and `ln f = 2 · atanh(z)` for
    `z = (f − 1) / (f + 1)` in `[0, 1/3]`. The series `z + z³/3 + z⁵/5 + …` loses a factor of nine
    per term there, so twenty terms are far past `Q32`'s last bit; the count is fixed rather than
    tested against zero so the loop is the same shape in both implementations.
    """
    k = v.bit_length() - 1
    one = 1 << 32
    f = (v << 32) >> k
    z = ((f - one) << 32) // (f + one)
    z2 = (z * z) >> 32
    term = z
    acc = z
    i = 3
    while i <= 41:
        term = (term * z2) >> 32
        acc += term // i
        i += 2
    return k * LN2_Q32 + 2 * acc


def estimate_registers(registers: bytearray, precision: int = SKETCH_PRECISION) -> int:
    """The estimated number of distinct values behind one register array.

    A histogram of the ranks rather than a pass of wide shifts: there are at most `64 - precision +
    1` distinct rank values, so the sum is a handful of terms however many registers there are.
    """
    rank_max = 64 - precision + 1
    inv = 0
    for rank in range(rank_max + 1):
        count = registers.count(rank)
        if count:
            inv += count << (rank_max - rank)
    zeros = registers.count(0)

    m = 1 << precision
    raw = min((alpha_q32(precision) << (precision + 33)) // inv, MASK64)

    # Linear counting below 2.5m, which is where HyperLogLog's estimator is biased and where a
    # register array with empty slots has a better one available: with `V` of `m` registers still
    # empty, `m · ln(m/V)` is the balls-into-bins estimate.
    if zeros > 0 and raw <= (5 * m) // 2:
        ln_ratio_q32 = precision * LN2_Q32 - ln_q32(zeros)
        return (m * ln_ratio_q32) >> 32
    # No large-range correction: the hash is 64 bits wide, so the `2^32/30` threshold a 32-bit
    # HyperLogLog needs is unreachable here.
    return raw


def ladder(tiles: Iterable[int], depth: int, precision: int = SKETCH_PRECISION) -> list[int]:
    """`N_occ(d)` for every `d` in `0..=depth`, from the depth-`depth` occupied tile set.

    `tiles` is the **distinct** depth-`depth` tiles holding a visible row. The depth-*d'* tile
    holding one of them is `tile >> 2(depth - d')`, so every shallower rung is a function of the
    same set — which is what lets the engine fill the whole ladder from one walk. Adding a value to
    a sketch twice is adding it once, so the engine's optimisation (descend only until the ancestor
    stops changing, which within an ascending walk visits each distinct ancestor exactly once)
    leaves the register arrays identical to what this loop produces. Order does not matter either:
    a register holds a maximum.

    Two things are applied to each rung before it is served, in this order:

    * **the ceiling `4^d`** — there are only `4^d` tiles at depth *d*, so an estimate above that is
      wrong on its face. This is what makes the shallow rungs exact rather than merely close;
    * **a running maximum over the depths at or below it** — §7.2's nesting proof needs θ
      non-decreasing in depth, and while `N_occ` itself is non-decreasing by the structure of the
      grid, two adjacent *estimates* of it need not be. The maximum makes the property structural.
      It can only raise a rung, and serving more marks than the formula asks is harmless where
      serving fewer is not.
    """
    stride = 1 << precision
    plane = [bytearray(stride) for _ in range(depth + 1)]
    for tile in tiles:
        for d in range(depth, -1, -1):
            index, rank = register_of(tile >> (2 * (depth - d)), precision)
            if rank > plane[d][index]:
                plane[d][index] = rank

    counts: list[int] = []
    running = 0
    for d in range(depth + 1):
        running = max(running, min(estimate_registers(plane[d], precision), 1 << (2 * d)))
        counts.append(running)
    return counts
