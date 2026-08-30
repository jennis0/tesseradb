"""Known-answer tests for `oracle.filters` — decision 0062's tree, evaluated by hand.

Every other use of this module compares it against the engine, so a rule the two got wrong the
same way is reported as agreement. These cases assert the four column families, the boolean
combinators and `none_of`'s presence requirement against answers taken from the design —
contracts §3.2, decision 0062, decision 0066, filter-surface §5.1 — over columns declared in this
file. Nothing here opens a bundle, so nothing here needs the Phase 0 corpus.

The `region` cases use an identity `quantise`, which keeps the polygon arithmetic checkable by
eye; `fixed32` itself is `oracle.morton`'s and is not under test here.
"""

from __future__ import annotations

import math

import pytest

from oracle import filters
from oracle.filters import (
    CategoryColumn,
    KeywordColumn,
    NumericColumn,
    RegionColumn,
    UnbuiltOperator,
    UnknownColumn,
)


# The fixture schema, small enough to check every expected answer against.
#
#   entity  colour (category)  name (keyword)  score (numeric)  position
#        1  red                "alpha"          10              (1, 1)
#        2  blue               "alphabet"       20              (3, 3)
#        3  green              "beta"           30              (9, 9)
#        4  —                  "gamma"          NaN             —          (no colour, no row)
COLOURS = CategoryColumn(
    values={1: "red", 2: "blue", 3: "green"},
    codes={"red": 1, "blue": 2, "green": 3},
)
NAMES = KeywordColumn(values={1: "alpha", 2: "alphabet", 3: "beta", 4: "gamma"})
SCORES = NumericColumn(values={1: 10, 2: 20, 3: 30, 4: math.nan})
REGION = RegionColumn(
    positions={1: (1, 1), 2: (3, 3), 3: (9, 9)},
    artifacts={"77": {1, 3}},
    quantise=lambda v: int(v),
)
COLUMNS = {"colour": COLOURS, "name": NAMES, "score": SCORES, "region": REGION}
EVERYONE = {1, 2, 3, 4}


def _matching(expr) -> set[int]:
    return filters.evaluate(expr, COLUMNS, EVERYONE)


# -- category: key or code, freely mixed ---------------------------------------------------------


def test_a_category_operand_resolves_by_key_or_by_code():
    """A category `eq` takes the vocabulary's key or its code (contracts §3.2).

    Working: entity 1 holds "red", pinned to code 1 by the fixture's `[vocabulary.values]` block,
    so both spellings name the same one entity. `in` is `eq` over a list and may mix the two:
    `["red", 2]` names red and blue, which is entities 1 and 2.

    Kills: resolving a code through the *stored* column rather than the declaration; a code
    operand treated as a key and matching nothing.
    """
    assert _matching({"colour": {"eq": "red"}}) == {1}
    assert _matching({"colour": {"eq": 1}}) == {1}
    assert _matching({"colour": {"in": ["red", 2]}}) == {1, 2}


def test_an_unknown_value_is_an_empty_operand_rather_than_a_refusal():
    """An unknown *value* matches nothing; an unknown *column* is a 422 (contracts §3.2).

    The two must not be conflated: refusing a value would make the filter surface an existence
    oracle over exactly what `visibility = "derived"` hides.

    Kills: raising on an unrecognised key or an unpinned code; a code lookup that falls through to
    a key comparison and matches an entity whose value happens to be the integer's string.
    """
    assert _matching({"colour": {"eq": "puce"}}) == set()
    assert _matching({"colour": {"eq": 99}}) == set()
    with pytest.raises(UnknownColumn):
        filters.matches({"shade": {"eq": "red"}}, COLUMNS, 1)


def test_a_json_true_is_not_the_code_one():
    """JSON `true` is not a vocabulary code (contracts §3.2 — a key is never an integer).

    Working: Python's `True == 1`, and "red" is pinned to code 1, so an unguarded integer
    comparison would answer `{1}` here. The correct answer is nothing: `true` names no value.

    Kills: dropping the `isinstance(operand, bool)` guard — the whole of what this case exists
    for, and a defect no engine-facing comparison would surface unless the engine made it too.
    """
    assert COLOURS.codes["red"] == 1
    assert _matching({"colour": {"eq": True}}) == set()


def test_an_entity_with_no_value_matches_no_category_predicate():
    """Absence is absence from the layer's presence bitmap, and matches nothing.

    Working: entity 4 carries no colour, so it is outside every positive `colour` predicate.

    Kills: `values.get(entity)` compared without the `None` guard, which would make a missing
    value match an operand of `None`.
    """
    assert 4 not in _matching({"colour": {"in": ["red", "blue", "green"]}})


def test_a_category_takes_only_eq_and_in():
    """A column's family fixes its operators; the rest are refused (contracts §3.2).

    Refusing an operator discloses only deployment schema, where refusing a *value* would be an
    existence oracle over the viewer's data — which is why these two sit on opposite sides.

    Kills: an oracle that pre-implemented a guess at `prefix` on a category and so would ratify
    whatever the engine happened to ship.
    """
    with pytest.raises(UnbuiltOperator):
        filters.matches({"colour": {"prefix": "r"}}, COLUMNS, 1)


# -- keyword: the four string predicates ---------------------------------------------------------


def test_the_four_string_predicates_over_the_bytes_an_entity_holds():
    """`eq`, `in`, `prefix`, `contains` (contracts §3.2; records §10).

    Working, off the fixture: `eq "alpha"` is entity 1 alone — "alphabet" is a different string.
    `prefix "alpha"` is 1 and 2. `contains "bet"` is 2 ("alphabet") and 3 ("beta").
    `in ["alpha", "gamma"]` is 1 and 4.

    `prefix "eta"` is the case that separates the two: "beta" *contains* "eta" and does not
    start with it, so the correct answer is empty where `contains "eta"` answers {3}.

    Kills: `prefix` implemented as `contains`; `contains` implemented as equality or as a prefix
    test; `in` implemented as membership of the operand in the held value.
    """
    assert _matching({"name": {"eq": "alpha"}}) == {1}
    assert _matching({"name": {"prefix": "alpha"}}) == {1, 2}
    assert _matching({"name": {"prefix": "eta"}}) == set()
    assert _matching({"name": {"contains": "eta"}}) == {3}
    assert _matching({"name": {"contains": "bet"}}) == {2, 3}
    assert _matching({"name": {"in": ["alpha", "gamma"]}}) == {1, 4}


def test_contains_the_empty_string_matches_every_value_and_no_absence():
    """`contains ""` matches every entity that *has* a value, never one that has none.

    Working: all four fixture entities carry a name, so all four match; entity 5 carries none and
    does not. This is the presence rule stated at its sharpest — the one operand where "matches
    everything" and "matches every present value" differ.

    Kills: an absence path that returns the operator's result rather than `False`.
    """
    assert _matching({"name": {"contains": ""}}) == EVERYONE
    assert filters.matches({"name": {"contains": ""}}, COLUMNS, 5) is False


# -- numeric: bounds, and the values that satisfy none of them -----------------------------------


def test_range_bounds_are_inclusive_or_exclusive_as_named():
    """`gte`/`gt`/`lte`/`lt`, each side at most one (contracts §3.2).

    Working: scores are 10, 20 and 30. `gte 20` is {2, 3}; `gt 20` is {3} — the boundary entity
    is the difference and is the only thing separating the two spellings. `{gte: 10, lt: 30}` is
    {1, 2}. Both bounds must hold, so a range is a conjunction.

    Kills: `gte` implemented as `gt` (or the reverse); a range that satisfies on either bound
    rather than both.
    """
    assert _matching({"score": {"range": {"gte": 20}}}) == {2, 3}
    assert _matching({"score": {"range": {"gt": 20}}}) == {3}
    assert _matching({"score": {"range": {"lte": 20}}}) == {1, 2}
    assert _matching({"score": {"range": {"lt": 20}}}) == {1}
    assert _matching({"score": {"range": {"gte": 10, "lt": 30}}}) == {1, 2}


def test_a_nan_satisfies_no_bound_and_no_equality():
    """IEEE comparison, without a branch saying so (`NumericColumn`'s docstring).

    Working: entity 4 holds NaN, which is neither ≥ nor ≤ nor equal to anything, so it is absent
    from every predicate above and from an unbounded-looking range as well.

    Kills: a range implemented with `not (held < lo or held > hi)`, which admits NaN by double
    negation — the classic spelling, and the reason this is written as four positive tests.
    """
    assert 4 not in _matching({"score": {"range": {"gte": -1e9, "lte": 1e9}}})
    assert _matching({"score": {"eq": math.nan}}) == set()


def test_a_range_with_no_bound_is_refused():
    """A range carries at least one bound; an empty object is not "everything".

    Kills: an empty range falling through the four `if`s and returning `True` for every entity
    that carries a value — a filter that silently stops filtering.
    """
    with pytest.raises(ValueError):
        filters.matches({"score": {"range": {}}}, COLUMNS, 1)


# -- the combinators -----------------------------------------------------------------------------


def test_all_of_and_any_of_are_intersection_and_union():
    """Decision 0062's tree: `all_of` conjoins, `any_of` disjoins, and they nest.

    Working: `colour eq red` is {1} and `name prefix alpha` is {1, 2}. Their `all_of` is {1},
    their `any_of` is {1, 2}. Nested, `any_of[ all_of[red, alpha*], score gte 30 ]` is {1} ∪ {3}.

    Kills: the two combinators transposed; a nested combinator evaluated only one level deep.
    """
    red = {"colour": {"eq": "red"}}
    alpha = {"name": {"prefix": "alpha"}}
    assert _matching({"all_of": [red, alpha]}) == {1}
    assert _matching({"any_of": [red, alpha]}) == {1, 2}
    assert _matching({"any_of": [{"all_of": [red, alpha]}, {"score": {"range": {"gte": 30}}}]}) == {
        1,
        3,
    }


def test_the_empty_combinators_are_their_operators_identities():
    """`all_of: []` matches the whole candidate; `any_of: []` matches nothing (contracts §3.2).

    The two differ, and the difference is the one a reader is most likely to assume away: an
    empty conjunction is vacuously true and an empty disjunction vacuously false.

    Kills: both spellings collapsing to the same answer; an empty list treated as a refusal.
    """
    assert _matching({"all_of": []}) == EVERYONE
    assert _matching({"any_of": []}) == set()


def test_a_node_with_more_than_one_key_is_refused():
    """A filter node is exactly one key (contracts §3.2's wire form).

    Kills: a two-key node evaluated by whichever key `dict` iteration reached first — an
    expression whose meaning depends on JSON key order.
    """
    with pytest.raises(ValueError, match="one key"):
        filters.matches({"colour": {"eq": "red"}, "name": {"eq": "alpha"}}, COLUMNS, 1)


# -- none_of: a negation that requires presence --------------------------------------------------


def test_none_of_requires_the_entity_to_carry_a_value():
    """`none_of` is "carries a value here, and none of these matches it" — decision 0066.

    Working: `none_of [colour eq red]` over the fixture. Entities 2 and 3 carry a colour that is
    not red, so both match. Entity 1 is red and does not. Entity 4 carries **no** colour, and the
    complement — `not any(...)` — would admit it; decision 0066 does not, because that would
    admit every entity whose value is merely unreachable and invert the failure arithmetic
    `filter-index.md` §5 rests on.

    Kills: `none_of` implemented as `not any_of` — the single most plausible defect in this
    module, and the one with a second statement on the Rust side
    (`none_of_requires_a_value_rather_than_taking_the_complement`).
    """
    assert _matching({"none_of": [{"colour": {"eq": "red"}}]}) == {2, 3}
    assert _matching({"any_of": [{"colour": {"eq": "red"}}]}) == {1}


def test_none_of_names_exactly_one_column():
    """The presence requirement is per column, so a `none_of` over two is refused (decision 0066).

    Working: with two columns under one negation there is no single column whose presence could
    be required, and answering anyway would silently pick one.

    Kills: a presence check taken over the first column named, or over any column named.
    """
    both = {"none_of": [{"colour": {"eq": "red"}}, {"name": {"eq": "alpha"}}]}
    with pytest.raises(ValueError, match="exactly one column"):
        filters.matches(both, COLUMNS, 2)


def test_none_of_over_several_predicates_in_one_column():
    """Several leaves are allowed while they name one column — the negation of their disjunction.

    Working: `none_of [colour eq red, colour eq blue]` leaves green (entity 3) and excludes the
    valueless entity 4.

    Kills: a `none_of` that negates only its first sub-expression.
    """
    expr = {"none_of": [{"colour": {"eq": "red"}}, {"colour": {"eq": "blue"}}]}
    assert _matching(expr) == {3}


# -- the region leaf -----------------------------------------------------------------------------


def test_a_bbox_region_is_inclusive_on_both_corners():
    """`region` by bbox, over the entity's **stored** position (selection-operand §5).

    Working: positions are (1, 1), (3, 3) and (9, 9). The box [1, 1, 3, 3] contains the first two
    on its corners and excludes the third. Entity 4 has no row, so it carries no position and
    matches neither the region nor — see below — its negation.

    Kills: an exclusive comparison on either corner; a bbox tested against the source coordinate
    rather than the quantised one (identical here by construction, and the reason `quantise` is
    applied to the operand rather than to the position).
    """
    assert _matching({"region": {"bbox": [1, 1, 3, 3]}}) == {1, 2}
    assert _matching({"region": {"bbox": [4, 4, 8, 8]}}) == set()


def test_a_polygon_region_counts_a_point_on_an_edge_as_inside():
    """Even-odd with a point on an edge inside (polygon-membership §8).

    Working: the square (0,0), (4,0), (4,4), (0,4). (1,1) and (3,3) are strictly inside, so
    entities 1 and 2 match and entity 3 at (9,9) does not. Shrinking the square to (0,0), (3,0),
    (3,3), (0,3) puts entity 2 exactly on the top-right vertex, which is still inside — the tie
    rule, and the case a strict interior test would drop.

    Kills: a strict interior test; a ray cast that double-counts a vertex and flips parity twice.
    """
    square = [[0, 0], [4, 0], [4, 4], [0, 4]]
    assert _matching({"region": {"polygon": square}}) == {1, 2}

    shrunk = [[0, 0], [3, 0], [3, 3], [0, 3]]
    assert _matching({"region": {"polygon": shrunk}}) == {1, 2}

    tiny = [[0, 0], [2, 0], [2, 2], [0, 2]]
    assert _matching({"region": {"polygon": tiny}}) == {1}


def test_a_region_by_artifact_is_the_membership_the_principal_is_served():
    """`region` by published artifact, by `tessera_id` (selection-operand §2).

    Working: artifact 77's members are entities 1 and 3. An id nobody published — or one
    suppressed, or withheld by criterion — is an **empty operand**, matching nothing, exactly as
    the server answers; it is not a refusal, for the same reason an unknown value is not.

    Kills: an unknown artifact id raising, which would make the leaf an existence oracle over
    published artifacts.
    """
    assert _matching({"region": {"artifact": 77}}) == {1, 3}
    assert _matching({"region": {"artifact": 78}}) == set()


def test_a_region_none_of_is_the_complement_within_the_rowed_entities():
    """A region is total over rowed entities, so its presence is having a row at all.

    Working: entities 1, 2 and 3 have positions; entity 4 has none. `none_of [region bbox
    [1,1,3,3]]` therefore answers {3} — outside the box and rowed — and excludes entity 4, which
    is neither in the region nor in its negation (`filter-index.md` §5, without exception).

    Kills: a presence check that looks for `column.values` on a `RegionColumn` (which has no
    such attribute) or that treats every candidate entity as present.
    """
    assert _matching({"none_of": [{"region": {"bbox": [1, 1, 3, 3]}}]}) == {3}


def test_an_unbuilt_region_spelling_is_refused_rather_than_guessed():
    """A circle or an ellipse is refused, not approximated (`UnbuiltOperator`).

    Kills: an oracle that guessed at a circle's boundary rule and so would ratify whichever one
    the engine shipped.
    """
    with pytest.raises(UnbuiltOperator):
        filters.matches({"region": {"circle": {"cx": 0, "cy": 0, "r": 5}}}, COLUMNS, 1)
    with pytest.raises(ValueError):
        filters.matches({"region": {"blob": []}}, COLUMNS, 1)


# -- the candidate is an input, not a product ----------------------------------------------------


def test_evaluate_returns_a_subset_of_the_candidate():
    """`M_sel ⊆ M_auth` — I12's mask half, structurally (filter-surface §5.1).

    Working: `contains ""` matches all four entities in the fixture, but a candidate of {2, 3}
    yields {2, 3} — the two the caller admitted. The candidate is the **composed** verdict, so an
    entity a suppression removed cannot be resurrected by a filter that would have matched it.

    Kills: `evaluate` iterating the columns' own entity sets and intersecting afterwards, which
    is the fragment-intersection shape §5.1 exists to forbid; any path that unions rather than
    filters.
    """
    assert filters.evaluate({"name": {"contains": ""}}, COLUMNS, {2, 3}) == {2, 3}
    assert filters.evaluate({"all_of": []}, COLUMNS, {2}) == {2}
    assert filters.evaluate({"name": {"contains": ""}}, COLUMNS, set()) == set()
