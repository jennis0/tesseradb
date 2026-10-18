"""A bundle built over three shards, two of them sealed, written to through HTTP.

Twenty items at `[shards] size = 8`: the build seals shards 0 and 1 at eight items each, and shard
2 is open with four. A sealed shard takes no new item from its high water, but its items are
edited, deleted and suppressed like any other, an edit taking a new entity in the item's own shard
past the size, and a number a deletion frees there is issued again one tenancy higher. Every
placement is read from the identifiers the server answers, inverted with the bundle's identity
key; nothing here reads stored state. The tests run in order over one deployment.
"""

from __future__ import annotations

import pytest

from shards_fixture import Shards

pytestmark = pytest.mark.own_shards

N_ITEMS = 20
SIZE = 8


@pytest.fixture(scope="module")
def world(tmp_path_factory):
    w = Shards(tmp_path_factory.mktemp("sealed-shard"), N_ITEMS, SIZE)
    yield w
    w.stop()


def _in_shard(w: Shards, shard: int) -> list[int]:
    """The live items of `shard` at tenancy 0, by `fx`, lowest number first."""
    held = [(w.placement(i), i) for i in w.ids]
    return [i for (s, tenancy, _n), i in sorted(held) if s == shard and tenancy == 0]


def test_the_build_numbers_items_across_three_shards(world):
    w = world
    placed = sorted(w.placement(i) for i in w.ids)
    expected = sorted((s, 0, n) for s, count in ((0, 8), (1, 8), (2, 4)) for n in range(count))
    assert placed == expected
    status = w.status()
    assert (status["shards"]["size"], status["entity_id_high_water"]) == (SIZE, N_ITEMS)
    w.check_served()


def test_an_edit_of_a_sealed_shards_item_keeps_its_identifier_and_takes_no_open_shard_id(world):
    w = world
    item = _in_shard(w, 0)[2]
    before = w.ids[item]
    assert w.edit(item, score=1_001) == before
    assert w.high_water() == N_ITEMS + 1, "the edit took its entity from shard 0, past its size"
    w.publish()
    card = w.card(before)
    assert card.status_code == 200, card.text
    assert card.json()["fields"]["score"] == 1_001

    assert w.new_items([100]) == [(2, 0, 4)], "the open shard's high water did not move"
    assert w.edit(item, score=1_002) == before
    assert w.high_water() == N_ITEMS + 3
    w.publish()
    assert w.card(before).json()["fields"]["score"] == 1_002
    w.check_served()


def test_denies_in_sealed_shards_are_served_as_the_oracle_computes(world):
    w = world
    deleted, suppressed = _in_shard(w, 1)[3], _in_shard(w, 0)[5]
    w.changes([("delete", deleted), ("suppress", suppressed)])
    w.check_served()
    for t in (w.deleted[deleted], w.ids[suppressed]):
        assert w.card(t).status_code == 404


def test_a_compaction_reaches_every_shard_with_something_to_reclaim(world):
    w = world
    before, written = w.status()["compaction"], set(w.written)
    assert written == {0, 1, 2}
    w.compact()
    status = w.status()
    assert status["compaction"]["folds"] - before["folds"] == len(written), status["compaction"]
    assert status["compaction"]["shard"] == max(written), status["compaction"]
    assert status["overlay"]["retirable"] == 0, status["overlay"]
    w.check_served()
    [(_, gone)] = w.deleted.items()
    assert w.card(gone).status_code == 404
    resp = w.server.changes([{"op": "delete", "match": {"mosaica_id": str(gone)}}], strict=True)
    assert resp.status_code != 200, resp.text
    [suppressed] = w.suppressed
    w.changes([("unsuppress", suppressed)])
    w.check_served()


def test_a_restart_changes_nothing_and_a_freed_number_goes_to_its_sealed_shard(world):
    """Shard 1's freed number is a pool of one, which the size's threshold admits, so the next new
    item takes it one tenancy higher; shard 0's tenancy-0 ids, freed by the edits, are its edits'
    alone. The item after goes to the open shard."""
    w = world
    high_water = w.high_water()
    served = {p: w.served(p) for p in ("everyone", "two", "few")}
    w.restart()
    assert w.high_water() == high_water
    assert {p: w.served(p) for p in served} == served
    w.check_served()

    [freed] = [n for _s, _t, n in map(w.place, w.deleted.values())]
    assert w.new_items([101]) == [(1, 1, freed)]
    assert w.new_items([102]) == [(2, 0, 5)]
    item = _in_shard(w, 1)[0]
    assert w.edit(item, score=7) == w.ids[item]
    w.publish()
    w.check_served()
