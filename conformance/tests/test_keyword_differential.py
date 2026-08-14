"""The keyword family's differential over the base build — records §4.3, §10.

`submitter` is the catalogue's `keyword` column: the engine stores a `u32` ordinal per present
entity into that layer's own front-coded sorted dictionary and answers `eq`, `in`, `prefix` and
`contains` by resolving the needle *inside* the layer and scanning ordinals. The oracle
(`oracle.filters.KeywordColumn`) holds the strings the fixture planted and compares them, with no
dictionary and no ordinal anywhere in it. Agreement is therefore agreement between two
constructions: an implementation that interned, front-coded or resolved wrongly and then served
consistently by its own wrong ordinals disagrees here rather than being agreed with.

This module covers the base build — one layer, the whole corpus.
`conformance/tests/test_keyword_layers.py` covers what more than one layer adds, which is where
the per-layer identity of an ordinal is actually at stake.

The catalogue entries records §10 gives this family, and where each is covered:

| entry | here |
|---|---|
| the first and last value of a dictionary (ordinal boundaries) | `test_the_dictionary_s_first_and_last_values_are_served_exactly` |
| a needle absent from every dictionary | `test_a_needle_no_dictionary_holds_still_answers_and_answers_empty` |
| a value present in one layer's dictionary and absent from another's | `test_keyword_layers.py` |
| a prefix range empty in one layer and non-empty in the next | `test_keyword_layers.py` |
| an entity whose only value arrived in a coalesced extent | **not reachable** — see `test_keyword_layers.py`'s module doc |

What this module deliberately does not assert: **work**. Records §4.3 makes the unresolved needle a
scan rather than an early return so that "no item has this value" costs what "some do" costs, and
that is a timing property — surface §9's C11 row says a conformance test asserting one would fail
the design as ruled. The outcome half is asserted below; the work half is a probe's job.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat
from oracle import filters as filt
from oracle import viewport as vp
from oracle.wire import decode_viewport, decode_viewport_points, split_frames

ZOOM = 3

# The one entity whose `submitter` this module names as a single-carrier value. Inside `cross_lo`,
# on `SUBMITTER_UNIQUE_STRIDE` and off `SUBMITTER_ABSENT_STRIDE`, so a principal short of full
# coverage can see it; the *value* is derived from the generation function rather than written out,
# because the fixture owns what it planted.
SINGLE_CARRIER_ID = 65_626
SINGLE_CARRIER = cat.submitter_of(SINGLE_CARRIER_ID)

# A needle no planted value holds and no planted value contains — the sentinel case, records §4.3.
ABSENT_NEEDLE = "no-such-submitter"

# One expression per operator shape the family owns. Each is evaluated by the engine against the
# artefact and by the oracle against the fixture's own generation function, and the two must name
# the same entities exactly.
BASE_EXPRESSIONS: list[tuple[str, dict]] = [
    ("eq on a value thousands of entities hold", {"submitter": {"eq": "hub-emea-d"}}),
    ("eq on a value exactly one entity holds", {"submitter": {"eq": SINGLE_CARRIER}}),
    ("eq on the dictionary's first value", {"submitter": {"eq": cat.SUBMITTER_FIRST}}),
    ("eq on the dictionary's last value", {"submitter": {"eq": cat.SUBMITTER_LAST}}),
    ("eq on a needle no dictionary holds", {"submitter": {"eq": ABSENT_NEEDLE}}),
    (
        "in across both value families",
        {"submitter": {"in": ["hub-apac-a", "hub-latam-k", SINGLE_CARRIER]}},
    ),
    # An `in` whose members are a held value and an unheld one: the unheld member must contribute
    # nothing and must not disturb the held one, which is the list form of the sentinel rule.
    (
        "in mixing a held value with one nothing holds",
        {"submitter": {"in": ["hub-emea-d", ABSENT_NEEDLE]}},
    ),
    ("in over an empty list", {"submitter": {"in": []}}),
    # The prefix that spans hundreds of front-coded blocks — the reason the fixture's near-unique
    # family shares a stem at all. A range this wide cannot be satisfied by a single block's
    # sequential decode, so a resolve that only ever looked inside one restart is caught here.
    ("prefix spanning hundreds of dictionary blocks", {"submitter": {"prefix": "node-emea-"}}),
    ("prefix over the repeat-heavy family", {"submitter": {"prefix": "hub-"}}),
    # A prefix that is a whole key: the ordinal range is one ordinal wide, the boundary case of a
    # range scan.
    ("prefix that is exactly one key", {"submitter": {"prefix": cat.SUBMITTER_LAST}}),
    ("prefix no key begins with", {"submitter": {"prefix": "relay-"}}),
    # `contains` reaches both families through one needle, and the substring it looks for spans a
    # front-coded elision — the reason the broad route decodes every key rather than searching the
    # stored bytes flat.
    ("contains across both value families", {"submitter": {"contains": "emea"}}),
    ("contains inside a single-carrier key", {"submitter": {"contains": "-ingress-"}}),
    ("contains no key holds", {"submitter": {"contains": "zebra"}}),
    # Every key contains the empty needle, so the answer is *carries a value in this column* — and
    # an entity the absence stride left out must still not match.
    ("contains the empty needle", {"submitter": {"contains": ""}}),
]

# The principals the operator matrix runs against. Chosen for the two `contains` routes as much as
# for coverage: the engine picks the narrow route (one dictionary probe per candidate entity) below
# roughly 15% of the layer's key count and the broad route (decode every key) at or above it, so
# `sparse_0_01pct` and `container_boundary` take one route and `crossover_above` and `full_100pct`
# take the other. A matrix run against one coverage would exercise one route and say nothing about
# the other. `empty` is here because a zero-visibility principal is where a scan that forgot the
# candidate returns the whole corpus.
MATRIX_PRINCIPALS = [
    "empty",
    "sparse_0_01pct",
    "container_boundary",
    "crossover_above",
    "full_100pct",
]

# The composed sweep: a keyword leaf beside a category leaf and a string leaf, so the keyword
# operand is shown to compose under decision 0062's tree rather than only to answer alone.
COMPOSED_EXPR = {
    "all_of": [
        {"submitter": {"prefix": "hub-"}},
        {"any_of": [{"department": {"eq": "alpha"}}, {"title": {"contains": "myth"}}]},
    ]
}


def _tiles_by_id(tiles) -> dict[int, tuple[int, int, int]]:
    return {t: (v, m, s) for t, v, m, s in tiles}


def _served_entities(raw: bytes, entity_of_fx: dict[int, int]) -> set[int]:
    """The served set as entity ids, joined through the planted `fx_key`.

    A response that served nothing carries no points frame at all under the streamed format, and
    the empty set is a legitimate answer here — the zero-visibility principal and the sentinel
    needle both reach it, and both are cases this module exists to check rather than to skip.
    """
    if not any(kind == 3 for kind, _ in split_frames(raw)):
        return set()
    points = decode_viewport_points(raw)
    return {entity_of_fx[k] for k in points.column("fx_key").to_pylist()}


@pytest.fixture(scope="module")
def entity_of_fx() -> dict[int, int]:
    return {key: e for e, key in enumerate(cat.fx_keys())}


@pytest.fixture(scope="module")
def cases() -> dict[str, cat.MaskCase]:
    return {c.name: c for c in cat.catalogue()}


@pytest.fixture(scope="module")
def unfiltered(catalogue_server, cases):
    """Each principal's unfiltered tile counts, requested once.

    `visible` is the composed count and a filter never changes it, so every filtered response below
    is checked against this baseline rather than against a second unfiltered request per assertion.
    """
    baseline: dict[str, dict[int, tuple[int, int, int]]] = {}

    def get(case_name: str):
        if case_name not in baseline:
            case = cases[case_name]
            token = catalogue_server.authorise(list(case.grants))["token"]
            raw = catalogue_server.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
            baseline[case_name] = _tiles_by_id(decode_viewport(raw)[0])
        return baseline[case_name]

    return get


# ---------------------------------------------------------------------------------------------
# The fixture's own precondition: the column has the shapes the catalogue entries need
# ---------------------------------------------------------------------------------------------


def test_the_keyword_column_has_the_shapes_its_catalogue_entries_need(catalogue_filter_columns):
    """Every claim the tests below rest on, derived from the generation function and checked.

    A differential against a fixture that quietly lost its shapes is a differential that passes
    while testing nothing — a column of one repeated value would agree with the oracle on every
    assertion in this module. Each claim here is named for the entry it makes reachable.
    """
    values = catalogue_filter_columns["submitter"].values
    ordered = cat.submitter_values_in_order()

    # Ordinal boundaries: the two anchors really are the extremes of the sorted key set, and each
    # really is carried by exactly one entity.
    assert ordered[0] == cat.SUBMITTER_FIRST, ordered[:3]
    assert ordered[-1] == cat.SUBMITTER_LAST, ordered[-3:]
    carriers: dict[str, int] = {}
    for key in values.values():
        carriers[key] = carriers.get(key, 0) + 1
    assert carriers[cat.SUBMITTER_FIRST] == 1
    assert carriers[cat.SUBMITTER_LAST] == 1

    # Both carrier shapes the brief asks for: values exactly one entity holds, and values thousands
    # do. Without the second, `eq` never exercises a scan that matches more than a handful of slots.
    assert carriers[SINGLE_CARRIER] == 1, SINGLE_CARRIER
    assert sum(1 for n in carriers.values() if n == 1) > 1_000
    assert max(carriers.values()) > 1_000

    # Heavy prefix sharing: one region's prefix must span far more keys than a front-coded block
    # holds, or a prefix range never crosses a restart at all and the range scan is tested inside
    # one block. The shipped writer restarts every 16 keys
    # (`tessera_filter::DEFAULT_RESTART_INTERVAL`); a hundred clears that by enough that changing
    # the interval cannot silently make this claim false.
    spanning = [key for key in ordered if key.startswith("node-emea-")]
    assert len(spanning) > 100, len(spanning)

    # Partial presence: the ordinary case the presence bitmap exists for, and what makes
    # `contains ""` — every *value* contains it — different from "every entity".
    assert len(values) < cat.N_ITEMS

    # The prefixes this module and the layers module use as "empty in the base dictionary" really
    # are absent from it, or the flush-extent half of the catalogue entry proves nothing.
    assert not [key for key in ordered if key.startswith(("relay-", "beacon", "aab"))]
    assert ABSENT_NEEDLE not in carriers
    assert not [key for key in ordered if ABSENT_NEEDLE in key]


# ---------------------------------------------------------------------------------------------
# `/v1/meta`: the family, its operators, and the one operator it must not have
# ---------------------------------------------------------------------------------------------


def test_meta_publishes_the_keyword_family_and_never_a_range(catalogue_server, cases):
    """`submitter` is published as `keyword` with exactly `eq`, `in`, `prefix` and `contains` —
    **and `range` is not among them**, which is the assertion this test exists for.

    A keyword's stored values are ordinals: positions in the layer's own sorted dictionary. A
    numeric range over them would compare one layer's positions against another's, so the same
    request would answer differently against the base build and against a flush extent, with
    nothing on the wire to show it. The family derivation enumerates the numeric types rather than
    defaulting to them precisely so a keyword can never fall into `Numeric` (records §4.3); this is
    the published consequence, pinned where a client would read it.

    `keyword` is published as its own family rather than folded into `string` although the two take
    the same four operators, because a client draws its control from the family and a keyword has
    no listing, no autocomplete and no `/v1/categories` counterpart — the dictionary is an index
    internal and is never served.

    The refusal half is checked too: a `range` leaf on this column is a `422`, not an operand that
    matches nothing. A column's family is deployment schema, identical for every principal, so
    refusing discloses nothing — whereas answering it emptily would turn a client's mistake into a
    silent "no matches".
    """
    token = catalogue_server.authorise([])["token"]
    operands = catalogue_server.meta(token)["filter_operands"]
    by_column = {entry["column"]: entry for entry in operands}

    assert "submitter" in by_column, sorted(by_column)
    assert by_column["submitter"]["family"] == "keyword"
    assert set(by_column["submitter"]["operands"]) == {"eq", "in", "prefix", "contains"}
    assert "range" not in by_column["submitter"]["operands"], (
        "`/v1/meta` offers a keyword column a numeric range. Its values are per-layer dictionary "
        "positions, so a range over them answers one thing against the base and another against a "
        "flush extent"
    )

    case = cases["crossover_below"]
    session = catalogue_server.authorise(list(case.grants))["token"]
    resp = catalogue_server.viewport_request(
        session,
        cat.SLICE_ID,
        ZOOM,
        cat.FULL_VIEWPORT,
        filters={"submitter": {"range": {"gte": 0, "lte": 10}}},
    )
    assert resp.status_code == 422, f"{resp.status_code} {resp.text}"
    assert resp.json()["error"] == "contract"


# ---------------------------------------------------------------------------------------------
# Every operator, every principal, against the oracle
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("label,expr", BASE_EXPRESSIONS, ids=[n for n, _ in BASE_EXPRESSIONS])
@pytest.mark.parametrize("case_name", MATRIX_PRINCIPALS)
def test_every_operator_agrees_with_the_oracle_over_the_base_build(
    catalogue_server, catalogue_filter_columns, cases, entity_of_fx, unfiltered, case_name, label, expr
):
    """The matrix: one expression, one principal, exact agreement on the served set.

    θ is saturated and the caps are huge on this server, so the served set *is* the filtered set
    and equality is the strongest available form of `M_sel ⊆ M_auth` — a subset assertion alone
    would also pass an engine that dropped visible matching items, which is a different defect from
    a widened mask and must not be able to masquerade as one.

    Three checks per response, each against a different derivation: the served set against the
    oracle's brute-force walk; the per-tile `matched` total against that walk's cardinality; and
    every tile's `visible` against the principal's own unfiltered baseline, because a filter moves
    `matched` and never `visible`.
    """
    case = cases[case_name]
    token = catalogue_server.authorise(list(case.grants))["token"]
    raw = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
    )

    m_auth = set(case.entities)
    m_sel = filt.evaluate(expr, catalogue_filter_columns, m_auth)

    assert _served_entities(raw, entity_of_fx) == m_sel, f"{case_name} / {label}"

    tiles = _tiles_by_id(decode_viewport(raw)[0])
    assert sum(m for _v, m, _s in tiles.values()) == len(m_sel), (
        f"{case_name} / {label}: the per-tile `matched` counts do not sum to the filtered set's "
        "cardinality, so the counting surface and the served surface disagree"
    )
    baseline = unfiltered(case_name)
    for tile, (visible, matched, served) in tiles.items():
        assert visible == baseline[tile][0], f"tile {tile}: the filter moved `visible`"
        assert matched <= visible, f"tile {tile}: matched {matched} > visible {visible} — I12"
        assert served == matched, f"tile {tile}: served {served} != matched {matched}"


def test_a_keyword_leaf_composes_with_the_other_families(
    catalogue_bundle, catalogue_server, catalogue_filter_columns, cases, entity_of_fx
):
    """A keyword leaf inside decision 0062's tree, beside a category leaf and a string leaf.

    New capability enters through the filter contract and composes there or not at all, so an
    operand that answers correctly alone and wrongly under a conjunction is a real failure mode:
    the keyword scan takes the candidate as its input, and a leaf that ignored the candidate agrees
    with the definition for the full-coverage principal and disagrees for every other one.

    The per-tile `matched` counts are compared against the §7.1 oracle here — the one place in this
    module that recomputes the count surface from geometry rather than checking its total — so the
    counting surface is tied to the definition and not only to itself.
    """
    case = cases["crossover_above"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)
    m_sel = filt.evaluate(COMPOSED_EXPR, catalogue_filter_columns, m_auth)
    assert m_sel, "the composed expression selects nothing — the case tests an empty corner"
    assert m_sel < m_auth, "the composed expression selects everything — composition is untested"

    raw = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=COMPOSED_EXPR
    )
    assert _served_entities(raw, entity_of_fx) == m_sel

    expected = vp.counts(catalogue_bundle, m_sel, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    tiles = _tiles_by_id(decode_viewport(raw)[0])
    assert {t: m for t, (_v, m, _s) in tiles.items() if m} == expected


# ---------------------------------------------------------------------------------------------
# The catalogue entries this module owns
# ---------------------------------------------------------------------------------------------


def test_the_dictionary_s_first_and_last_values_are_served_exactly(
    catalogue_server, catalogue_filter_columns, cases, entity_of_fx
):
    """**The ordinal boundaries** (records §10). The base build's dictionary is sorted, so the
    fixture's two anchors are its ordinal 0 and its ordinal `len - 1`.

    Both are carried by exactly one entity, which is what makes this sharp rather than
    decorative: ordinal 0 is the value an engine reaching past the end of a resolve returns, and
    the last ordinal is the one an off-by-one range bound drops. A single-entity answer has no
    room to be approximately right.

    Asserted through two operators, because they fail differently. `eq` resolves to one ordinal:
    an inclusive/exclusive slip at either end of the dictionary returns the neighbouring key's
    entity instead. `prefix` on a whole key resolves to a one-wide *range*: the same slip returns
    nothing or returns the neighbour too.
    """
    case = cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)

    for label, entity, value in [
        ("first", cat.SUBMITTER_FIRST_ID, cat.SUBMITTER_FIRST),
        ("last", cat.SUBMITTER_LAST_ID, cat.SUBMITTER_LAST),
    ]:
        assert entity in m_auth, f"{label}: this principal cannot see the anchor at all"
        for operator in ("eq", "prefix"):
            expr = {"submitter": {operator: value}}
            raw = catalogue_server.viewport(
                token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
            )
            served = _served_entities(raw, entity_of_fx)
            assert served == filt.evaluate(expr, catalogue_filter_columns, m_auth)
            assert served == {entity}, (
                f"the dictionary's {label} value under `{operator}` served {sorted(served)}, not "
                f"the one entity that carries it — an ordinal boundary is off by one"
            )


def test_a_needle_no_dictionary_holds_still_answers_and_answers_empty(
    catalogue_server, catalogue_filter_columns, cases, entity_of_fx, unfiltered
):
    """**The sentinel** (records §4.3, §10). A needle absent from every layer's dictionary is an
    ordinal no slot holds, and the scan runs anyway.

    What is asserted here is the outcome: the request succeeds, matches nothing, moves no
    `visible`, and is indistinguishable in its answer from a needle that merely selects nothing.
    Skipping the scan on a dictionary miss would make *no item has this value* cheaper than *some
    do* — but that is work, and a conformance test asserting a timing property would fail the
    design as ruled (surface §9's C11 row). The module doc says the same at greater length.

    Asserted across all four operators, since each has its own miss: `eq` and `in` fail to resolve,
    `prefix` produces an empty ordinal range, and `contains` collects no ordinals from a walk that
    read every key regardless.

    The positive control is the same column, the same principal and a needle one entity does hold.
    Without it, four empty bodies are equally good evidence that the column answers nothing at all.
    """
    case = cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)

    for label, expr in [
        ("eq", {"submitter": {"eq": ABSENT_NEEDLE}}),
        ("in", {"submitter": {"in": [ABSENT_NEEDLE, "another-" + ABSENT_NEEDLE]}}),
        ("prefix", {"submitter": {"prefix": "relay-"}}),
        ("contains", {"submitter": {"contains": ABSENT_NEEDLE}}),
    ]:
        resp = catalogue_server.viewport_request(
            token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
        )
        assert resp.status_code == 200, (
            f"{label}: {resp.status_code} — a needle no dictionary holds is answered, never "
            "refused; refusing it would make the filter surface an existence oracle over the "
            "corpus's values"
        )
        tiles, points = decode_viewport(resp.content)
        assert points == [], label
        assert all(matched == 0 for _t, _v, matched, _s in tiles), label
        assert {t: v for t, (v, _m, _s) in _tiles_by_id(tiles).items()} == {
            t: v for t, (v, _m, _s) in unfiltered("crossover_below").items()
        }, f"{label}: an empty-operand request moved `visible`"
        assert filt.evaluate(expr, catalogue_filter_columns, m_auth) == set(), label

    control = {"submitter": {"eq": SINGLE_CARRIER}}
    raw = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=control
    )
    assert _served_entities(raw, entity_of_fx) == {SINGLE_CARRIER_ID}, (
        "the control failed — a value one visible entity holds was not served, so the four empty "
        "answers above may be a column that matches nothing at all"
    )
