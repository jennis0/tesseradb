"""Where new items go over several shards, read from the identifiers a server answers.

The rule: the shard holding the largest pool of freed numbers at or above the threshold,
`max(1, size >> 10)`, takes new items, a tie going to the lowest shard, each number one tenancy
above its last holder's; a sealed shard's numbers freed by an edit are its edits' alone. Otherwise
the open shard takes them from its pool and then its high water up to the size, and past it the
next shard opens. A size at or below the open shard's high water seals it, and a sealed shard
stays sealed. Each identifier is inverted with the bundle's identity key and counted by the
oracle's holder count; nothing here reads stored state.
"""

from __future__ import annotations

import random

import pytest

from shards_fixture import Shards

pytestmark = pytest.mark.own_shards

#: Threshold 2: shards 0 and 1 sealed at the size, shard 2 open with 100.
THRESHOLD_SIZE = 2048
THRESHOLD_ITEMS = 2 * THRESHOLD_SIZE + 100


@pytest.fixture(scope="module")
def threshold(tmp_path_factory):
    w = Shards(tmp_path_factory.mktemp("allocation-threshold"), THRESHOLD_ITEMS, THRESHOLD_SIZE)
    w.next_fx = 10 * THRESHOLD_ITEMS
    yield w
    w.stop()


def _fresh(w: Shards, n: int) -> list[int]:
    out = list(range(w.next_fx, w.next_fx + n))
    w.next_fx += n
    return out


def _numbers(w: Shards, shard: int) -> list[int]:
    """The numbers of `shard` its live items hold, ascending."""
    return sorted(n for s, _t, n in map(w.placement, w.ids) if s == shard)


def _delete_numbers(w: Shards, shard: int, numbers: list[int]) -> None:
    by_number = {w.placement(i)[2]: i for i in w.ids if w.placement(i)[0] == shard}
    w.changes([("delete", by_number[n]) for n in numbers])


def test_new_items_take_the_largest_pool_lowest_first(threshold):
    w = threshold
    assert sorted({w.placement(i)[0] for i in w.ids}) == [0, 1, 2]
    held = {s: _numbers(w, s) for s in (0, 1)}
    _delete_numbers(w, 0, held[0][10:13])
    _delete_numbers(w, 1, held[1][5:10])
    w.compact()
    placed = w.new_items(_fresh(w, 4))
    assert sorted(placed) == [(1, 1, n) for n in held[1][5:9]], placed
    w.publish()
    w.check_served()


def test_a_pool_below_the_threshold_is_passed_over(threshold):
    """Shard 0's pool of three is now the largest; then shard 1's one left is below the
    threshold, and the open shard takes the next from its high water."""
    w = threshold
    freed_0 = sorted(n for s, _t, n in map(w.place, w.deleted.values()) if s == 0)
    assert sorted(w.new_items(_fresh(w, 3))) == [(0, 1, n) for n in freed_0]
    assert w.new_items(_fresh(w, 1)) == [(2, 0, 100)]
    w.publish()
    w.check_served()


def test_a_batch_past_the_open_shards_size_opens_the_next(threshold):
    w = threshold
    before = w.high_water()
    room = THRESHOLD_SIZE - 101
    placed = w.new_items(_fresh(w, room + 2))
    opened = [(3, 0, 0), (3, 0, 1)]
    assert sorted(placed) == [(2, 0, n) for n in range(101, THRESHOLD_SIZE)] + opened
    assert w.high_water() == before + room + 2
    w.publish()
    w.restart()
    assert w.high_water() == before + room + 2
    assert w.new_items(_fresh(w, 1)) == [(3, 0, 2)]
    w.publish()
    w.check_served()


def test_a_lower_size_seals_the_open_shard(threshold):
    """At size 3, shard 3's high water, shard 3 seals; the threshold falls to one, so shard 1's
    one freed number is taken first, and the item after opens shard 4. Raising the size again
    leaves shard 3 sealed."""
    w = threshold
    w.d.control("PUT", "/control/shards", json={"size": 3}, expect=(201,))
    pooled = max(n for s, _t, n in map(w.place, w.deleted.values()) if s == 1)
    assert sorted(w.new_items(_fresh(w, 2))) == [(1, 1, pooled), (4, 0, 0)]
    w.d.control("PUT", "/control/shards", json={"size": THRESHOLD_SIZE}, expect=(201,))
    assert w.new_items(_fresh(w, 1)) == [(4, 0, 1)]
    w.publish()
    w.restart()
    assert w.status()["shards"]["size"] == THRESHOLD_SIZE
    assert w.new_items(_fresh(w, 1)) == [(4, 0, 2)]
    w.publish()
    w.check_served()


#: Shards 0 and 1 sealed at 64, shard 2 open with 22; the threshold is one.
CHURN_SIZE = 64
CHURN_ITEMS = 150
ROUNDS = 10


def test_ten_rounds_of_churn_open_no_shard(tmp_path):
    """Each round deletes a seeded half of the live items, compacts, and ingests as many: every
    number freed is issued again, so no shard opens and the high water stays where it was."""
    w = Shards(tmp_path / "churn", CHURN_ITEMS, CHURN_SIZE)
    try:
        rng = random.Random(20261017)
        next_fx = 10 * CHURN_ITEMS
        high_water = w.high_water()
        for _round in range(ROUNDS):
            half = rng.sample(sorted(w.ids), len(w.ids) // 2)
            w.changes([("delete", i) for i in half])
            w.compact()
            placed = w.new_items(range(next_fx, next_fx + len(half)))
            next_fx += len(half)
            assert max(s for s, _t, _n in placed) <= 2, placed
            w.publish()
            assert set(w.served("everyone")) == w.expected("everyone")
            assert w.high_water() == high_water
        w.check_served()
    finally:
        w.stop()
