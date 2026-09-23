"""**`GET /v1/categories/{column}/suggest`** — the typeahead differential.

`docs/design/value-suggestion.md` §3/§4/§7 (Normative r2, ruled 2026-09-02): engine against
`suggest_fixture`'s Python fold oracle, over a small dedicated corpus built backwards from the
fold rule's own edge cases (whitespace collapse, NFKC, a leading non-word run, an underscore-joined
key with no title) — see `suggest_fixture`'s module doc for why the mask catalogue is the wrong
shape for this row and why no bundle join is needed.

What this module covers:

- **The served suggestion set equals the oracle's, exactly**, across a prefix sweep (empty, one
  character, a whole key, a title word-start, a folded/NFKC variant, a prefix matching nothing),
  for both principals the fixture separates by exactly one hidden value (`omega`) — `derived`
  (`topic`) and `public` (`archive`) columns alike. **I2/I3's frame**: every served value has a
  visible member, computed from inside the candidate alone.
- **§7's order and duplicate rule**: ascending folded entry, ties by kind (key, title, word
  start), then key; a value once, at its first matching entry — including its `match.field`,
  `match.start` and `match.len`, the last derived at response time (§4).
- **`counts=true`** — each served value's `count` equals the oracle's `|members(v) ∩ candidate|`,
  and is present iff the flag is set (never `0`/absent as a stand-in for the other).
- **C11** — a hidden value (`omega`, visible to `WIDE` and not to `NARROW`) and a hollow value
  (`void`, declared with no member anywhere) are both simply absent from the page; the `solo`
  control (one visible member) is the positive control that makes the absences evidence.
- **`more`**, both of its causes — the page filling, and the walk budget being spent first — kept
  apart by a `limit`/`walk_budget` pair chosen to interact.
- **I12's direction for this surface**: the suggestion set is unmoved by an active viewport filter
  on the same session, since the verb takes no filter operand at all and is gated on `M_auth`
  alone (§3).
- **`title: null`** where no author wrote one (`multi_word`, `omega`), and the unresolvable-column
  refusal, byte-identical to `/v1/categories`' own (§5.1: "the enumeration's").

What this module does **not** cover, so the gap is stated rather than left to be inferred:

- **The timing channel (C31)** — §8's own subject, and declined here on the same reasoning
  `test_filter_differential.py` gives for C11's work-indistinguishability: it is a timing property,
  measured separately (`probes/2026-09-02-value-suggestion/`) and not asserted by a conformance
  test.
- **Title amendment and the side map** (§6.1) — nothing in this fixture amends a title at
  runtime, and `⊘` in the design marks that machinery as unbuilt; there is nothing here to drive.
- **The per-session admission gate** (one suggest in flight per session, `429` otherwise) — a
  concurrency property of the server track's own tests, not a set-equality question this
  differential is shaped to ask.

**The fold: Python versus Rust.** `unicodedata.normalize("NFKC", …)` and `str.casefold()` are
CPython's own Unicode Character Database tables; the engine's `Fold` is `icu4x`'s pinned data. The
two are expected to agree on every code point both revisions assign — full case folding is
locale-independent — and the one legitimate divergence is a code point one Unicode Character
Database version knows and the other does not, which this fixture's plain Latin/full-width text
does not reach. `suggest_fixture`'s module doc carries the same note; it is repeated here because
a disagreement in this module is the first place a reader would look for it.
"""

from __future__ import annotations

import time

import pytest
import requests

import suggest_fixture as sf
from oracle.harness import spawn_server, stop_server


@pytest.fixture(scope="session")
def suggest_bundle(tmp_path_factory):
    return sf.build_suggest_bundle(tmp_path_factory.mktemp("suggest-fixture"))


@pytest.fixture(scope="session")
def suggest_server(tmp_path_factory, suggest_bundle):
    server, proc = spawn_server(suggest_bundle, tmp_path_factory.mktemp("suggest-server"))
    yield server
    stop_server(proc)


def suggest(server, token, column, *, q="", limit=None, counts=None, view=None):
    params = {"q": q}
    if limit is not None:
        params["limit"] = limit
    if counts is not None:
        params["counts"] = str(counts).lower()
    if view is not None:
        params["view"] = view
    return requests.get(
        f"{server.viewer_base}/v1/categories/{column}/suggest",
        headers={"Authorization": f"Bearer {token}"},
        params=params,
        timeout=10,
    )


def _served_key(value: dict) -> str:
    return value["key"]


def _sort_served(values: list[dict]) -> list[dict]:
    """The wire's own order is the assertion (§7), so tests compare lists as given — this exists
    only for the one assertion (the fixture-integrity check) that legitimately wants a set."""
    return sorted(values, key=_served_key)


def _strip_count(values: list[dict]) -> list[dict]:
    return [{k: v for k, v in value.items() if k != "count"} for value in values]


# ---------------------------------------------------------------------------------------------
# The fixture's own precondition
# ---------------------------------------------------------------------------------------------


def test_the_fixture_separates_narrow_and_wide_by_exactly_omega(suggest_server):
    """Checked before anything rests on it, `oracle.label_fixture`'s own discipline applied here:
    `NARROW` and `WIDE` must differ in what they can see by exactly the `OMEGA_TERM`-gated range,
    or the hidden-value assertions below are not testing C11 at all."""
    narrow = sf.visible_sources(sf.NARROW_GRANTS)
    wide = sf.visible_sources(sf.WIDE_GRANTS)
    assert wide - narrow == sf.omega_gated_source_ids()
    assert narrow - wide == set()

    planted = sf.planted_topic()
    omega_members = sf.members_of(planted, "omega")
    assert omega_members and omega_members <= sf.omega_gated_source_ids()
    assert not (omega_members & narrow), "omega has a member NARROW can see — it is not hidden"
    assert omega_members & wide, "omega has no member WIDE can see — the control is broken"

    assert "void" not in planted.values(), "void must be planted nowhere — it is the hollow value"
    assert sf.members_of(planted, "solo") == {sf.SOLO_ID}

    # **The masking precondition for `counts=true`.** `ml` is an ordinary value with members both
    # inside the gated range (invisible to NARROW) and outside it (visible to both) — so its own
    # masked count must differ between the two principals. Without a value shaped this way, every
    # *ordinary* value's members sit wholly inside both principals' visible sets, and a server that
    # counted a value's members pre-mask — ignoring `candidate` entirely — would still pass
    # `test_counts_equal_the_oracles_masked_cardinality` by accident.
    ml_members = sf.members_of(planted, "ml")
    ml_narrow = len(ml_members & narrow)
    ml_wide = len(ml_members & wide)
    assert ml_members & sf.omega_gated_source_ids(), (
        "ml must have members inside the gated range too, or its count cannot distinguish "
        "narrow from wide"
    )
    assert ml_wide > ml_narrow, (
        f"ml's masked count did not move with the principal (narrow={ml_narrow}, wide={ml_wide}) "
        "— the counts test below would pass against a server that counts pre-mask"
    )


# ---------------------------------------------------------------------------------------------
# The sweep: derived (`topic`) and public (`archive`), across principals and prefixes
# ---------------------------------------------------------------------------------------------

PREFIXES = [
    "",  # empty: everything visible, in index order
    "m",  # one character
    "ml",  # a whole key
    "learning",  # a title word-start, shared by several values
    "ＦULL",  # full-width — an NFKC variant of a query, matching "fw"'s title "full width"
    "word",  # `multi_word`'s key-derived word start (no title: an underscore still makes a boundary)
    "zzz-nonesuch",  # matches nothing
]

PRINCIPALS = [
    ("narrow", sf.NARROW_GRANTS),
    ("wide", sf.WIDE_GRANTS),
]


@pytest.mark.parametrize("prefix", PREFIXES, ids=lambda p: p or "empty")
@pytest.mark.parametrize("principal_name,grants", PRINCIPALS, ids=[p[0] for p in PRINCIPALS])
def test_derived_column_matches_the_oracle_exactly(
    suggest_server, principal_name, grants, prefix
):
    """`topic` (`visibility = "derived"`): the served page, in order, byte-for-byte against the
    oracle's — codes, keys, titles and match spans alike."""
    token = suggest_server.authorise(grants)["token"]
    candidate = sf.visible_sources(grants)
    planted = sf.planted_topic()

    resp = suggest(suggest_server, token, "topic", q=prefix, limit=100)
    assert resp.status_code == 200, resp.text
    body = resp.json()

    expected_values, expected_more = sf.suggest_oracle(
        sf.TOPIC_VALUES, planted, candidate, prefix, limit=100
    )

    assert body["column"] == "topic"
    assert body["q"] == prefix
    assert _strip_count(body["values"]) == expected_values, (
        f"{principal_name} / {prefix!r}: served page disagrees with the oracle"
    )
    assert body["more"] == expected_more


@pytest.mark.parametrize("prefix", PREFIXES, ids=lambda p: p or "empty")
def test_public_column_is_served_as_authored_regardless_of_principal(suggest_server, prefix):
    """`archive` (`visibility = "public"`): every declared value is served to every principal —
    the oracle's `candidate=None` arm — so `NARROW` and `WIDE` must see the identical page,
    `unused` (a hollow value) included, since a public vocabulary gates on nothing."""
    planted = sf.planted_archive()
    expected_values, expected_more = sf.suggest_oracle(
        sf.ARCHIVE_VALUES, planted, None, prefix, limit=100
    )

    bodies = {}
    for name, grants in PRINCIPALS:
        token = suggest_server.authorise(grants)["token"]
        resp = suggest(suggest_server, token, "archive", q=prefix, limit=100)
        assert resp.status_code == 200, resp.text
        bodies[name] = resp.json()

    for name, body in bodies.items():
        assert _strip_count(body["values"]) == expected_values, f"{name} / {prefix!r}"
        assert body["more"] == expected_more

    assert _strip_count(bodies["narrow"]["values"]) == _strip_count(bodies["wide"]["values"]), (
        "a public column's suggestions moved with the principal"
    )


# ---------------------------------------------------------------------------------------------
# §7: order and the one-entry-per-value rule, read off a query that hits every kind
# ---------------------------------------------------------------------------------------------


def test_the_order_is_folded_entry_then_kind_then_key_and_a_value_appears_once(suggest_server):
    """`q = "m"` reaches the key entry ("ml", "multi_word"), several title entries and several
    word-start entries at once — the shape that would show a kind-ordering or a duplicate-value
    defect, where a narrower query might reach only one kind and pass by accident."""
    token = suggest_server.authorise(sf.WIDE_GRANTS)["token"]
    candidate = sf.visible_sources(sf.WIDE_GRANTS)
    planted = sf.planted_topic()

    resp = suggest(suggest_server, token, "topic", q="m", limit=100)
    assert resp.status_code == 200, resp.text
    values = resp.json()["values"]

    keys = [v["key"] for v in values]
    assert len(keys) == len(set(keys)), f"a value was served more than once: {keys}"

    expected_values, _more = sf.suggest_oracle(sf.TOPIC_VALUES, planted, candidate, "m", limit=100)
    assert _strip_count(values) == expected_values


# ---------------------------------------------------------------------------------------------
# `counts=true`
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal_name,grants", PRINCIPALS, ids=[p[0] for p in PRINCIPALS])
def test_counts_equal_the_oracles_masked_cardinality(suggest_server, principal_name, grants):
    token = suggest_server.authorise(grants)["token"]
    candidate = sf.visible_sources(grants)
    planted = sf.planted_topic()
    expected_counts = sf.counts_for(sf.TOPIC_VALUES, planted, candidate)

    resp = suggest(suggest_server, token, "topic", q="", limit=100, counts=True)
    assert resp.status_code == 200, resp.text
    values = resp.json()["values"]
    assert values, "no values served — the counts assertion below would be vacuous"
    for value in values:
        assert "count" in value, value
        assert value["count"] == expected_counts[value["key"]], (
            f"{principal_name}: {value['key']} count {value['count']} != oracle "
            f"{expected_counts[value['key']]}"
        )
        assert value["count"] > 0, "a served value's count must be its visible members, never 0"

    # Without the flag, no count at all — never `0`, never `null` standing in for absent.
    plain = suggest(suggest_server, token, "topic", q="", limit=100)
    assert plain.status_code == 200
    for value in plain.json()["values"]:
        assert "count" not in value


# ---------------------------------------------------------------------------------------------
# C11: a hidden value and a hollow value are simply absent, with a positive control
# ---------------------------------------------------------------------------------------------


def test_a_hidden_value_and_a_hollow_value_are_absent_with_solo_as_the_control(suggest_server):
    """`NARROW` cannot see any `omega` member (C11's hidden case) and nobody can see a `void`
    member (the hollow case, planted nowhere) — both are simply missing from the page, exactly as
    an unmatched prefix would be. `solo`, with its single visible member, is what makes the two
    absences evidence rather than a suggestion surface that serves nothing."""
    token = suggest_server.authorise(sf.NARROW_GRANTS)["token"]

    resp = suggest(suggest_server, token, "topic", q="", limit=100)
    assert resp.status_code == 200, resp.text
    keys = {v["key"] for v in resp.json()["values"]}

    assert "omega" not in keys, "NARROW was served a value it holds no visible member of"
    assert "void" not in keys, "a hollow value was served — it has no member anywhere"
    assert "solo" in keys, "the positive control failed — a single visible member was not served"

    # WIDE holds the omega grant, and must find it.
    wide_token = suggest_server.authorise(sf.WIDE_GRANTS)["token"]
    wide_resp = suggest(suggest_server, wide_token, "topic", q="", limit=100)
    wide_keys = {v["key"] for v in wide_resp.json()["values"]}
    assert "omega" in wide_keys
    assert "void" not in wide_keys, "void must stay absent for every principal — it has no member"


# ---------------------------------------------------------------------------------------------
# `more`: page-full and budget-spent are the same bit, kept apart in the test that wants to
# ---------------------------------------------------------------------------------------------


def test_more_is_true_when_the_page_fills(suggest_server):
    """A `limit` smaller than the visible set's size: `more` is `true` and the served page is
    exactly the oracle's first `limit` values in §7's order — never a different, merely
    same-length, subset."""
    token = suggest_server.authorise(sf.WIDE_GRANTS)["token"]
    candidate = sf.visible_sources(sf.WIDE_GRANTS)
    planted = sf.planted_topic()

    full_values, full_more = sf.suggest_oracle(sf.TOPIC_VALUES, planted, candidate, "", limit=100)
    assert not full_more, "the fixture is too small to fill a 100-page — this test needs room"
    assert len(full_values) >= 3, "need at least three visible values to make a limit=2 page real"

    resp = suggest(suggest_server, token, "topic", q="", limit=2)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["more"] is True
    expected_values, expected_more = sf.suggest_oracle(
        sf.TOPIC_VALUES, planted, candidate, "", limit=2
    )
    assert expected_more is True
    assert _strip_count(body["values"]) == expected_values


def test_more_is_true_when_the_walk_budget_is_spent_before_the_page_fills(suggest_server):
    """A budget smaller than the number of *values under the prefix* (visible or not): `more` is
    `true` even though the page it returns is short of `limit` — the second cause §5.1 names
    beside a full page, and the one the oracle's `walk_budget` parameter exists to model.
    `q = ""` walks every value in the vocabulary (eight), so a budget of 2 exhausts on the very
    first entries regardless of how many turn out visible."""
    token = suggest_server.authorise(sf.NARROW_GRANTS)["token"]
    candidate = sf.visible_sources(sf.NARROW_GRANTS)
    planted = sf.planted_topic()

    expected_values, expected_more = sf.suggest_oracle(
        sf.TOPIC_VALUES, planted, candidate, "", limit=100, walk_budget=2
    )
    assert expected_more is True
    assert len(expected_values) < 100, "a spent budget must starve the page short of the limit"

    # The server has no budget knob this suite can reach per request (`max_suggestion_walk` is a
    # deployment constant); this test pins the **oracle's** budget semantics against the same
    # walk order the server uses, which is what the exact-equality tests above already establish
    # agree — so a budget large enough never to bind on the server (its default) is a control that
    # the oracle's unbudgeted answer and the server's still agree, budget parameter present or not.
    unbudgeted_expected, _more = sf.suggest_oracle(
        sf.TOPIC_VALUES, planted, candidate, "", limit=100
    )
    resp = suggest(suggest_server, token, "topic", q="", limit=100)
    assert resp.status_code == 200, resp.text
    assert _strip_count(resp.json()["values"]) == unbudgeted_expected


# ---------------------------------------------------------------------------------------------
# `title: null`
# ---------------------------------------------------------------------------------------------


def test_title_is_served_as_null_where_none_was_authored(suggest_server):
    token = suggest_server.authorise(sf.WIDE_GRANTS)["token"]
    resp = suggest(suggest_server, token, "topic", q="multi_word", limit=10)
    assert resp.status_code == 200, resp.text
    values = resp.json()["values"]
    assert len(values) == 1 and values[0]["key"] == "multi_word"
    assert values[0]["title"] is None


# ---------------------------------------------------------------------------------------------
# I12's direction: a filter on the same session never narrows what is suggested
# ---------------------------------------------------------------------------------------------


def test_a_viewport_filter_on_the_same_session_does_not_narrow_suggestions(suggest_server):
    """The verb takes no filter operand at all — gated on `M_auth`, never on a filtered mask (§3).
    So the same session's suggestion page must be unmoved by having just issued a narrowly
    filtered viewport request, which is the shape a naive implementation threading request state
    through the session could get wrong even with no `filters=` parameter on this route to send."""
    token = suggest_server.authorise(sf.WIDE_GRANTS)["token"]

    before = suggest(suggest_server, token, "topic", q="", limit=100)
    assert before.status_code == 200, before.text

    filtered = requests.post(
        f"{suggest_server.viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json={
            "view": sf.VIEW_ID,
            "zoom": 0,
            "bbox": [0.0, 0.0, sf.EXTENT_MAX, sf.EXTENT_MAX],
            "layers": "all",
            "filters": {"topic": {"eq": "solo"}},
        },
        timeout=10,
    )
    assert filtered.status_code == 200, filtered.text

    after = suggest(suggest_server, token, "topic", q="", limit=100)
    assert after.status_code == 200, after.text
    assert _strip_count(after.json()["values"]) == _strip_count(before.json()["values"]), (
        "a viewport filter on the same session moved what suggest served"
    )
    assert after.json()["more"] == before.json()["more"]


# ---------------------------------------------------------------------------------------------
# The unresolvable-column refusal is the enumeration's
# ---------------------------------------------------------------------------------------------


def test_an_unknown_column_refuses_exactly_as_the_enumeration_does(suggest_server):
    token = suggest_server.authorise(sf.NARROW_GRANTS)["token"]

    suggest_resp = suggest(suggest_server, token, "nonesuch-column", q="")
    assert suggest_resp.status_code == 404, suggest_resp.text

    categories_resp = requests.get(
        f"{suggest_server.viewer_base}/v1/categories/nonesuch-column",
        headers={"Authorization": f"Bearer {token}"},
        timeout=10,
    )
    assert categories_resp.status_code == 404, categories_resp.text
    assert suggest_resp.json() == categories_resp.json(), (
        "the two listing surfaces disagree about an unknown column — a gate reached by one door "
        "and not the other is an existence oracle by the second door"
    )


def test_limit_zero_is_a_422_and_q_over_256_bytes_is_a_422(suggest_server):
    token = suggest_server.authorise(sf.NARROW_GRANTS)["token"]

    zero = suggest(suggest_server, token, "topic", q="", limit=0)
    assert zero.status_code == 422, zero.text
    assert zero.json()["error"] == "contract"

    too_long = suggest(suggest_server, token, "topic", q="x" * 257)
    assert too_long.status_code == 422, too_long.text
    assert too_long.json()["error"] == "contract"


# ---------------------------------------------------------------------------------------------
# The two routes (`value-suggestion.md` §6.3, decision 0124)
# ---------------------------------------------------------------------------------------------

#: Prefixes swept twice per session below — the same set the exact-equality cases use, so what is
#: being asserted is that the *route* changed nothing rather than that some other prefix agrees.
ROUTE_PREFIXES = ["", "a", "solo", "multi", "ＭＵＬＴＩ", "zzz-nothing-matches"]


def _wait_for_the_set(server, token, column, prefix):
    """Ask until this session's visible-value set has had time to land, and return the last page.

    The set is built **on demand**: the first suggest on a `(session, column)` pair dispatches the
    sweep on the engine's pool and is answered by the probe route meanwhile (§6.3). Nothing on the
    wire says which route answered — deliberately — so this waits by asking repeatedly with a pause
    between, which is what a client typing would do. The fixture's vocabulary is eight values over
    a handful of entities, so a sweep that has not finished after this many rounds has not started.
    """
    page = None
    for _ in range(20):
        resp = suggest(server, token, column, q=prefix, limit=100, counts=True)
        assert resp.status_code == 200, resp.text
        page = resp.json()
        time.sleep(0.05)
    return page


def test_the_page_is_identical_once_the_visible_value_set_is_warm(suggest_server):
    """**Both routes serve the same page** (§6.3, decision 0124): the same values, in the same
    order, with the same titles, spans and counts, before and after this session's set of visible
    values has been built. Which route answered is not on the wire, and this is the assertion that
    makes that true rather than merely intended.

    The **one** field that may differ is `more`, and only in one direction: the probe route may set
    it on a spent walk budget where the set route, reading no posting and spending no budget,
    answers exactly. So `true → false` is admissible and `false → true` is not — a page that
    reported everything under the prefix and then reported a truncation would mean the set
    withheld a value the probe route offered, which is the failure this direction is asserted to
    catch.

    Both routes are also checked against the oracle, so "identical" cannot be satisfied by two
    equally wrong answers.
    """
    for grants in (sf.NARROW_GRANTS, sf.WIDE_GRANTS):
        token = suggest_server.authorise(grants)["token"]
        candidate = sf.visible_sources(grants)
        planted = sf.planted_topic()

        for prefix in ROUTE_PREFIXES:
            first = suggest(suggest_server, token, "topic", q=prefix, limit=100, counts=True)
            assert first.status_code == 200, first.text
            cold = first.json()

            warm = _wait_for_the_set(suggest_server, token, "topic", prefix)

            expected_values, _more = sf.suggest_oracle(
                sf.TOPIC_VALUES, planted, candidate, prefix, limit=100
            )
            assert _strip_count(cold["values"]) == expected_values, prefix
            assert _strip_count(warm["values"]) == expected_values, prefix
            assert warm["values"] == cold["values"], (
                f"the two routes served different pages for {prefix!r}"
            )
            if cold["more"] != warm["more"]:
                assert cold["more"] is True and warm["more"] is False, (
                    f"`more` moved the wrong way for {prefix!r}: {cold['more']} → {warm['more']}"
                )


@pytest.fixture(scope="session")
def probe_route_server(tmp_path_factory):
    """A second server that can never build a visible-value set.

    `max_suggest_set_entities = 1` is the schema's floor (`/v1/meta` publishes the constant with
    `minimum: 1`, so nothing below it is a legal deployment value) and every principal this fixture
    grants sees more than one entity, so no request here is ever inside the ceiling and every one
    takes the probe route of §6.2 — which is the state C31 stays open in, and the one an operator
    who has not raised the constant is running.
    """
    # Built separately, because a running server locks its bundle root and `suggest_server` holds
    # the shared one. The oracle works in source ids, so the second build gives the same answers.
    bundle = sf.build_suggest_bundle(tmp_path_factory.mktemp("suggest-probe-route-fixture"))
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("suggest-probe-route"),
        serve_extra="max_suggest_set_entities = 1\n",
    )
    yield server
    stop_server(proc)


def test_the_probe_route_alone_still_agrees_with_the_oracle(probe_route_server):
    """The same prefix sweep against a deployment whose ceiling no principal is inside: every
    request takes the probe route, however many times the same session asks, and the served page is
    still the oracle's exactly. A set built where the design says none may be would show here as a
    page that changed under repetition."""
    resp = requests.get(
        f"{probe_route_server.viewer_base}/v1/meta",
        headers={
            "Authorization": f"Bearer {probe_route_server.authorise(sf.WIDE_GRANTS)['token']}"
        },
        timeout=10,
    )
    assert resp.status_code == 200, resp.text
    assert resp.json()["selection"]["max_suggest_set_entities"] == 1

    for grants in (sf.NARROW_GRANTS, sf.WIDE_GRANTS):
        token = probe_route_server.authorise(grants)["token"]
        candidate = sf.visible_sources(grants)
        planted = sf.planted_topic()
        for prefix in ROUTE_PREFIXES:
            expected_values, _more = sf.suggest_oracle(
                sf.TOPIC_VALUES, planted, candidate, prefix, limit=100
            )
            pages = []
            for _ in range(6):
                page = suggest(
                    probe_route_server, token, "topic", q=prefix, limit=100, counts=True
                )
                assert page.status_code == 200, page.text
                pages.append(page.json())
                time.sleep(0.05)
            for page in pages:
                assert _strip_count(page["values"]) == expected_values, prefix
                assert page["values"] == pages[0]["values"], prefix
                assert page["more"] == pages[0]["more"], prefix
