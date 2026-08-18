"""The text family's differential over the base build — records §4.4, §4.5, §10.

`abstract` is the catalogue's `text` column. The engine stores a per-layer sorted **token**
dictionary and one posting per term, answers `match` by intersecting the query tokens' postings
inside the candidate, and answers `phrase` by narrowing that conjunction and then re-reading each
survivor's prose out of the record blob to check adjacency. The oracle (`oracle.text.TextColumn`)
holds the prose the fixture planted and walks it, with no dictionary, no posting and no ordinal
anywhere in it.

**Both sides tokenise through one analyser, and that is the design decision this module rests on.**
The oracle reaches `tessera tokenise` — the verb decision 0070 put there for exactly this — rather
than reimplementing UAX #29 segmentation in Python, which would compare PyICU's ICU4C against the
engine's icu4x and turn every marginal disagreement into a research question. What is differential
here is the *set and sequence arithmetic over one token stream*: the engine's through a
dictionary, postings and a blob read, the oracle's through a list walk.

The catalogue entries records §10 gives this family, and where each is covered:

| entry | here |
|---|---|
| a word exactly one entity carries | `test_the_text_column_has_the_shapes_its_catalogue_entries_need`, and the matrix |
| a word no dictionary holds | `test_a_word_no_document_carries_answers_and_answers_empty` |
| hidden versus absent are identical **in the answer** | same test |
| a suppressed entity's terms are still in the postings | `test_a_suppressed_entity_is_not_served_though_its_terms_are_in_the_postings` |
| non-Latin prose under the real segmenter | the golden vectors, and the matrix's CJK/Thai expressions |
| m-of-n at every m | the matrix |
| a phrase whose words are present but not adjacent | the matrix |
| ⊘ field-scoped semantics over multi values | **not reachable** — `multi = true` is refused at the schema (records §5, #87) |

What this module deliberately does not assert: **work**. Appendix C's C25 accepts that a token's
existence and coarse carrier count are observable in service time, and
`probes/2026-08-14-hidden-vs-absent/` measures it; a conformance test asserting the timing were
equal would fail the design as ruled. The *answer* half is asserted below — hidden and absent are
byte-identical — and the timing half is a probe's job.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat
from oracle import text as txt
from oracle.harness import CLI_BIN
from oracle.wire import FRAME_TRAILER, decode_viewport, decode_viewport_points, split_frames

ZOOM = 3

# One expression per shape the family owns. Each is evaluated by the engine against the artefact
# and by the oracle against the fixture's own generation function, and the two must name the same
# entities exactly.
#
# The bodies are `(label, filter, query, minimum, phrase)` — the last three being what the oracle
# needs to answer the same question, kept beside the wire form so the two cannot drift.
BASE_EXPRESSIONS: list[tuple[str, dict, str, int | None, bool]] = [
    # The shared head, which nearly every document carries: a conjunction over three common words,
    # where a route taking the first token's posting instead of the intersection would pass.
    ("match on three words nearly every document has", {"abstract": {"match": "the archive holds"}}, "the archive holds", None, False),
    ("match on one common word", {"abstract": {"match": "archive"}}, "archive", None, False),
    # The adjacency pair, as a conjunction: both the entities that say it together and the ones
    # that say it apart.
    ("match on the pair, in either arrangement", {"abstract": {"match": "quiet harbour"}}, "quiet harbour", None, False),
    # ... and as a phrase, which must be the strict subset that says it adjacent and in order.
    ("phrase on the pair", {"abstract": {"phrase": "quiet harbour"}}, "quiet harbour", None, True),
    ("phrase on the pair reversed", {"abstract": {"phrase": "harbour quiet"}}, "harbour quiet", None, True),
    # A phrase whose words are common and whose sequence occurs: the head itself.
    ("phrase on the shared head", {"abstract": {"phrase": "the archive holds"}}, "the archive holds", None, True),
    ("phrase on the head reordered", {"abstract": {"phrase": "archive the holds"}}, "archive the holds", None, True),
    # A one-word phrase is a `match`, and the engine short-circuits the blob verify for it. The
    # oracle does not short-circuit, so agreement here is agreement that the short cut is sound.
    ("phrase of one word", {"abstract": {"phrase": "harbour"}}, "harbour", None, True),
    # The sole-carrier word: a single-item answer a posting confusion cannot fake.
    (
        "match on a word exactly one entity carries",
        {"abstract": {"match": cat.ABSTRACT_SOLE_WORD}},
        cat.ABSTRACT_SOLE_WORD,
        None,
        False,
    ),
    # The sentinel: hidden-versus-absent must be identical in the answer.
    (
        "match on a word no document carries",
        {"abstract": {"match": cat.ABSTRACT_ABSENT_WORD}},
        cat.ABSTRACT_ABSENT_WORD,
        None,
        False,
    ),
    (
        "match mixing a carried word with one nothing carries",
        {"abstract": {"match": f"archive {cat.ABSTRACT_ABSENT_WORD}"}},
        f"archive {cat.ABSTRACT_ABSENT_WORD}",
        None,
        False,
    ),
    # m-of-n at every m over three words, including the boundary where it becomes the conjunction
    # and the one above it, which is unsatisfiable rather than the conjunction.
    (
        "m-of-n, one of three",
        {"abstract": {"match": {"query": "quiet harbour zzzznonesuch", "minimum_should_match": 1}}},
        "quiet harbour zzzznonesuch",
        1,
        False,
    ),
    (
        "m-of-n, two of three",
        {"abstract": {"match": {"query": "quiet harbour zzzznonesuch", "minimum_should_match": 2}}},
        "quiet harbour zzzznonesuch",
        2,
        False,
    ),
    (
        "m-of-n, three of three is the conjunction",
        {"abstract": {"match": {"query": "quiet harbour zzzznonesuch", "minimum_should_match": 3}}},
        "quiet harbour zzzznonesuch",
        3,
        False,
    ),
    (
        "m-of-n above the token count is unsatisfiable",
        {"abstract": {"match": {"query": "quiet harbour", "minimum_should_match": 4}}},
        "quiet harbour",
        4,
        False,
    ),
    # **Non-Latin prose through the real segmenter.** A split-on-spaces tokeniser indexes each of
    # these as one token, so a query for an inner word finds nothing; the engine and the oracle
    # must agree on the segmentation *and* on what it makes findable.
    ("match on a CJK run", {"abstract": {"match": cat._ABSTRACT_CJK}}, cat._ABSTRACT_CJK, None, False),
    ("phrase on a CJK run", {"abstract": {"phrase": cat._ABSTRACT_CJK}}, cat._ABSTRACT_CJK, None, True),
    ("match on a Thai run", {"abstract": {"match": cat._ABSTRACT_THAI}}, cat._ABSTRACT_THAI, None, False),
    # Case and width fold at the analyser, so an uppercase query is the same question.
    ("match, uppercased", {"abstract": {"match": "ARCHIVE"}}, "ARCHIVE", None, False),
    # A query analysing to no tokens at all matches nothing — never everything.
    ("match on punctuation alone", {"abstract": {"match": "!!! ---"}}, "!!! ---", None, False),
]

# The principals the matrix runs against — the same spread the keyword differential uses, and for
# the same reason at one remove: `empty` is where a route that forgot the candidate returns the
# whole corpus, and the coverage spread puts different posting/candidate density ratios in play.
MATRIX_PRINCIPALS = [
    "empty",
    "sparse_0_01pct",
    "container_boundary",
    "crossover_above",
    "full_100pct",
]

# The composed sweep: a text leaf beside a category leaf and a keyword leaf, so the text operands
# are shown to compose under decision 0062's tree rather than only to answer alone.
COMPOSED_EXPR = {
    "all_of": [
        {"abstract": {"phrase": "quiet harbour"}},
        {"any_of": [{"department": {"eq": "alpha"}}, {"submitter": {"prefix": "hub-"}}]},
    ]
}


def _tiles_by_id(tiles) -> dict[int, tuple[int, int, int]]:
    return {t: (v, m, s) for t, v, m, s in tiles}


def _served_entities(raw: bytes, entity_of_fx: dict[int, int]) -> set[int]:
    """The served set as entity ids, joined through the planted `fx_key`."""
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
def column() -> txt.TextColumn:
    """The whole corpus's prose, tokenised once through the shipped analyser."""
    prose = {
        e: value
        for e in range(cat.N_ITEMS)
        if (value := cat.abstract_of(e)) is not None
    }
    return txt.TextColumn.from_prose(prose, CLI_BIN)


@pytest.fixture(scope="module")
def tokens_of(column):
    """Query text → its token list, through the same analyser, memoised per module."""
    cache: dict[str, list[str]] = {}

    def get(query: str) -> list[str]:
        if query not in cache:
            cache[query] = txt.tokenise([query], CLI_BIN)[0]
        return cache[query]

    return get


@pytest.fixture(scope="module")
def unfiltered(catalogue_server, cases):
    baseline: dict[str, dict[int, tuple[int, int, int]]] = {}

    def get(case_name: str):
        if case_name not in baseline:
            case = cases[case_name]
            token = catalogue_server.authorise(list(case.grants))["token"]
            raw = catalogue_server.viewport(token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT)
            baseline[case_name] = _tiles_by_id(decode_viewport(raw)[0])
        return baseline[case_name]

    return get


# ---------------------------------------------------------------------------------------------
# The fixture's own precondition
# ---------------------------------------------------------------------------------------------


def test_the_text_column_has_the_shapes_its_catalogue_entries_need(column, tokens_of):
    """Every claim the tests below rest on, derived from the generation function and checked.

    A differential against a fixture that quietly lost its shapes passes while testing nothing — a
    corpus of one repeated sentence would agree with the oracle on every assertion in this module.
    Each claim is named for the entry it makes reachable.
    """
    carriers: dict[str, int] = {}
    for stream in column.tokens.values():
        for word in set(stream):
            carriers[word] = carriers.get(word, 0) + 1

    # A word nearly every document carries, so a conjunction has something wide to intersect.
    assert carriers["archive"] > 100_000, carriers.get("archive")

    # A word exactly one entity carries — the family's single-item answer.
    assert carriers[cat.ABSTRACT_SOLE_WORD] == 1
    assert column.tokens[cat.ABSTRACT_SOLE_ID].count(cat.ABSTRACT_SOLE_WORD) == 1

    # The sentinel is genuinely absent from the whole corpus.
    assert cat.ABSTRACT_ABSENT_WORD not in carriers

    # **Both adjacency shapes, densely.** Without the second the phrase differential is vacuous:
    # every entity carrying both words would carry them adjacent, and returning the conjunction
    # would pass.
    pair = tokens_of("quiet harbour")
    both = column.carriers(lambda e: column.matches(e, pair, None))
    adjacent = column.carriers(lambda e: column.has_phrase(e, pair))
    assert len(adjacent) > 10_000, len(adjacent)
    assert len(both - adjacent) > 10_000, len(both - adjacent)
    assert adjacent < both, "a phrase's carriers must be a strict subset of the conjunction's"

    # Non-Latin prose, and it segments into more than one token — the whole reason the analyser is
    # icu4x rather than a split on spaces.
    cjk = tokens_of(cat._ABSTRACT_CJK)
    assert len(cjk) > 1, cjk
    assert len(column.carriers(lambda e: column.matches(e, cjk, None))) > 10_000
    thai = tokens_of(cat._ABSTRACT_THAI)
    assert len(thai) > 1, thai

    # A long singleton tail, which is what real prose has and what puts both posting encodings in
    # play (a small posting is a bare `u32` array, a large one a Roaring bitmap).
    assert sum(1 for n in carriers.values() if n == 1) > 100_000

    # And the absence stride left a scattering of entities with no prose at all: an entity carrying
    # no value must match no predicate, which is only checkable if some entity carries none.
    assert len(column.tokens) < cat.N_ITEMS
    assert cat.N_ITEMS - len(column.tokens) > 1_000


def test_the_manifest_records_the_analyser_the_oracle_tokenises_with(catalogue_bundle):
    """**The differential is only a differential if both sides used one pipeline** (decision 0070).

    The column records a full `<name>/<version>` identity; the oracle reaches the analyser through
    `tessera tokenise`, whose default is the same name. If the manifest ever records an identity
    the CLI's default does not resolve to, every comparison in this module becomes a comparison
    between two segmentations and its agreements stop meaning anything — silently, because both
    sides would still be internally consistent.
    """
    declared = {d["name"]: d for d in catalogue_bundle.manifest["declared_scalars"]}
    recorded = declared["abstract"]["analyser"]
    assert recorded == txt.identity(CLI_BIN), (
        f"the column was indexed by {recorded!r} and the oracle tokenises with "
        f"{txt.identity(CLI_BIN)!r}"
    )
    assert recorded.startswith("unicode/")


def test_meta_publishes_the_analyser_identity_a_client_would_need(
    catalogue_server, catalogue_bundle, cases
):
    """**The identity is on the wire, not only in the manifest** (contracts §3.1, decision 0070).

    A client sends query text raw and the server segments it, so an empty `match` is ambiguous
    between *no document says this* and *your query segmented differently from the index* — and for
    CJK or Thai the second is the likely one. The identity is what separates them, and a client that
    has it can reproduce the segmentation through `tessera tokenise`.

    Asserted against the **manifest's** own field rather than a literal, since the two being the
    same string is the whole point; and against a non-text column, whose `null` is what makes the
    field's presence mean something.
    """
    token = catalogue_server.authorise(list(cases["full_100pct"].grants))["token"]
    published = {s["name"]: s for s in catalogue_server.meta(token)["declared_scalars"]}
    declared = {d["name"]: d for d in catalogue_bundle.manifest["declared_scalars"]}

    assert published["abstract"]["analyser"] == declared["abstract"]["analyser"]
    assert published["abstract"]["analyser"] == txt.identity(CLI_BIN)
    for name in ("submitter", "department", "pages"):
        assert published[name]["analyser"] is None, (
            f"{name} is not a text column and has no analyser — publishing one would invite a "
            "client to reproduce a segmentation that was never applied"
        )


def test_meta_publishes_text_with_exactly_its_two_operands(catalogue_server, cases):
    """`/v1/meta` names `abstract` a `text` column taking `match` and `phrase`, and **not** the four
    string predicates — the operand list is what a client is held to, and a family that published
    an operator the parse gate refuses is the drift `filter_operands_expected` exists to pin."""
    token = catalogue_server.authorise(list(cases["full_100pct"].grants))["token"]
    meta = catalogue_server.meta(token)
    published = {
        row["column"]: (row["family"], frozenset(row["operands"]))
        for row in meta["filter_operands"]
    }
    assert published == cat.filter_operands_expected()
    assert published["abstract"] == ("text", frozenset({"match", "phrase"}))

    # And the refusals that make the list load-bearing rather than decorative.
    for body, why in [
        ({"abstract": {"eq": "archive"}}, "a string predicate on a text column"),
        ({"abstract": {"prefix": "arch"}}, "a prefix on a text column"),
        ({"abstract": {"range": {"gte": 3}}}, "a numeric operator on a text column"),
        ({"submitter": {"match": "hub"}}, "`match` on a keyword column"),
        ({"abstract": {"phrase": {"query": "a b"}}}, "an object form for `phrase`"),
        (
            {"abstract": {"match": {"query": "a", "minimum_should_match": 0}}},
            "a zero minimum",
        ),
    ]:
        resp = catalogue_server.viewport_request(
            token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT, filters=body
        )
        assert resp.status_code == 422, f"{why} must be refused: {resp.text}"

    # **A negation over a text column is refused, not answered empty** (records §4.4). There is no
    # per-item value for `none_of`'s presence half to subtract from, and answering the empty set
    # would be indistinguishable from a corpus where nothing matches.
    resp = catalogue_server.viewport_request(
        token,
        cat.VIEW_ID,
        ZOOM,
        cat.FULL_VIEWPORT,
        filters={"none_of": [{"abstract": {"match": "archive"}}]},
    )
    assert resp.status_code == 422, resp.text
    assert "abstract" in resp.text


# ---------------------------------------------------------------------------------------------
# The differential
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal", MATRIX_PRINCIPALS)
@pytest.mark.parametrize(
    ("label", "expr", "query", "minimum", "phrase"),
    BASE_EXPRESSIONS,
    ids=[e[0] for e in BASE_EXPRESSIONS],
)
def test_the_engine_and_the_oracle_name_the_same_entities(
    catalogue_server,
    cases,
    unfiltered,
    entity_of_fx,
    column,
    tokens_of,
    principal,
    label,
    expr,
    query,
    minimum,
    phrase,
):
    """Engine against oracle, per principal per expression: **the served set is equal, exactly.**

    The server is θ-saturated for this fixture, so the served set *is* `M_sel` — which means a
    filter that dropped a visible matching item is caught here, not only one that widened. And
    `visible` must not move: a filter narrows `matched` and never the composed mask (I12).
    """
    case = cases[principal]
    token = catalogue_server.authorise(list(case.grants))["token"]
    raw = catalogue_server.viewport(
        token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
    )
    tiles, _ = decode_viewport(raw)
    got = _served_entities(raw, entity_of_fx)

    q = tokens_of(query)
    if phrase:
        want = column.carriers(lambda e: column.has_phrase(e, q)) & case.entities
    else:
        want = column.carriers(lambda e: column.matches(e, q, minimum)) & case.entities
    assert got == want, (
        f"{principal} / {label}: engine and oracle disagree on "
        f"{len(got ^ want)} entities (engine-only {sorted(got - want)[:5]}, "
        f"oracle-only {sorted(want - got)[:5]})"
    )

    # I12's mask half: `visible` is the composed count and a filter never moves it.
    baseline = unfiltered(principal)
    for tile, (visible, matched, _served) in _tiles_by_id(tiles).items():
        assert visible == baseline[tile][0], f"{principal} / {label}: tile {tile}'s visible moved"
        assert matched <= visible, f"{principal} / {label}: tile {tile} matched more than visible"


def test_a_phrase_is_a_strict_subset_of_its_own_conjunction(
    catalogue_server, cases, entity_of_fx, column, tokens_of
):
    """**The phrase's answer is inside the `match`'s, and strictly** — asserted engine-to-engine.

    The matrix above compares each operand against the oracle separately, so a route that answered
    both from the conjunction would fail the phrase rows. This says the same thing without the
    oracle, which is the check that survives an oracle bug: a phrase cannot name an entity its own
    conjunction does not, and on this corpus it must name fewer.
    """
    token = catalogue_server.authorise(list(cases["full_100pct"].grants))["token"]
    served = {}
    for name, expr in [
        ("match", {"abstract": {"match": "quiet harbour"}}),
        ("phrase", {"abstract": {"phrase": "quiet harbour"}}),
    ]:
        raw = catalogue_server.viewport(
            token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
        )
        served[name] = _served_entities(raw, entity_of_fx)

    assert served["phrase"] < served["match"], (
        "a phrase must be a strict subset of its own conjunction; equal sets mean the verify did "
        "nothing, and a superset means it widened"
    )
    assert len(served["match"] - served["phrase"]) > 1_000


def test_a_word_no_document_carries_answers_and_answers_empty(
    catalogue_server, cases, entity_of_fx
):
    """The sentinel, and the hidden-versus-absent pair **in the answer** (Appendix C, C25).

    A word no document carries must produce an ordinary empty answer — not a `422`, which would
    make the filter an existence oracle over the corpus's vocabulary, and not a `500`. And a word
    the corpus *does* carry, asked by a principal who may see none of its carriers, must produce
    the byte-identical response: that is what C25 accepts as observable only in the timing.

    The positive control is beside it: the same shapes against a principal who can see a carrier
    must be non-empty, or "identical and empty" is satisfied by a route that answers nothing.
    """
    empty_case = cases["empty"]
    full_case = cases["full_100pct"]
    empty_token = catalogue_server.authorise(list(empty_case.grants))["token"]
    full_token = catalogue_server.authorise(list(full_case.grants))["token"]

    def body(token, word):
        return catalogue_server.viewport(
            token,
            cat.VIEW_ID,
            ZOOM,
            cat.FULL_VIEWPORT,
            filters={"abstract": {"match": word}},
        )

    absent = body(full_token, cat.ABSTRACT_ABSENT_WORD)
    assert _served_entities(absent, entity_of_fx) == set()

    # Hidden: a word the corpus carries, asked by a principal who sees nothing.
    hidden = body(empty_token, "archive")
    # Absent, asked by the same principal.
    nothing = body(empty_token, cat.ABSTRACT_ABSENT_WORD)
    # **Every frame but the trailer**, and the exclusion is C25 itself rather than a convenience:
    # the trailer carries `stream_us`, which is the service time the register accepts as the one
    # observable difference between these two requests. Comparing it here would assert the opposite
    # of what was ruled. Everything a caller *reads* — the tile counts, the sub-cell stream, the
    # points — must be identical, and is compared frame by frame so a surface added later is
    # covered without this test being touched.
    def answer(raw: bytes) -> list[tuple[int, bytes]]:
        return [(kind, payload) for kind, payload in split_frames(raw) if kind != FRAME_TRAILER]

    assert answer(hidden) == answer(nothing), (
        "a token the corpus holds and one it does not must be identical in the answer for a "
        "principal who may see neither; only the service time may differ (C25)"
    )

    # The positive control.
    carried = body(full_token, "archive")
    assert len(_served_entities(carried, entity_of_fx)) > 100_000
    assert carried != absent


def test_text_operands_compose_under_the_tree(
    catalogue_server, cases, entity_of_fx, column, tokens_of, catalogue_filter_columns
):
    """A text leaf beside a category leaf and a keyword leaf, under `all_of`/`any_of`.

    A family whose operand answers alone and composes wrongly is a family that passes every test
    above. The oracle evaluates the same tree by set arithmetic over its own three columns.
    """
    from oracle import filters as filt  # noqa: PLC0415

    case = cases["crossover_above"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    raw = catalogue_server.viewport(
        token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT, filters=COMPOSED_EXPR
    )
    got = _served_entities(raw, entity_of_fx)

    pair = tokens_of("quiet harbour")
    department = catalogue_filter_columns["department"]
    submitter = catalogue_filter_columns["submitter"]
    want = {
        e
        for e in case.entities
        if column.has_phrase(e, pair)
        and (
            department.matches(e, "alpha")
            or submitter.matches(e, "prefix", "hub-")
        )
    }
    assert got == want, f"composition disagrees on {len(got ^ want)} entities"
    assert len(want) > 100, "the composed expression must select something to be a test"
    assert filt is not None


def test_a_suppressed_entity_is_not_served_though_its_terms_are_in_the_postings(
    catalogue_server, catalogue_bundle, cases, entity_of_fx, column, tokens_of
):
    """**The catalogue's suppression row, on the text routes** (write-path §5.4, Rule S).

    A suppression retires nothing and touches no artefact — the suppressed entity's terms are still
    in the token postings, and its prose is still a blob row. What must not happen is that either
    route serves it. Both are checked, because they read different artefacts: `match` reads the
    postings, and `phrase` reads the postings *and then the record blob*, which is the one filter
    route in this system that decompresses a stored value at query time.

    Asserted against the same expressions before and after, so the difference is exactly the one
    suppression rather than a corpus that answered differently for another reason.
    """
    from oracle.journal import AckedJournal  # noqa: PLC0415

    case = cases["full_100pct"]
    token = catalogue_server.authorise(list(case.grants))["token"]
    journal = AckedJournal(catalogue_server, catalogue_bundle)
    pair = tokens_of("quiet harbour")

    # A victim that both routes name: it says the phrase, so it is in the conjunction and survives
    # the verify.
    victim = next(
        e
        for e in sorted(column.tokens)
        if column.has_phrase(e, pair) and cat.abstract_of(e) is not None
    )

    def served(expr):
        raw = catalogue_server.viewport(
            token, cat.VIEW_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr
        )
        return _served_entities(raw, entity_of_fx)

    match_expr = {"abstract": {"match": "quiet harbour"}}
    phrase_expr = {"abstract": {"phrase": "quiet harbour"}}
    before_match = served(match_expr)
    before_phrase = served(phrase_expr)
    assert victim in before_match and victim in before_phrase

    resp = journal.change(victim, "suppress")
    assert resp.status_code == 200, resp.text

    after_match = served(match_expr)
    after_phrase = served(phrase_expr)
    assert victim not in after_match, "a suppressed entity was served through `match`"
    assert victim not in after_phrase, "a suppressed entity was served through `phrase`"
    assert before_match - after_match == {victim}, "the suppression moved more than one item"
    assert before_phrase - after_phrase == {victim}

    # And Rule S's other half: the artefacts are untouched, so an unsuppress reveals it again.
    resp = journal.change(victim, "unsuppress")
    assert resp.status_code == 200, resp.text
    assert served(match_expr) == before_match
    assert served(phrase_expr) == before_phrase


def test_the_golden_tokeniser_vectors_run_here_too():
    """**The analyser's golden vectors, in the suite** — not only in the crate's own tests.

    The vectors pin the token stream across fourteen script families, and the whole text family
    rests on them: an analyser change that altered a segmentation would silently change what every
    index in every bundle means. They live in `crates/tessera-analyse/tests/vectors/golden.json`
    and run in Rust; running them here as well is what makes them a *conformance* obligation rather
    than one crate's unit test, and it checks them through the CLI — the surface the oracle uses —
    rather than through the library.
    """
    import json  # noqa: PLC0415
    from pathlib import Path  # noqa: PLC0415

    from oracle.harness import REPO_ROOT  # noqa: PLC0415

    path = Path(REPO_ROOT) / "crates/tessera-analyse/tests/vectors/golden.json"
    doc = json.loads(path.read_text(encoding="utf-8"))

    for entry in doc["analysers"]:
        name = entry["name"]
        assert txt.identity(CLI_BIN, name) == entry["identity"], (
            f"analyser {name!r} records {entry['identity']!r} in the vectors and the binary "
            f"answers {txt.identity(CLI_BIN, name)!r} — every index built under the old identity "
            "means something different"
        )
        inputs = [v["input"] for v in entry["vectors"]]
        expected = [v["tokens"] for v in entry["vectors"]]
        assert txt.tokenise(inputs, CLI_BIN, name) == expected, (
            f"analyser {name!r} no longer produces its golden token streams"
        )
        assert len(entry["vectors"]) >= 10, "the vector set has shrunk"
