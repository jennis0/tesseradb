"""The two routes through the points diff must answer identically — correctness-suite §12.2.

The diff is specified over row *tuples*: a served row belongs to the change when its full column
tuple is absent from the other recording. Stated that way it is obviously right and unusably
slow, because answering it decodes every point of both recordings into Python — 0.59 s per
200,000 points across seven columns, against recordings that run to millions at endurance scale
and are compared twice per stage.

So the hot path answers the same question over Arrow and numpy instead, joining on the served
identity and comparing columns at the matched positions, and decodes only the rows that moved.
That is a different computation, and its equality with the specification is not self-evident —
null handling and duplicate identities are exactly where such a rewrite goes quietly wrong. This
module pins the equality: the tuple route is kept in the source as the reference, and every case
here runs both and demands the same answer, down to the order of the rows returned.

The randomised half matters more than the enumerated half. The shapes that broke earlier drafts
were not the ones anybody writes by hand — a row whose only change is a null appearing in one
column, a reorder that preserves the set, an identity served twice — so the generator mixes adds,
removals, edits, reorders and nulls together and the enumerated cases exist to name the
individual failures for a reader.
"""

from __future__ import annotations

import random

import numpy as np
import pyarrow as pa
import pytest

from .entitlement import _rows_at, _rows_by_tuple, _split_rows

#: Wide enough to carry a null-bearing column of each kind the render columns actually use — the
#: null semantics are where the two routes diverge if `_column_equal` is wrong.
COLUMNS = ("tessera_id", "code", "fx_key", "weight", "seen_at", "bay")


def _table(rows: list[tuple]) -> pa.Table:
    """A points table from `(tessera_id, code, fx, weight, seen_at, bay)` tuples, nulls allowed
    in the three non-identity columns."""
    return pa.table(
        {
            "tessera_id": pa.array([r[0] for r in rows], pa.uint64()),
            "code": pa.array([r[1] for r in rows], pa.uint64()),
            "fx_key": pa.array([r[2] for r in rows], pa.uint64()),
            "weight": pa.array([r[3] for r in rows], pa.float64()),
            "seen_at": pa.array([r[4] for r in rows], pa.int64()),
            "bay": pa.array([r[5] for r in rows], pa.string()),
        }
    )


def _reference(tb: pa.Table, ta: pa.Table):
    """The specification's answer: decode both sides, compare tuples."""
    rows_before = _rows_at(tb, np.arange(tb.num_rows))
    rows_after = _rows_at(ta, np.arange(ta.num_rows))
    return _rows_by_tuple(rows_before, rows_after)


def _both(tb: pa.Table, ta: pa.Table):
    """`(reference, vectorised)` as comparable `(removed keys, added keys, residual_ok)`, or
    `(reference, None)` when the vectorised route declined."""
    removed_ref, added_ref, ok_ref = _reference(tb, ta)
    reference = ([r.key for r in removed_ref], [r.key for r in added_ref], ok_ref)
    split = _split_rows(tb, ta)
    if split is None:
        return reference, None
    removed_pos, added_pos, ok_fast = split
    fast = (
        [r.key for r in _rows_at(tb, removed_pos)],
        [r.key for r in _rows_at(ta, added_pos)],
        ok_fast,
    )
    return reference, fast


def _row(rng: random.Random, ident: int) -> tuple:
    """One row, with a one-in-six chance of a null in each nullable column."""

    def maybe(value):
        return None if rng.randrange(6) == 0 else value

    return (
        ident,
        rng.randrange(1 << 40) << 24,
        ident * 16,
        maybe(round(rng.random() * 100, 3)),
        maybe(rng.randrange(1_700_000_000, 1_800_000_000)),
        maybe(f"bay-{rng.randrange(40)}"),
    )


def _mutate(rng: random.Random, row: tuple) -> tuple:
    """The same identity with one column changed — including to and from null, which is the case
    a naive `pyarrow.compute.equal` gets wrong."""
    column = rng.randrange(3, 6)
    values = list(row)
    if values[column] is None:
        values[column] = {3: 1.5, 4: 1_750_000_000, 5: "bay-new"}[column]
    else:
        values[column] = None if rng.randrange(2) else _row(rng, row[0])[column]
    return tuple(values)


# -- the enumerated failures ---------------------------------------------------------------------


def test_identical_recordings_move_nothing():
    rows = [_row(random.Random(i), i) for i in range(50)]
    reference, fast = _both(_table(rows), _table(rows))
    assert fast == reference == ([], [], True)


def test_an_added_row_is_added_on_both_routes():
    rng = random.Random(1)
    rows = [_row(rng, i) for i in range(20)]
    reference, fast = _both(_table(rows), _table(rows + [_row(rng, 99)]))
    assert fast == reference
    assert fast[1] and not fast[0]


def test_a_removed_row_is_removed_on_both_routes():
    rng = random.Random(2)
    rows = [_row(rng, i) for i in range(20)]
    reference, fast = _both(_table(rows), _table(rows[:-1]))
    assert fast == reference
    assert fast[0] and not fast[1]


def test_an_edited_row_is_one_removal_and_one_addition():
    rng = random.Random(3)
    rows = [_row(rng, i) for i in range(20)]
    edited = list(rows)
    edited[7] = _mutate(rng, edited[7])
    reference, fast = _both(_table(rows), _table(edited))
    assert fast == reference
    assert len(fast[0]) == len(fast[1]) == 1


@pytest.mark.parametrize("column", [3, 4, 5])
def test_a_column_going_null_is_a_change_not_a_coincidence(column):
    """`pyarrow.compute.equal` answers *null* when either side is null. Read as False it reports
    unchanged rows as changed; read as True it hides a real edit. Both routes must call this a
    change in one direction and a match in the other."""
    rng = random.Random(4)
    rows = [_row(rng, i) for i in range(10)]
    rows[2] = tuple(v if c != column else 1.5 if column == 3 else 17 if column == 4 else "x"
                    for c, v in enumerate(rows[2]))
    nulled = list(rows)
    nulled[2] = tuple(None if c == column else v for c, v in enumerate(nulled[2]))
    reference, fast = _both(_table(rows), _table(nulled))
    assert fast == reference
    assert len(fast[0]) == 1, "a value becoming null is a change"

    both_null = _table(nulled)
    reference, fast = _both(both_null, both_null)
    assert fast == reference == ([], [], True), "two nulls are equal, as the tuple route has it"


def test_a_reorder_that_preserves_the_set_is_reported_as_a_reorder():
    """No row moved, but the residual order did — which contracts §3.2 makes a defect. Neither
    route may absorb it into the (empty) added and removed sets."""
    rng = random.Random(5)
    rows = [_row(rng, i) for i in range(20)]
    swapped = list(rows)
    swapped[3], swapped[11] = swapped[11], swapped[3]
    reference, fast = _both(_table(rows), _table(swapped))
    assert fast == reference
    assert fast[0] == [] and fast[1] == []
    assert fast[2] is False, "the surviving rows changed order and the diff must say so"


def test_a_duplicated_identity_makes_the_vectorised_route_decline():
    """Two rows sharing a `tessera_id` break the join the fast route is built on, so it must
    refuse rather than match one of them and silently drop the other. The reference route still
    answers, so the defect is caught — slowly, which is the right trade for a shape the contract
    forbids.

    The duplicate here carries *different* content under the same identity, which is the case
    that would actually be mis-attributed: a join keyed on identity has two candidates and no
    rule for choosing. (Two byte-identical rows under one identity are invisible to the tuple
    route as well — set semantics collapse them — so nothing hangs on that case either way.)
    """
    rng = random.Random(6)
    rows = [_row(rng, i) for i in range(10)]
    rows.append(_mutate(rng, rows[4]))
    reference, fast = _both(_table(rows), _table(rows[:-1]))
    assert fast is None, "a non-unique identity must not be answered by the identity join"
    assert reference[0], "and the reference route still sees the duplicate leave"


def test_an_empty_side_is_wholly_one_directional():
    rng = random.Random(7)
    rows = [_row(rng, i) for i in range(12)]
    reference, fast = _both(_table(rows), _table([]))
    assert fast == reference
    assert len(fast[0]) == 12 and fast[1] == []


# -- the randomised half -------------------------------------------------------------------------


@pytest.mark.parametrize("seed", range(40))
def test_the_routes_agree_on_a_mixed_recording(seed):
    """Adds, removals, edits, a reorder and nulls in one recording — the combination is what a
    real stage produces, and the combination is where an off-by-one in the position arithmetic
    shows up as a row attributed to the wrong side."""
    rng = random.Random(seed)
    size = rng.randrange(1, 120)
    identities = rng.sample(range(1, 10_000), size)
    before = [_row(rng, i) for i in identities]

    after = [r for r in before if rng.randrange(8)]  # ~1 in 8 removed
    after = [_mutate(rng, r) if rng.randrange(10) == 0 else r for r in after]
    for _ in range(rng.randrange(4)):
        after.append(_row(rng, rng.randrange(10_000, 20_000)))
    if len(after) > 2 and rng.randrange(3) == 0:
        i, j = rng.randrange(len(after)), rng.randrange(len(after))
        after[i], after[j] = after[j], after[i]

    reference, fast = _both(_table(before), _table(after))
    assert fast is not None, "identities are unique here, so the fast route must answer"
    assert fast == reference
