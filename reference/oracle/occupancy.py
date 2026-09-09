"""`N_occ(d)` — θ's second anchor — as the engine computes it: counted at one segment, estimated
above it.

§7.2 anchors the selection threshold at `θ_d = m_target · N_occ(d) / V_total`, where `N_occ(d)` is
the number of depth-*d* tiles holding at least one row the viewer may see. At **one segment** the
engine counts them: the walk emits each tile once and ascending, so a counter is the accumulator
and the exact answer is the cheaper one. At **two or more** it estimates them with a sketch of
`2^SKETCH_PRECISION` one-byte registers, because a tile can hold rows in several segments and an
exact accumulator would need a union, a sort or a bitset to say so — where adding a tile twice to a
sketch is adding it once. The two properties §7.2 actually needs — θ monotone in depth, and both
factors computed inside the viewer's own composed mask — an estimate has as readily as a count.
[`ladder`] owns that predicate, exactly as `crates/tessera-engine/src/occupancy.rs` does.

**Which route the differential exercises.** Every bundle this oracle opens has exactly one segment
per view (`oracle/bundle.py` — phase 1), so `Selection.n_occ` takes the counted route and
`conformance/tests/test_i7_selection.py` compares two exact counts. The estimator below is the
second implementation of the route a *live* deployment takes from its second flush onward, and it
is checked against the engine's vector for vector in `reference/tests/test_occupancy_sketch.py`
rather than through the differential. That is a real limit on the suite's coverage of the sketch
and is stated here rather than left to be discovered.

**Why the estimator is integer arithmetic throughout.** A `u64` per `(mask, view, generation,
depth)` must not depend on the libm a process linked or the order a summation took: a fixed hash
seed, an exact harmonic sum, α as an exact rational, and a fixed-point logarithm. No float appears,
so there is nothing for two implementations to disagree over — which is what would let the
differential stay exact if a fixture ever carried two segments.

**The independence this module has.** It was never in the sketch — reproducing a hash bit for bit
is transcription, and this file says so rather than pretending otherwise. It is in *what is
counted or hashed*: `Selection` derives each row's tile from the source geometry, where the engine
gallops the stored Morton column, so a build that wrote a wrong Morton column still fails the
comparison.
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


def _finish(raw_rungs: list[int], depth: int) -> list[int]:
    """The `4^d` ceiling and the running maximum, applied to raw rungs from either route.

    **One tail for both routes**, so the two properties §7.2's nesting proof reads off a ladder are
    established in one place, in the order the engine's `finish_ladder` establishes them:

    * **the ceiling `4^d`** — there are only `4^d` tiles at depth *d*, so a rung above that is
      wrong on its face. This is what makes the shallow rungs exact on the estimated route;
    * **a running maximum over the depths at or below it** — §7.2's nesting proof needs θ
      non-decreasing in depth, and while `N_occ` itself is non-decreasing by the structure of the
      grid, two adjacent *estimates* of it need not be. The maximum makes the property structural.
      It can only raise a rung, and serving more marks than the formula asks is harmless where
      serving fewer is not.

    On the counted route neither step ever binds. Both are applied anyway rather than branched
    around, for the engine's reason: a tail that behaved differently by route would be two sets of
    properties to keep true rather than one.
    """
    counts: list[int] = []
    running = 0
    for d in range(depth + 1):
        running = max(running, min(raw_rungs[d], 1 << (2 * d)))
        counts.append(running)
    return counts


def exact_ladder(tiles: Iterable[int], depth: int) -> list[int]:
    """`N_occ(d)` for every `d` in `0..=depth`, **counted**, from the depth-`depth` tile set.

    The depth-*d'* tile holding a depth-`depth` tile is `tile >> 2(depth - d')`, so every shallower
    rung is the distinct count of that image. The engine reaches the same numbers a different way —
    it counts how many times the depth-*d'* ancestor changes along an ascending walk, which within
    one segment is once per distinct ancestor — and the two agree by that argument rather than by
    transcription.
    """
    tiles = list(tiles)
    return _finish([len({t >> (2 * (depth - d)) for t in tiles}) for d in range(depth + 1)], depth)


def sketch_ladder(
    tiles: Iterable[int], depth: int, precision: int = SKETCH_PRECISION
) -> list[int]:
    """`N_occ(d)` for every `d` in `0..=depth`, **estimated**, from the depth-`depth` tile set.

    `tiles` is the **distinct** depth-`depth` tiles holding a visible row. Adding a value to a
    sketch twice is adding it once, so the engine's optimisation — descend only until the ancestor
    stops changing, which within an ascending walk visits each distinct ancestor exactly once —
    leaves the register arrays identical to what this loop produces. Order does not matter either:
    a register holds a maximum.
    """
    stride = 1 << precision
    plane = [bytearray(stride) for _ in range(depth + 1)]
    for tile in tiles:
        for d in range(depth, -1, -1):
            index, rank = register_of(tile >> (2 * (depth - d)), precision)
            if rank > plane[d][index]:
                plane[d][index] = rank
    return _finish([estimate_registers(plane[d], precision) for d in range(depth + 1)], depth)


def ladder(
    tiles: Iterable[int], depth: int, segments: int, precision: int = SKETCH_PRECISION
) -> list[int]:
    """`N_occ(d)` for every `d` in `0..=depth`, by the route the engine takes at `segments`.

    **The predicate is the engine's, verbatim**: one segment counts, two or more estimate. It lives
    here rather than at the call site so that this module is the whole statement of the anchor, and
    so that a change to the predicate is one edit on each side rather than one per caller.
    """
    if segments == 1:
        return exact_ladder(tiles, depth)
    return sketch_ladder(tiles, depth, precision)
