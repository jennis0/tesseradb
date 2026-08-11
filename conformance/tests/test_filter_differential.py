"""The attribute-filter differential — I12's mask half, driven over HTTP against the oracle.

`conformance.md` §4.6 recorded I12 as *not covered — nothing to test*. The filter surface now
exists, and this module is what moves that row: engine versus `oracle.filters` across the
adversarial mask catalogue, exact equality, with the oracle deriving every expected set from the
fixture's own generation functions rather than from the `attrs/` artefact the engine serves from
(see `oracle/filters.py`'s module doc for why that is what makes it a differential and not a
transcription).

What this module covers:

- **I12, mask half** — `M_sel ⊆ M_auth` across the catalogue's principals, asserted three ways:
  per-tile `matched ≤ visible`, the filtered served set a subset of the unfiltered one, and the
  filtered served set *equal* to the oracle's brute-force `M_sel` (θ is saturated on this server,
  so served == matched == the whole filtered set — a subset check would pass a filter that
  dropped visible items, which is I7's failure mode, not I12's).
- **The frontier direction** — a filter moves `matched` and never `visible`, per tile, including
  under the empty-combinator identities (`all_of: []` is the whole candidate, `any_of: []` is
  nothing).
- **C11 / per-point-attributes §3.8** — a value the principal cannot see a member of, a declared
  value with no members anywhere, and a value that does not exist are indistinguishable **in
  outcome**: byte-identical bodies. Work-indistinguishability is a timing property, measured in
  `probes/2026-08-08-filter-layout/` and deliberately not asserted here — surface §9's C11 row
  says a conformance test asserting it would fail the design as ruled.
- **Composition** — `any_of` across columns, mixed key/code operands, and a nested
  `all_of`/`any_of` tree, against brute force; plus two algebraically equal spellings of one
  expression, which must serve identical responses.
- **Refusals** — an unknown *column* is a `422`; an unknown *value* is not; `none_of` and
  `match` are `422` because they are unbuilt (decision 0062), and an operator outside the
  column's family is a `422` because a refusal is a function of the request and the deployment's
  schema, never of the viewer's data (§10.6).

What it deliberately does **not** cover, so the next reader is not left inferring it:

- **I12's frontier half and I3.** There is no label service, no frontier and no
  `min_visible_members` in the tree, so labels-gate-on-`M_auth` has nothing to gate.
  `test_i3_has_no_surface_to_test` pins the absence: the day `/v1/labels` answers, it fails and
  this module owes the coverage.
- **Rule S over filter results** — a suppressed entity absent from every filter count although
  its postings and its attribute value stand (surface §9's row). The catalogue servers here are
  session-scoped and shared, and a suppression would mutate them under every other module; the
  overlay-journal differential owns the machinery, and extending it with a filtered request is
  the natural home. Uncovered here, stated rather than implied.
- **θ-live filtered selection** — §5.2's rule that a filtered selection stays anchored on
  `M_auth`. This module runs θ saturated so that a composition bug and a θ bug cannot masquerade
  as each other (the same argument `conftest.catalogue_server` makes for the mask tests).
- **Post-build ingest** — a flush now appends a value-column extent and a filter answers over the
  entities it published (`filter-index.md` §2.1). The fixture here is build-only, so that path is
  exercised in `crates/tessera-engine/tests/filtering.rs` against a real flush rather than over
  HTTP; extending the catalogue to ingest would make every other module's server mutable under it,
  which is the same reason Rule S is not driven here.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat
from oracle import filters as filt
from oracle import viewport as vp
from oracle.wire import decode_viewport, decode_viewport_points

ZOOM = 3

# The sweep expression: disjunction across both columns, with a category leaf mixing a key
# ("alpha") and a code (3, gamma's pinned code) in one `in` list — contracts §3.2 says the two
# are freely mixed, so the sweep proves it everywhere rather than in a corner case.
SWEEP_EXPR = {
    "any_of": [
        {"department": {"in": ["alpha", 3]}},
        {"title": {"prefix": "smi"}},
    ]
}


def _tiles_by_id(tiles) -> dict[int, tuple[int, int, int]]:
    return {t: (v, m, s) for t, v, m, s in tiles}


def _served_entities(raw: bytes) -> set[int]:
    """The served set as entity ids, joined through the planted `fx_key` — the suite's one
    legitimate handle→item route (no reverse map, no I10 tension)."""
    entity_of_fx = {key: e for e, key in enumerate(cat.fx_keys())}
    points = decode_viewport_points(raw)
    return {entity_of_fx[k] for k in points.column("fx_key").to_pylist()}


@pytest.fixture(scope="module")
def sweep_cases():
    return {c.name: c for c in cat.catalogue()}


# ---------------------------------------------------------------------------------------------
# The fixture's own precondition: the attribute is decorrelated from the grants
# ---------------------------------------------------------------------------------------------


def test_the_filter_columns_are_decorrelated_from_the_grant_structure(catalogue_filter_columns):
    """If the attribute tracked the permission, every cross-principal assertion below would pass
    while meaning nothing: masking and filtering would select the same items for different
    reasons. So the decorrelation is a checked precondition, not a comment — each cycling
    department value must select a strict, non-empty subset of every mask case large enough to
    contain the cycle, and the three planted special values must have exactly the memberships
    the C11 test depends on."""
    department = catalogue_filter_columns["department"]
    members: dict[str, set[int]] = {}
    for entity, key in department.values.items():
        members.setdefault(key, set()).add(entity)

    for case in cat.catalogue():
        if len(case.entities) < 100:
            continue
        for value in ("alpha", "beta", "gamma"):
            inside = members[value] & case.entities
            assert 0 < len(inside) < len(case.entities), (
                f"department={value} selects {len(inside)} of {case.name}'s "
                f"{len(case.entities)} entities — all or none, so the filter and the mask are "
                "correlated and every cross-principal assertion is vacuous"
            )

    # omega: members exist, and none is visible to any principal but the full-coverage one.
    assert members["omega"], "omega has no members anywhere — it must be hidden, not hollow"
    for case in cat.catalogue():
        if case.name == "full_100pct":
            continue
        assert not members["omega"] & case.entities, (
            f"{case.name} can see an omega member, so no catalogue principal is blind to it and "
            "the hidden-value test has no principal to run as"
        )
    # hollow: declared, planted nowhere. solo: exactly one member, visible to the crossover pair.
    assert "hollow" not in members
    assert members["solo"] == {cat.DEPARTMENT_SOLO_ID}
    assert cat.DEPARTMENT_SOLO_ID in cat.BLOCKS["cross_lo"].entities


# ---------------------------------------------------------------------------------------------
# The served contract for filters
# ---------------------------------------------------------------------------------------------


def test_meta_publishes_the_filter_operands(catalogue_server):
    """`/v1/meta` must say what each column accepts (contracts §3.2), or a client is left
    inferring the operator table — and a conforming independent implementation could not know
    which leaf is a `422` before sending it."""
    token = catalogue_server.authorise([])["token"]
    operands = catalogue_server.meta(token)["filter_operands"]
    by_column = {entry["column"]: entry for entry in operands}

    assert set(by_column) == {"department", "archive", "title"}, (
        f"filter_operands names {sorted(by_column)} — the three declared filter columns, no more "
        "(fx_key is render-only) and no fewer"
    )
    assert by_column["department"]["family"] == "category"
    assert set(by_column["department"]["operands"]) == {"eq", "in"}
    # **The route is invisible on the wire, and that is the assertion.** `archive` is `public` and
    # `department` is `per_viewer`, so decision 0063 answers the first from its derived postings and
    # the second by scanning — and `/v1/meta` publishes the same family and the same operand list
    # for both. A client cannot see which route it will take, and must not be able to: the routing
    # is a property of the deployment's declaration, never of the query surface.
    assert by_column["archive"]["family"] == "category"
    assert set(by_column["archive"]["operands"]) == {"eq", "in"}
    # "string", not "utf8": the *type* is `utf8` and the *family* is string (filter-index §2.6's
    # family table). Contracts §3.2 names the block but not the family spellings, so the design's
    # family vocabulary is the authority this asserts.
    assert by_column["title"]["family"] == "string"
    # `in` is `eq` over a list, which is not a category-only generalisation — a string column
    # takes it too, and the published list is what the parser holds a client to.
    assert set(by_column["title"]["operands"]) == {"eq", "in", "prefix", "contains"}


# ---------------------------------------------------------------------------------------------
# I12, mask half — the differential sweep
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("case", cat.catalogue(), ids=lambda c: c.name)
def test_i12_a_filter_narrows_matched_and_never_touches_visible(
    catalogue_bundle, catalogue_server, catalogue_filter_columns, case
):
    """The sweep: one composed expression, every catalogue principal, exact agreement.

    Three quantities per case, each against a different derivation. `visible` per tile against
    the §7.1 oracle over the case's own entity set (the composed count a filter never changes);
    `matched` per tile against the same oracle over the *oracle's* `M_sel` — brute-force
    evaluation of the expression over fixture-planted values; and the served set, joined through
    `fx_key`, against `M_sel` exactly. θ is saturated and the caps are huge on this server, so
    the served set *is* the filtered set and equality is the strongest available form of
    `M_sel ⊆ M_auth`: a subset assertion alone would also pass a filter that dropped visible
    matching items, which is a defect this suite must distinguish from masking."""
    token = catalogue_server.authorise(list(case.grants))["token"]
    plain = catalogue_server.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    filtered = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=SWEEP_EXPR
    )

    plain_tiles = _tiles_by_id(decode_viewport(plain)[0])
    filt_tiles = _tiles_by_id(decode_viewport(filtered)[0])

    # With no filters, matched == visible per tile (contracts §3.2) — the baseline the filtered
    # request is then compared against.
    for tile, (visible, matched, _served) in plain_tiles.items():
        assert matched == visible, f"unfiltered tile {tile}: matched {matched} != visible {visible}"

    # The frontier direction: the filter moved nothing about `visible` — same tiles, same counts.
    assert set(filt_tiles) == set(plain_tiles), (
        "a filter changed which tiles were reported — `visible` is the composed count and a "
        "filter never changes it, so the tile set (visible > 0) may not move"
    )
    for tile, (visible, matched, served) in filt_tiles.items():
        assert visible == plain_tiles[tile][0], f"tile {tile}: the filter moved `visible`"
        assert matched <= visible, f"tile {tile}: matched {matched} > visible {visible} — I12"
        assert served == matched, (
            f"tile {tile}: θ is saturated and the caps are huge, so every matched row must be "
            f"served; served {served} != matched {matched}"
        )

    # The oracle's side: brute-force M_sel from the fixture's planted values, then §7.1 counts.
    m_auth = set(case.entities)
    m_sel = filt.evaluate(SWEEP_EXPR, catalogue_filter_columns, m_auth)
    assert m_sel <= m_auth  # structural in the oracle; the engine half is the assertions below

    expected_visible = vp.counts(catalogue_bundle, m_auth, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    expected_matched = vp.counts(catalogue_bundle, m_sel, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    assert {t: v for t, (v, _m, _s) in filt_tiles.items()} == expected_visible
    assert {t: m for t, (_v, m, _s) in filt_tiles.items() if m} == expected_matched

    # The served sets: filtered ⊆ unfiltered (I12 on the wire), and filtered == oracle M_sel.
    plain_served = _served_entities(plain)
    filt_served = _served_entities(filtered)
    assert filt_served <= plain_served, (
        "the filtered response served points the unfiltered one did not — a filter widened the "
        "served set, which is I12 inverted"
    )
    assert filt_served == m_sel, (
        f"the engine's filtered served set has {len(filt_served)} entities against the oracle's "
        f"{len(m_sel)} — the two implementations disagree about which items match"
    )


def test_the_empty_combinators_are_their_operators_identities(
    catalogue_server, sweep_cases
):
    """`all_of: []` matches the whole candidate and `any_of: []` matches nothing (contracts
    §3.2) — and in both directions `visible` stands still."""
    case = sweep_cases["crossover_above"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    plain_tiles = _tiles_by_id(
        decode_viewport(catalogue_server.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT))[0]
    )

    everything = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"all_of": []}
    )
    for tile, (visible, matched, _s) in _tiles_by_id(decode_viewport(everything)[0]).items():
        assert (visible, matched) == (plain_tiles[tile][0], plain_tiles[tile][0])

    nothing = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"any_of": []}
    )
    nothing_tiles, nothing_points = decode_viewport(nothing)
    assert {t: v for t, (v, _m, _s) in _tiles_by_id(nothing_tiles).items()} == {
        t: v for t, (v, _m, _s) in plain_tiles.items()
    }, "`any_of: []` moved `visible`"
    assert all(m == 0 and s == 0 for _t, _v, m, s in nothing_tiles)
    assert nothing_points == []


# ---------------------------------------------------------------------------------------------
# C11: hidden, hollow and nonexistent values are one outcome
# ---------------------------------------------------------------------------------------------


def test_a_hidden_value_a_hollow_value_and_a_nonexistent_value_are_one_outcome(
    catalogue_server, sweep_cases
):
    """Five spellings of "matches nothing this principal may know about", one body.

    The principal holds `cross_lo`, so `omega` (real members, all in the ungranted `high_tail`),
    `hollow` (declared, no members anywhere), `"nonesuch"` (no such key), and two code forms
    (omega's pinned 4, and 200 which names nothing) must be **byte-identical** in response:
    status, tiles with `matched = 0`, `visible` exactly the unfiltered counts, no points. A
    weaker canonicalised comparison would leave room for a distinguishing field; bytes leave
    none. (Work-indistinguishability is deliberately not asserted — module doc.)

    The `solo` control is what makes the five empty bodies evidence rather than a broken filter
    agreeing with itself: the same column, the same operator, the same principal, and a value
    with exactly one visible member — which must be found."""
    case = sweep_cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]

    plain_tiles = _tiles_by_id(
        decode_viewport(catalogue_server.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT))[0]
    )

    bodies = {}
    for label, operand in [
        ("hidden key", "omega"),
        ("hidden code", 4),
        ("hollow key", "hollow"),
        ("unknown key", "nonesuch"),
        ("unknown code", 200),
    ]:
        resp = catalogue_server.viewport_request(
            token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"department": {"eq": operand}}
        )
        assert resp.status_code == 200, (
            f"{label}: {resp.status_code} — an unresolvable value is an empty operand, never a "
            "refusal (contracts §3.2; per-point-attributes §3.8)"
        )
        bodies[label] = resp.content

    first_label, first = next(iter(bodies.items()))
    for label, body in bodies.items():
        assert body == first, (
            f"the {label!r} response differs from the {first_label!r} response — the outcomes "
            "are distinguishable, so the filter surface is an existence oracle over what "
            "`listing = \"per_viewer\"` hides (C11)"
        )

    tiles, points = decode_viewport(first)
    assert points == []
    assert {t: v for t, (v, _m, _s) in _tiles_by_id(tiles).items()} == {
        t: v for t, (v, _m, _s) in plain_tiles.items()
    }, "an empty-operand request moved `visible`"
    assert all(m == 0 for _t, _v, m, _s in tiles)

    # The positive control: one visible member, found.
    solo = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"department": {"eq": "solo"}}
    )
    assert _served_entities(solo) == {cat.DEPARTMENT_SOLO_ID}, (
        "the solo control failed — a single-member value was not served, so the five empty "
        "bodies above may just be a filter that matches nothing"
    )


# ---------------------------------------------------------------------------------------------
# The postings route (decision 0063) — the same sets by a different construction
# ---------------------------------------------------------------------------------------------


ROUTED_EXPRS = [
    ("eq by key", {"archive": {"eq": "red"}}),
    ("eq by code", {"archive": {"eq": 22}}),
    ("in, key and code mixed", {"archive": {"in": ["red", 33]}}),
    ("in over the whole vocabulary", {"archive": {"in": ["red", "green", "blue", "void"]}}),
    # A declared value planted nowhere. On the routed side this is a keyed postings file with no
    # record for the code, which must read as the empty set and never as an error — and never as
    # the *whole* set, which is what a reader that treated a missing record as "unconstrained"
    # would produce.
    ("a declared value with no members", {"archive": {"eq": "void"}}),
]


@pytest.mark.parametrize("name,expr", ROUTED_EXPRS, ids=[n for n, _ in ROUTED_EXPRS])
@pytest.mark.parametrize(
    "case_name", ["full_100pct", "crossover_above", "sparse_0_01pct"]
)
def test_a_public_category_answers_exactly_what_the_definition_says(
    catalogue_bundle, catalogue_server, catalogue_filter_columns, sweep_cases, case_name, name, expr
):
    """**The routed differential.** `archive` is `listing = "public"`, so decision 0063 answers its
    `eq` and `in` from the column's derived per-value postings — a corpus-wide set intersected with
    the candidate — where `department` is answered by scanning the candidate's values. The oracle
    has one evaluation for both, so agreement here is agreement between two constructions rather
    than a transcription.

    Run over three principals of different coverage, because the two routes differ in *where* the
    mask enters: the scan takes the candidate as its input, and the postings meet it afterwards. A
    routed answer that forgot the intersection agrees with the definition for the full-coverage
    principal and disagrees for every other, so a single-principal test would miss exactly the
    defect the route can have.
    """
    case = sweep_cases[case_name]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)
    m_sel = filt.evaluate(expr, catalogue_filter_columns, m_auth)

    raw = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
    )
    assert _served_entities(raw) == m_sel, f"{case_name} / {name}"

    expected_matched = vp.counts(catalogue_bundle, m_sel, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    tiles = _tiles_by_id(decode_viewport(raw)[0])
    assert {t: m for t, (_v, m, _s) in tiles.items() if m} == expected_matched, (
        f"{case_name} / {name}: the per-tile matched counts disagree with the definition"
    )


def test_the_two_routes_compose_with_each_other(
    catalogue_bundle, catalogue_server, catalogue_filter_columns, sweep_cases
):
    """A `public` leaf and a `per_viewer` leaf in one expression. The two are evaluated by
    different constructions and must still intersect and union as sets — a routed leaf that
    returned a set outside the candidate would show up here first, since the conjunction's later
    leaf is evaluated under the earlier one's result."""
    case = sweep_cases["crossover_above"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)

    for expr in (
        {"all_of": [{"archive": {"eq": "red"}}, {"department": {"in": ["alpha", "beta"]}}]},
        {"any_of": [{"archive": {"eq": "void"}}, {"department": {"eq": "solo"}}]},
        {"all_of": [{"archive": {"in": ["red", "green"]}}, {"title": {"prefix": "smi"}}]},
    ):
        m_sel = filt.evaluate(expr, catalogue_filter_columns, m_auth)
        raw = catalogue_server.viewport(
            token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
        )
        assert _served_entities(raw) == m_sel, expr
        assert m_sel <= m_auth, "I12: a filter may not widen the mask"


# ---------------------------------------------------------------------------------------------
# Composition against brute force
# ---------------------------------------------------------------------------------------------


NESTED_EXPR = {
    "all_of": [
        {"department": {"in": ["alpha", "beta"]}},
        {"any_of": [{"title": {"contains": "myth"}}, {"title": {"prefix": "smithy"}}]},
    ]
}

# The same set by distribution: A ∧ (B ∨ C) = (A ∧ B) ∨ (A ∧ C). Two spellings, one answer —
# and since the served set is a pure function of `(mask, corpus state, k, viewport)` (§10.4),
# one *response*.
DISTRIBUTED_EXPR = {
    "any_of": [
        {"all_of": [{"department": {"in": ["alpha", "beta"]}}, {"title": {"contains": "myth"}}]},
        {"all_of": [{"department": {"in": ["alpha", "beta"]}}, {"title": {"prefix": "smithy"}}]},
    ]
}


def test_a_nested_tree_agrees_with_brute_force_and_its_algebra(
    catalogue_bundle, catalogue_server, catalogue_filter_columns, sweep_cases
):
    case = sweep_cases["crossover_above"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)

    m_sel = filt.evaluate(NESTED_EXPR, catalogue_filter_columns, m_auth)
    assert m_sel, "the nested expression selects nothing — the case tests an empty corner"
    assert m_sel < m_auth, "the nested expression selects everything — composition is untested"
    assert m_sel == filt.evaluate(DISTRIBUTED_EXPR, catalogue_filter_columns, m_auth)

    nested = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=NESTED_EXPR
    )
    assert _served_entities(nested) == m_sel
    expected_matched = vp.counts(catalogue_bundle, m_sel, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
    tiles = _tiles_by_id(decode_viewport(nested)[0])
    assert {t: m for t, (_v, m, _s) in tiles.items() if m} == expected_matched

    distributed = catalogue_server.viewport(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=DISTRIBUTED_EXPR
    )
    assert decode_viewport(distributed) == decode_viewport(nested), (
        "two algebraically equal spellings served different responses — evaluation depends on "
        "the tree's shape rather than its meaning"
    )


# ---------------------------------------------------------------------------------------------
# Refusals
# ---------------------------------------------------------------------------------------------


def test_an_unknown_column_refuses_and_an_unknown_value_does_not(
    catalogue_server, sweep_cases
):
    """Contracts §3.2's one distinction, both halves. The column table is deployment schema —
    published in `/v1/meta`, identical for every principal — so naming a column that is not in
    it is a shape error (`422`, must not be retried unchanged). A *value* is data, and refusing
    one would disclose which values exist."""
    case = sweep_cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]

    unknown_column = catalogue_server.viewport_request(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"nonesuch": {"eq": "x"}}
    )
    assert unknown_column.status_code == 422, unknown_column.text
    assert unknown_column.json()["error"] == "contract"

    unknown_value = catalogue_server.viewport_request(
        token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters={"department": {"eq": "nonesuch"}}
    )
    assert unknown_value.status_code == 200, (
        "an unknown value must be an empty operand, never a refusal — refusing it makes the "
        "filter an existence oracle over the gated vocabulary"
    )


def test_the_unbuilt_operators_refuse_by_name(catalogue_server, sweep_cases):
    """Decision 0062 and decision 0013: `none_of` and `match` are specified and unbuilt, so
    naming either is a `422` — never an ignored clause, which would answer a different question
    while looking like an answer to this one."""
    case = sweep_cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]

    for label, filters in [
        ("none_of", {"none_of": [{"department": {"eq": "alpha"}}]}),
        ("match on utf8", {"title": {"match": "smith"}}),
    ]:
        resp = catalogue_server.viewport_request(
            token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=filters
        )
        assert resp.status_code == 422, f"{label}: {resp.status_code} {resp.text}"
        assert resp.json()["error"] == "contract", label


def test_an_operator_outside_the_columns_family_refuses(catalogue_server, sweep_cases):
    """An operator outside the column's family is a **shape error**, not an empty operand.

    The distinction is which side of the trust boundary the fact lives on. A column's family is
    deployment schema — `/v1/meta` publishes it "so a client need not infer it" — and is identical
    for every principal, so refusing discloses nothing. A *value*'s existence is viewer data, and
    refusing that would be an existence oracle over exactly the vocabulary `per_viewer` hides. So
    the two get opposite treatments on purpose, and this test pins the family half.

    Recorded as a divergence when this suite was first written, and ruled the other way: the
    server answered `200` with an empty operand, which is fail-closed and discloses nothing but
    turns a client typo into a silent "no matches" and contradicts the refusal `match` already
    gets one operator over.

    `in` on a `utf8` column is **no longer an example** of this: `in` is `eq` over a list, which is
    not a category-only generalisation, so a string column takes it and the divergence report's
    second case was itself the bug."""
    case = sweep_cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]

    for label, filters in [
        ("prefix on a category", {"department": {"prefix": "al"}}),
        ("contains on a category", {"department": {"contains": "lph"}}),
    ]:
        resp = catalogue_server.viewport_request(
            token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=filters
        )
        assert resp.status_code == 422, f"{label}: {resp.status_code}"


# ---------------------------------------------------------------------------------------------
# I3: nothing to test, pinned rather than skipped
# ---------------------------------------------------------------------------------------------


def test_i3_has_no_surface_to_test(catalogue_server, sweep_cases):
    """I3 (labels gate on `M_auth`, never `M_sel`) and I12's frontier half stay uncovered:
    there is no label service, no `/v1/labels` route, no generating sets and no frontier
    (`conformance.md` §4.6; surface §9 says a test row for that machinery would be decision
    0013's error inside a conformance suite).

    This test pins the *absence* instead, so the gap cannot rot silently: the day a labels
    route answers anything but 404/405, this fails, and whoever lands it owes the I3 half of
    this differential — a filtered request whose label set equals the unfiltered request's."""
    import requests  # noqa: PLC0415 — a driver concern, used only to probe for the route

    case = sweep_cases["crossover_below"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    resp = requests.post(
        f"{catalogue_server.viewer_base}/v1/labels",
        headers={"Authorization": f"Bearer {token}"},
        json={"slice": cat.SLICE_ID, "zoom": ZOOM, "bbox": list(cat.FULL_VIEWPORT)},
        timeout=10,
    )
    assert resp.status_code in (404, 405), (
        f"/v1/labels answered {resp.status_code} — a label surface exists, so I3 and I12's "
        "frontier half are no longer 'nothing to test' and this module owes their coverage"
    )
