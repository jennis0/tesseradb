"""Known-answer tests for `oracle.occupancy` — the sketch behind θ's second anchor.

**The vectors at the bottom are the contract.** `crates/tessera-engine/src/occupancy.rs`'s
`the_ladder_matches_the_python_oracle_vector_for_vector` asserts the same three lists over the same
three inputs. That pair is what makes `conformance/tests/test_i7_selection.py` an exact
differential: the served set depends on θ, θ depends on `N_occ(d)`, and `N_occ(d)` is an estimate,
so the two implementations must produce the *same* estimate rather than two estimates within a
band. A change to the seed, the mixer, the estimator, the `4^d` ceiling or the running maximum
moves these numbers, and both sides fail together.

The cases above them check the pieces against answers that are not the other implementation's:
`ln_q32` against `math.log`, `alpha_q32` against its closed form, and the estimator against
cardinalities it is told in advance.
"""

from __future__ import annotations

import math

import pytest

from oracle import occupancy as occ


def test_the_fixed_point_logarithm_is_a_logarithm():
    """`ln_q32` uses only integer multiplication, shifts and truncating division — no `math.log`,
    no float — so that the answer cannot depend on which libm a process linked.

    Kills: an estimator that reaches for `math.log` and drifts from the engine's on another box.
    """
    for v in (1, 2, 3, 5, 7, 16, 100, 1023, 4096, 16383, 16384, 65535):
        assert abs(occ.ln_q32(v) / 2**32 - math.log(v)) < 1e-8


def test_alpha_is_the_closed_form():
    """`α_m = 0.7213 / (1 + 1.079/m)`, as an exact rational rather than a float literal."""
    for precision in range(7, 19):
        m = 1 << precision
        want = 0.7213 / (1 + 1.079 / m)
        assert abs(occ.alpha_q32(precision) / 2**32 - want) < 1e-9


def test_the_hash_is_a_bijection_on_the_inputs_that_matter():
    """The mixer is SplitMix64's finalizer, so distinct tiles cannot share a hash — a cardinality
    sketch's registers should be decided by the mixing, not by collisions in it."""
    seen = {occ.mix64(i) for i in range(20_000)}
    assert len(seen) == 20_000
    assert all(h >> 64 == 0 for h in seen), "the mixer must stay inside 64 bits"


@pytest.mark.parametrize("n", [0, 1, 2, 10, 100, 1_000, 10_000, 100_000])
def test_the_estimate_tracks_a_known_cardinality(n):
    """Five standard errors of a 2¹⁴-register sketch, with a floor of two below a hundred.

    Kills: an estimator missing the linear-counting branch, which over-reads a small set by
    thousands.
    """
    registers = bytearray(1 << occ.SKETCH_PRECISION)
    for i in range(n):
        index, rank = occ.register_of((i * 0x9E3779B9 + 7) & occ.MASK64)
        if rank > registers[index]:
            registers[index] = rank
    got = occ.estimate_registers(registers)
    assert abs(got - n) <= max(n * 0.0406, 2)


def test_the_ladder_is_monotone_and_bounded_by_the_grid():
    """Two properties §7.2 rests on, over a set whose rungs the estimator would otherwise invert.

    `N_occ(d) <= 4^d` because there are only that many tiles, and `N_occ` is non-decreasing in
    depth because every occupied tile has an occupied child. The first is a clamp and the second a
    running maximum, and together they are what makes the nesting proof hold over an estimate.

    Kills: serving a deeper rung a smaller anchor than a shallower one, which shrinks θ on a zoom
    in and drops marks the parent tile drew.
    """
    tiles = [(i * 2_654_435_761) % (1 << 32) for i in range(50_000)]
    rungs = occ.ladder(tiles, 16)
    assert rungs == sorted(rungs), "the running maximum must leave no inversion"
    for d, value in enumerate(rungs):
        assert value <= 4**d, f"depth {d}: {value} exceeds the {4**d} tiles the grid has"


CROSS_LANGUAGE_VECTORS = {
    "small": (
        [i * 0x00010001 for i in range(37)],
        [1, 1, 1, 1, 1, 1, 3, 10, 37, 37, 37, 37, 37, 37, 37, 37, 37],
    ),
    "mid": (
        [(i * 2_654_435_761) % (1 << 32) for i in range(5_000)],
        [1, 4, 16, 63, 253, 1020, 3858, 5017, 5017, 5017, 5017, 5017, 5017, 5017, 5017, 5017, 5017],
    ),
    "big": (
        [(i * 48_271) % (1 << 32) for i in range(250_000)],
        [
            1, 4, 16, 63, 253, 1020, 4079, 16333, 65536, 156628, 253239, 253239, 253239, 253239,
            253239, 253239, 253239,
        ],
    ),
}


@pytest.mark.parametrize("name", sorted(CROSS_LANGUAGE_VECTORS))
def test_the_ladder_matches_the_engine_vector_for_vector(name):
    """The engine's `the_ladder_matches_the_python_oracle_vector_for_vector` asserts these lists.

    Kills: any drift between the two implementations of the sketch — which would not surface as a
    wrong answer here, but as a conformance differential that has to be relaxed to a tolerance.
    """
    tiles, want = CROSS_LANGUAGE_VECTORS[name]
    assert occ.ladder(tiles, 16) == want
