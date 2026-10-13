"""The holder count (`oracle.tenancy`), against identifiers built by hand.

The build holds numbers 0 to 9, one item each at tenancy 0, in shard 0, unless a case says
otherwise.
"""

from __future__ import annotations

import pytest

from oracle.identity import forward_artifact, item_key
from oracle.tenancy import HolderCount

from .conftest import KEY, item


@pytest.fixture
def count() -> HolderCount:
    return HolderCount(key=KEY, built={0: 10})


def test_a_built_number_is_held_once_at_tenancy_zero_and_a_number_above_by_nobody(count):
    assert (count.latest(3), count.holder(3)) == (0, item(3))
    assert (count.latest(10), count.holder(10)) == (None, None)
    assert count.live(item(3)) and not count.live(item(3, 1))
    assert count.named(item(3)) == 3


def test_a_new_item_takes_a_number_nobody_held_at_tenancy_zero(count):
    assert count.created(item(10)) == 10
    assert count.live(item(10))
    with pytest.raises(AssertionError):
        count.created(item(11, 1))


def test_a_deleted_items_number_is_taken_one_tenancy_higher_and_its_old_identifier_dies(count):
    count.delete(item(3))
    assert count.created(item(3, 1)) == 3
    assert count.latest(3) == 1
    assert count.seen(item(3)) and not count.live(item(3))
    with pytest.raises(AssertionError):
        count.named(item(3))
    assert count.named(item(3, 1)) == 3


@pytest.mark.parametrize(
    "issued",
    [
        pytest.param(item(3), id="the deleted item's own identifier"),
        pytest.param(item(4, 1), id="a number whose holder was not deleted"),
        pytest.param(item(12, 0, shard=1), id="another shard"),
        pytest.param(forward_artifact(KEY, 0, 12), id="an artifact"),
    ],
)
def test_an_identifier_issued_out_of_turn_is_refused(count, issued):
    count.delete(item(3))
    with pytest.raises(AssertionError):
        count.created(issued)


def test_a_number_freed_again_after_a_restart_is_taken_a_tenancy_further_on(count):
    count.delete(item(3))
    assert count.created(item(3, 2)) == 3
    assert count.live(item(3, 2)) and not count.seen(item(3, 3))
    count.delete(item(3, 2))
    with pytest.raises(AssertionError):
        count.created(item(3, 2))


def test_a_receipt_tells_new_items_from_named_ones_by_the_identifiers_held_live(count):
    count.delete(item(3))
    ids = [str(item(5)), None, str(item(3, 1)), str(item(10))]
    assert count.receipt({"created": 2, "mosaica_ids": ids}) == [3, 10]
    assert count.live(item(3, 1)) and count.live(item(10))


@pytest.mark.parametrize(
    "ids",
    [
        pytest.param([item(5), item(10)], id="a held item's identifier counted as a new one"),
        pytest.param([item(10), item(10)], id="one new identifier issued twice"),
    ],
)
def test_a_receipt_whose_new_identifiers_disagree_with_its_created_count_is_refused(count, ids):
    with pytest.raises(AssertionError):
        count.receipt({"created": 2, "mosaica_ids": [str(t) for t in ids]})


def test_a_replay_answers_identifiers_already_seen_and_counts_nothing(count):
    count.receipt({"created": 1, "mosaica_ids": [str(item(10))]})
    count.delete(item(10))
    replay = {"created": 0, "replayed": True, "mosaica_ids": [str(item(10))]}
    assert count.receipt(replay) == []
    assert count.latest(10) == 0
    with pytest.raises(AssertionError):
        count.receipt({**replay, "mosaica_ids": [str(item(11))]})


def test_each_shards_numbers_are_counted_apart():
    """Number 3 of shard 1 is not number 3 of shard 0: deleting one and reissuing its number leaves
    the other held, and a shard the bundle does not hold names no item."""
    count = HolderCount(key=KEY, built={0: 10, 1: 4})
    assert count.holder(item_key(1, 3)) == item(3, shard=1)
    assert count.latest(item_key(1, 4)) is None
    count.delete(item(3, shard=1))
    assert count.created(item(3, 1, shard=1)) == item_key(1, 3)
    assert count.live(item(3)) and not count.live(item(3, shard=1))
    with pytest.raises(AssertionError):
        count.named(item(0, shard=2))
