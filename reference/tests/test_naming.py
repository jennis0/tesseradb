"""The identity rule's model (`oracle.naming`), pinned case by case.

Items 1 and 2 are held throughout: 1 holds `code = "a"` and `num = 10`, 2 holds `code = "b"` and
`num = 20`. Every row an ingest case creates from carries a position.
"""

from __future__ import annotations

import pytest

from oracle.naming import (
    ADDRESSING_STRICT_STATUS,
    INTEGER,
    KEYWORD,
    NAMES_NO_ITEM,
    NAMES_TWO_ITEMS,
    ONE_ITEM_TWICE,
    ONE_VALUE_TWICE,
    UNKNOWN_TESSERA_ID,
    Creates,
    Holdings,
    Names,
    Refused,
    apply_changes,
    apply_ingest,
    first_refusal,
    ignored_columns,
    malformed_addressing,
    malformed_ingest,
    refused,
    resolve_addresses,
    resolve_ingest,
    table_rows,
)

AT = {"x": 1.0, "y": 1.0}


def held() -> Holdings:
    return Holdings(
        unique={"code": KEYWORD, "num": INTEGER},
        items={1: {"code": "a", "num": 10, "x": 5.0, "y": 5.0}, 2: {"code": "b", "num": 20}},
    )


def test_a_row_with_no_identifier_creates_and_each_such_row_is_its_own_item():
    assert resolve_ingest(held(), [dict(AT), dict(AT), {"code": None, **AT}]) == [Creates()] * 3


def test_a_row_naming_nothing_without_a_position_creates_nothing():
    assert resolve_ingest(held(), [{"code": "new"}, {}]) == [Refused(NAMES_NO_ITEM)] * 2


def test_each_identifier_names_the_item_holding_it():
    rows = [{"tessera_id": "1"}, {"code": "b"}, {"num": 10}, {"tessera_id": "2", "code": "b"}]
    assert resolve_addresses(held(), rows) == [Names(1), Names(2), Names(1), Names(2)]


def test_an_integer_names_by_value_whether_sent_as_a_number_or_digits():
    assert resolve_addresses(held(), [{"num": "20"}, {"num": 20}]) == [Names(2), Names(2)]


def test_a_value_nobody_holds_names_nothing_and_does_not_stop_another_from_naming():
    rows = [{"code": "zz"}, {"code": "zz", "num": 20}, {"tessera_id": "1", "code": "new"}]
    assert resolve_addresses(held(), rows) == [Refused(NAMES_NO_ITEM), Names(2), Names(1)]
    assert resolve_ingest(held(), [{"code": "zz", **AT}]) == [Creates()]


def test_identifiers_naming_two_items_refuse_the_row():
    rows = [{"code": "a", "num": 20}, {"tessera_id": "2", "code": "a"}]
    assert resolve_ingest(held(), rows) == [Refused(NAMES_TWO_ITEMS)] * 2
    assert resolve_addresses(held(), rows) == [Refused(NAMES_TWO_ITEMS)] * 2


def test_a_tessera_id_naming_no_item_refuses_the_row_whatever_else_it_carries():
    rows = [{"tessera_id": "99"}, {"tessera_id": "99", "code": "a"}, {"tessera_id": "99", **AT}]
    assert resolve_ingest(held(), rows) == [Refused(UNKNOWN_TESSERA_ID)] * 3
    assert resolve_addresses(held(), rows) == [Refused(UNKNOWN_TESSERA_ID)] * 3


def test_a_null_names_nothing():
    assert resolve_addresses(held(), [{"tessera_id": None, "code": None}, {"code": None, "num": 10}]) == [
        Refused(NAMES_NO_ITEM),
        Names(1),
    ]


def test_an_ingest_keeps_the_first_of_two_rows_naming_one_item():
    rows = [{"code": "a"}, {"tessera_id": "1"}, {"num": 10}]
    assert resolve_ingest(held(), rows) == [
        Names(1),
        Refused(ONE_ITEM_TWICE),
        Refused(ONE_ITEM_TWICE),
    ]


def test_an_ingest_keeps_the_first_of_two_rows_setting_one_value():
    rows = [{"code": "new", **AT}, {"code": "new", **AT}, {"tessera_id": "2", "code": "new"}]
    assert resolve_ingest(held(), rows) == [
        Creates(),
        Refused(ONE_VALUE_TWICE),
        Refused(ONE_VALUE_TWICE),
    ]


def test_a_refused_row_blocks_nothing_after_it():
    rows = [
        {"code": "a", "num": 20},  # names two: not kept
        {"code": "a"},  # so this is the first row naming 1
        {"tessera_id": "99", "code": "fresh"},  # unknown: not kept
        {"code": "fresh", **AT},  # so this is the first row setting "fresh"
        {"code": "other"},  # names nothing, no position: not kept
        {"code": "other", **AT},
    ]
    assert resolve_ingest(held(), rows) == [
        Refused(NAMES_TWO_ITEMS),
        Names(1),
        Refused(UNKNOWN_TESSERA_ID),
        Creates(),
        Refused(NAMES_NO_ITEM),
        Creates(),
    ]


def test_a_row_refused_for_a_value_does_not_claim_its_item():
    # Row 1 names item 2 and carries the value row 0 set, so it is refused, and item 2 is still
    # unclaimed when row 2 names it.
    rows = [{"tessera_id": "1", "code": "v"}, {"tessera_id": "2", "code": "v"}, {"tessera_id": "2"}]
    assert resolve_ingest(held(), rows) == [Names(1), Refused(ONE_VALUE_TWICE), Names(2)]


def test_a_row_refused_for_its_item_does_not_claim_its_values():
    rows = [{"code": "a"}, {"tessera_id": "1", "num": 77}, {"code": "new", "num": 77, **AT}]
    assert resolve_ingest(held(), rows) == [Names(1), Refused(ONE_ITEM_TWICE), Creates()]


def test_where_two_reasons_apply_the_first_in_the_rules_order_is_given():
    rows = [
        {"code": "a", "num": 30},  # kept: claims item 1 and 30
        {"tessera_id": "99", "code": "a", "num": 20},  # unknown, two items, item and value twice
        {"code": "a", "num": 20},  # two items, and item 1 twice
        {"code": "zz", "num": 30},  # names nothing without a position, and 30 twice
        {"tessera_id": "1", "num": 30},  # item 1 twice, and 30 twice
    ]
    assert resolve_ingest(held(), rows) == [
        Names(1),
        Refused(UNKNOWN_TESSERA_ID),
        Refused(NAMES_TWO_ITEMS),
        Refused(NAMES_NO_ITEM),
        Refused(ONE_ITEM_TWICE),
    ]


def test_rows_resolve_against_what_was_held_before_the_batch():
    # Row 0 moves item 1 off "a"; row 1 still names item 1 by "a", so it is its second row.
    rows = [{"tessera_id": "1", "code": "moved"}, {"code": "a"}]
    assert resolve_ingest(held(), rows) == [Names(1), Refused(ONE_ITEM_TWICE)]


def test_addressing_lets_many_rows_name_one_item():
    rows = [{"code": "a"}, {"tessera_id": "1"}, {"num": 10}, {"code": "a"}]
    assert resolve_addresses(held(), rows) == [Names(1)] * 4


def test_a_deleted_item_names_nothing_and_frees_its_values():
    holdings = held()
    changes = [{"op": "delete", "match": {"tessera_id": "1"}}]
    assert apply_changes(holdings, changes, resolve_addresses(holdings, [c["match"] for c in changes])) == 1
    assert resolve_addresses(holdings, [{"tessera_id": "1"}, {"code": "a"}]) == [
        Refused(UNKNOWN_TESSERA_ID),
        Refused(NAMES_NO_ITEM),
    ]
    assert resolve_ingest(holdings, [{"code": "a", **AT}]) == [Creates()]


def test_a_suppressed_item_is_still_named():
    holdings = held()
    holdings.suppressed.add(1)
    assert resolve_addresses(holdings, [{"code": "a"}, {"tessera_id": "1"}]) == [Names(1)] * 2
    assert holdings.visible() == {2}


def test_strict_refuses_at_the_first_refused_row_with_its_status():
    verdicts = resolve_addresses(held(), [{"code": "a"}, {"code": "a", "num": 20}, {"code": "zz"}])
    assert first_refusal(verdicts) == (1, NAMES_TWO_ITEMS)
    assert ADDRESSING_STRICT_STATUS[first_refusal(verdicts)[1]] == 409
    verdicts = resolve_addresses(held(), [{"code": "zz"}, {"code": "a", "num": 20}])
    assert ADDRESSING_STRICT_STATUS[first_refusal(verdicts)[1]] == 404
    assert first_refusal(resolve_addresses(held(), [{"code": "a"}])) is None


def test_the_refused_list_names_rows_by_position():
    verdicts = resolve_ingest(held(), [{"code": "a"}, {"tessera_id": "99"}, {"code": "a"}])
    assert refused(verdicts) == [
        {"row": 1, "reason": UNKNOWN_TESSERA_ID},
        {"row": 2, "reason": ONE_ITEM_TWICE},
    ]


def test_applying_a_batch_counts_creations_edits_and_rows_that_changed_nothing():
    holdings = held()
    rows = [
        {"code": "new", **AT},
        {"tessera_id": "1", "x": 5.0, "y": 5.0},
        {"code": "b", "num": 21},
    ]
    verdicts = resolve_ingest(holdings, rows)
    counts = apply_ingest(holdings, rows, verdicts, created={0: 7})
    assert counts == {"created": 1, "edited": 1, "unchanged": 1}
    assert holdings.items[7] == {"code": "new", **AT}
    assert holdings.items[2] == {"code": "b", "num": 21}


def test_a_null_clears_a_held_unique_value_and_frees_it():
    holdings = held()
    rows = [{"tessera_id": "1", "code": None}]
    counts = apply_ingest(holdings, rows, resolve_ingest(holdings, rows), created={})
    assert counts["edited"] == 1 and "code" not in holdings.items[1]
    assert resolve_ingest(holdings, [{"code": "a", **AT}]) == [Creates()]


def test_a_member_table_is_read_row_by_row():
    assert table_rows({"tessera_id": ["1", None], "code": [None, "b"]}) == [
        {"tessera_id": "1", "code": None},
        {"tessera_id": None, "code": "b"},
    ]
    assert table_rows({}) == []
    with pytest.raises(ValueError):
        table_rows({"tessera_id": ["1"], "code": []})


def test_a_column_naming_nothing_is_ignored_and_a_match_left_empty_names_nothing():
    rows = [{"tessera_id": "1", "label": "x"}, {"label": "y"}, {}]
    assert ignored_columns(held(), rows) == {"label"}
    assert resolve_addresses(held(), rows) == [Names(1), Refused(NAMES_NO_ITEM), Refused(NAMES_NO_ITEM)]
    assert not malformed_addressing(held(), rows)


def test_a_request_with_rows_and_no_identifying_column_is_malformed():
    assert malformed_addressing(held(), [{}])
    assert malformed_addressing(held(), [{"label": "y"}, {}])
    assert not malformed_addressing(held(), [{"code": None}])
    assert not malformed_addressing(held(), table_rows({}))


def test_an_ingest_that_can_only_edit_needs_an_identifying_column():
    assert malformed_ingest(held(), [{"label": "y"}, {}])
    assert not malformed_ingest(held(), [{"label": "y"}, AT])
    assert malformed_ingest(held(), [{"label": "y"}], view=False)
    assert not malformed_ingest(held(), [{"label": "y"}, {"num": None}])
