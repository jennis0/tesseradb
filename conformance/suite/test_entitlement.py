"""The diff under truncation — correctness-suite §10's per-tile saturation rule, at the unit.

The fixture-size suites (`test_stage_invariance`, `test_crash_atomicity`) exercise the diff over
real recordings whose tiles are all saturated, so nothing there can show what the diff does when
a tile is capped — and the failure this module pins was found only by laddering the endurance
tier to a corpus no fixture reaches: a served window smaller than a tile's visible set, where a
removed row admits the next-priority row behind it and the wire moves without the corpus moving.
Recordings here are therefore fabricated — the same canonical surfaces `record` produces, with
the tile counts set to the truncated shapes a large corpus produces — because the property under
test is the *diff's*, and the diff sees only surfaces.

Three claims, and the middle one is the load-bearing one:

- displacement in a capped tile is not a defect: an entitled ingest at scale shows rows entering
  and leaving the capped window, and the diff must classify the recording as the entitlement
  rather than report the displaced rows as vanished corpus;
- a genuinely lost row is still a defect even when other tiles are capped: the capped-tile
  allowance must not become a blanket excuse, so a vanish evidenced by a saturated tile fails
  the same entitlement the displacement satisfied — by membership *and* by the net count;
- when every tile is capped the diff says so: [`Uncheckable`] equals no entitlement, because a
  check that quietly stops checking point sets is worse than one that fails.

One control closes the loop on the fabrication itself: a fully saturated pair yields the exact
[`Delta`] the fixture suites see, so the builders here demonstrably speak the diff's language
and the truncated cases differ only in the counts under test.
"""

from __future__ import annotations

import io
import json

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from .battery import Viewport
from .canonical import Streamed
from .driver import StageInvarianceViolation, StageResult, check
from .entitlement import CappedDelta, Delta, Nothing, Rows, Uncheckable, diff

ZOOM = 3
OFFSET = 2
VIEWPORT = Viewport("s0", ZOOM, bbox=(0.0, 0.0, 65536.0, 65536.0), underlay_offset=OFFSET)

#: Item identities: (tessera_id, fx). Positions are minted by [`_code`] below.
E, F = (0xE, 0xE0), (0xF, 0xF0)
A, B, C, D = (0xA, 0xA0), (0xB, 0xB0), (0xC, 0xC0), (0xD, 0xD0)


def _code(tile: int, sub: int) -> int:
    """A position code inside depth-``ZOOM`` tile `tile`, `sub` small enough that the code's
    depth-``ZOOM + OFFSET`` cell is ``tile << 2·OFFSET`` — one cell per tile, which keeps the
    underlay fabrication readable."""
    return (tile << (64 - 2 * ZOOM)) | sub


def _stream(table: pa.Table) -> bytes:
    sink = io.BytesIO()
    with ipc.new_stream(sink, table.schema) as writer:
        for batch in table.to_batches():
            writer.write_batch(batch)
    return sink.getvalue()


def _canon(
    rows: list[tuple[int, int, int]],
    tiles: dict[int, tuple[int, int, int]],
    cells: dict[int, int],
) -> Streamed:
    """One viewport recording: served points in order, the tiles batch, the underlay, and a
    trailer whose `points` matches — the shapes `canonicalise_viewport` emits."""
    points = _stream(
        pa.table(
            {
                "tessera_id": pa.array([t for t, _c, _f in rows], pa.uint64()),
                "code": pa.array([c for _t, c, _f in rows], pa.uint64()),
                "fx_key": pa.array([f for _t, _c, f in rows], pa.uint64()),
            }
        )
    )
    tiles_bytes = _stream(
        pa.table(
            {
                "tile": pa.array(sorted(tiles), pa.uint64()),
                "visible": pa.array([tiles[t][0] for t in sorted(tiles)], pa.uint64()),
                "matched": pa.array([tiles[t][1] for t in sorted(tiles)], pa.uint64()),
                "served": pa.array([tiles[t][2] for t in sorted(tiles)], pa.uint64()),
            }
        )
    )
    underlay = _stream(
        pa.table(
            {
                "cell": pa.array(sorted(cells), pa.uint64()),
                "count": pa.array([cells[c] for c in sorted(cells)], pa.uint64()),
            }
        )
    )
    trailer = json.dumps(
        {"flushes": 1, "points": len(rows)}, sort_keys=True, separators=(",", ":")
    ).encode()
    # No artifacts: this builder synthesises a points-and-counts response, which is what the
    # entitlement surfaces are about. An empty artifacts surface is the ordinary state.
    return Streamed(
        tiles=tiles_bytes,
        points=points,
        underlay=underlay,
        artifacts=b"",
        trailer=trailer,
    )


def _rows(*items: tuple[int, int], tile: int) -> list[tuple[int, int, int]]:
    return [(t, _code(tile, i), fx) for i, (t, fx) in enumerate(items)]


def _cell(tile: int) -> int:
    return tile << (2 * OFFSET)


def _result(before: Streamed, after: Streamed) -> StageResult:
    b, a = {VIEWPORT: before}, {VIEWPORT: after}
    return StageResult("synthetic", None, b, a, diff(b, a))


# -- the saturated control ----------------------------------------------------------------------


def test_a_saturated_pair_still_yields_the_exact_delta():
    """The builders speak the diff's language: with every tile saturated the result is the same
    exact [`Delta`] the fixture suites compare, so the truncated cases below differ from the
    fixture world only in the tile counts under test."""
    before = _canon(_rows(E, F, tile=1), {1: (2, 2, 2)}, {_cell(1): 2})
    after = _canon(_rows(E, F, tile=1) + _rows(D, tile=2), {1: (2, 2, 2), 2: (1, 1, 1)}, {_cell(1): 2, _cell(2): 1})
    result = _result(before, after)
    assert isinstance(result.delta, Delta)
    assert result.delta == Rows([D[1]])
    check(result, claimed=Rows([D[1]]))


# -- displacement is not a defect ---------------------------------------------------------------


def test_displacement_in_a_capped_tile_is_not_reported_as_a_defect():
    """An entitled ingest at scale: tile 2's window is capped at three marks over ten visible
    rows, the ingested row (d) enters it and displaces a resident (c). Nothing was deleted, so
    the diff must not read c's disappearance as a vanished item — the recording satisfies the
    ingest's entitlement, by membership over the saturated tile and by counts over the capped
    one."""
    before = _canon(
        _rows(E, F, tile=1) + _rows(A, B, C, tile=2),
        {1: (2, 2, 2), 2: (10, 10, 3)},
        {_cell(1): 2, _cell(2): 10},
    )
    after = _canon(
        _rows(E, F, tile=1) + _rows(A, B, D, tile=2),
        {1: (2, 2, 2), 2: (11, 11, 3)},
        {_cell(1): 2, _cell(2): 11},
    )
    result = _result(before, after)
    assert isinstance(result.delta, CappedDelta), (
        f"a capped tile must yield the partial comparison, not {result.delta!r}"
    )
    assert result.delta == Rows([D[1]]), (
        f"displacement in the capped window was not absorbed by the entitlement: {result.delta!r}"
    )
    check(result, claimed=Rows([D[1]]))  # the driver's own comparison, end to end
    assert result.delta.displaced_in == 1 and result.delta.displaced_out == 1
    assert not result.delta.appeared and not result.delta.vanished, (
        "nothing in a capped window is membership evidence"
    )


def test_a_capped_recording_never_equals_nothing():
    """Displacement is admissible only under an entitlement that moves the corpus. A stage
    entitled to `Nothing` must present bytes-equal recordings — selection is deterministic, so
    any window movement under a no-op stage is a defect, and `CappedDelta == Nothing()` must be
    False however the counts balance."""
    before = _canon(
        _rows(E, F, tile=1) + _rows(A, B, C, tile=2),
        {1: (2, 2, 2), 2: (10, 10, 3)},
        {_cell(1): 2, _cell(2): 10},
    )
    after = _canon(
        _rows(E, F, tile=1) + _rows(A, B, D, tile=2),
        {1: (2, 2, 2), 2: (10, 10, 3)},
        {_cell(1): 2, _cell(2): 10},
    )
    result = _result(before, after)
    assert isinstance(result.delta, CappedDelta)
    assert result.delta != Nothing()
    with pytest.raises(StageInvarianceViolation):
        check(result, claimed=Nothing())


# -- a genuine loss is still a defect -----------------------------------------------------------


def test_a_genuinely_lost_row_is_still_a_defect_when_other_tiles_are_capped():
    """The capped-tile allowance must not become a blanket excuse. The same entitled ingest as
    above, plus one row (f) genuinely gone from the *saturated* tile: the vanish is membership
    evidence, it satisfies no `Rows` entitlement, and the net count (+1 entitled, 0 observed)
    disagrees independently — so the defect survives even if the membership half were blinded."""
    before = _canon(
        _rows(E, F, tile=1) + _rows(A, B, C, tile=2),
        {1: (2, 2, 2), 2: (10, 10, 3)},
        {_cell(1): 2, _cell(2): 10},
    )
    after = _canon(
        _rows(E, tile=1) + _rows(A, B, D, tile=2),
        {1: (1, 1, 1), 2: (11, 11, 3)},
        {_cell(1): 1, _cell(2): 11},
    )
    result = _result(before, after)
    assert isinstance(result.delta, CappedDelta)
    assert result.delta.vanished == frozenset((F[1],))
    assert result.delta != Rows([D[1]]), (
        "a lost row in a saturated tile was absorbed by a capped-tile allowance elsewhere"
    )
    with pytest.raises(StageInvarianceViolation):
        check(result, claimed=Rows([D[1]]))


def test_a_lost_row_whose_counts_were_doctored_to_balance_is_still_a_defect():
    """Membership alone must carry the claim: doctor every count surface so the loss of f is
    invisible in the arithmetic (tile 1's counts left unmoved), and the saturated tile's point
    set still convicts — via the count/points disagreement that doctoring cannot avoid."""
    before = _canon(
        _rows(E, F, tile=1) + _rows(A, B, C, tile=2),
        {1: (2, 2, 2), 2: (10, 10, 3)},
        {_cell(1): 2, _cell(2): 10},
    )
    after = _canon(
        _rows(E, tile=1) + _rows(A, B, D, tile=2),
        {1: (2, 2, 2), 2: (11, 11, 3)},
        {_cell(1): 2, _cell(2): 11},
    )
    result = _result(before, after)
    assert result.delta != Rows([D[1]])
    with pytest.raises(StageInvarianceViolation):
        check(result, claimed=Rows([D[1]]))


# -- every tile capped is stated, not survived --------------------------------------------------


def test_every_tile_capped_is_uncheckable_not_a_silent_pass():
    """When no tile anywhere is saturated, a point-set entitlement can no longer be checked at
    all — and that is stated in the result, which equals no entitlement, rather than the diff
    quietly passing on counts alone (which displacement can compensate)."""
    before = _canon(_rows(A, B, C, tile=2), {2: (10, 10, 3)}, {_cell(2): 10})
    after = _canon(_rows(A, B, D, tile=2), {2: (11, 11, 3)}, {_cell(2): 11})
    result = _result(before, after)
    assert isinstance(result.delta, Uncheckable), (
        f"an all-capped recording must be declared uncheckable, not {result.delta!r}"
    )
    assert result.delta != Rows([D[1]]) and result.delta != Nothing()
    with pytest.raises(StageInvarianceViolation):
        check(result, claimed=Rows([D[1]]))
    assert "deep" in repr(result.delta), (
        "the result must point at the remedy — a deeper battery — not merely refuse"
    )
