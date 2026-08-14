"""What more than one layer adds to the text family — records §4.4, §4.5, §7, §10.

Three artefacts are per-layer for this family where the keyword family has two. A term ordinal is a
position in **its own layer's token dictionary** and means nothing anywhere else; the postings it
indexes are that layer's; and the prose `phrase` re-reads to check adjacency is that layer's record
blob. So `match` crosses two per-layer artefacts and `phrase` crosses three, and the third is the
one no engine-level fold test opens — `tests/fold_text.rs` reads dictionaries and postings out of
the artefacts directly, and never asks a server for a phrase.

The corpus is driven to three states over the control plane, on a private copy of the catalogue
bundle:

* **base + extent A** — one ingest, one flush;
* **base + extent A + extent B** — a second ingest and flush, so a term can be in two extents at two
  different ordinals and in neither the base nor the other extent;
* **folded** — `POST /control/compact`, after which there is one layer again and one dictionary
  rebuilt from the survivors.

The batches' prose is chosen so each layer's dictionary disagrees with the others where it costs.
The base's own first term is `archive` (every planted document carries it), and **neither batch
uses that word**; each batch's first term is one no other layer holds. An engine that resolved a
term in one layer and scanned another with the ordinal it got therefore returns *the wrong
entities* — `archive` would pick up `aalborg`'s single carrier — rather than none, which is the
failure a subset assertion would miss.

The catalogue entries records §10 gives this family, and what layering adds to each:

| entry | here |
|---|---|
| a word exactly one entity carries | `test_a_term_one_layer_holds_and_another_does_not_is_resolved_per_layer` |
| a word no dictionary holds | `test_a_term_no_layer_holds_answers_empty_at_every_state` |
| m-of-n at every m | the matrix, over a query whose three terms live in different layers |
| a phrase whose words are present but not adjacent | the matrix, with a counter-witness per layer |
| non-Latin prose under the real segmenter | the matrix — the base's last term is a CJK one, and
  extent A's prose carries a different CJK run that sorts *after* it |
| hidden versus absent, a suppressed entity's terms | the base module; neither gains anything from
  a second layer |

**One state this suite cannot reach, and it is not faked.** There is no `contains ""` counterpart
here: a text column publishes `match` and `phrase` and nothing that asks whether an item carries
prose at all ([#123](https://github.com/jennis0/tessera-index/issues/123)), so the keyword module's
presence probe has no form to take — what stands in its place is an ingested row whose prose
analyses to *no terms*, which must match no predicate while still being served.

**The coalesced state is reachable and is deliberately not driven here.** A text column *is*
coalesced (`filter-index.md` §5.2, unlike a keyword one), so a window of extents collapses into one
renumbered layer between folds — but the pass fires on a policy width of eight flushes, and driving
eight over the control plane to reach a state whose correctness is a merge property would be a slow
way to test what `crates/tessera-engine/tests/coalesce_text.rs` tests directly against the
artefacts. What this module owns is the layered and folded answers, which is where a cross-layer
defect reaches a client.

Every probe is asked at all three states, so the layered and the folded answers are each compared
against one definition and therefore against each other — epic #86's *folded-against-layered
differential*, at the service surface rather than at the merge function.
"""

from __future__ import annotations

import io
from dataclasses import dataclass

import pytest

from oracle import catalogue as cat
from oracle import text as txt
from oracle.harness import CLI_BIN, spawn_server, stop_server
from oracle.wire import decode_viewport, decode_viewport_points, split_frames

ZOOM = 3

# The access label every ingested row carries, and the descriptor a session is granted to see them.
# `builtin:passthrough` makes the two the same string.
INGEST_ACCESS = "text-extent"

# The principal: one catalogue block plus the ingested rows. `cross_lo` because it holds every
# planted entity this module names and is small enough that a full-viewport response is a few
# thousand points rather than the whole corpus.
BASE_CASE = "crossover_below"

# **The base dictionary's own boundaries**, derived from the fixture's prose and pinned here so the
# probes below are a fixed matrix rather than a computed one. `archive` is in every planted document
# and sorts before every other Latin term; the Japanese `日本語` sorts after every term in the
# corpus, Thai's `E0` lead byte preceding the `E3`/`E6` of the two other CJK tokens. Both are
# checked against the oracle's own vocabulary in the precondition test — a corpus edit that moved
# either would otherwise leave these probes quietly testing an interior ordinal.
BASE_FIRST_TERM = "archive"
BASE_LAST_TERM = "日本語"

# Two planted entities whose per-document term (`ref<id>`, one carrier each) is written into a
# batch as well, so one term lives in the base *and* in one extent at two unrelated ordinals — the
# shape a flush creates and a reused resolve breaks on. Both are inside `cross_lo` and off the
# absence stride, which the precondition test re-derives rather than trusts.
BASE_AND_A_ID = 65_903
BASE_AND_B_ID = 65_905
BASE_AND_A_TERM = f"ref{cat.PLANTED_ID_BASE + BASE_AND_A_ID}"
BASE_AND_B_TERM = f"ref{cat.PLANTED_ID_BASE + BASE_AND_B_ID}"

# The two batches' `abstract` values, in row order. Each row exists for one relation:
#
# 0. the adjacency **witness** — the pair adjacent and in order, so `phrase` has an answer that
#    lives in an extent and must be verified against *that extent's* record blob;
# 1. the adjacency **counter-witness** — the same two words apart, which matches the conjunction and
#    must not match the phrase, so an extent's phrase route cannot pass by returning its postings;
# 2. a planted entity's own term, at an ordinal that is nothing like the one the base gave it;
# 3. `None` in A — an ingested row carrying no value at all in this column;
# 4. a term neither the base nor the other extent holds, beside one both extents hold.
#
# Extent B's last row carries prose that analyses to **no terms**: a value with no posting anywhere,
# which is what the flush's presence bitmap exists for and what the base build does not yet write.
#
# Sorted, extent A's dictionary is `aalborg`(0), `beside`, `estuary`, `harbour`, `quiet`, `quokka`,
# `ref1065903`, `repository`, `the`, `zymurgy`, `東京`(last) and extent B's is `aabenraa`(0),
# `beyond`, `harbour`, `quiet`, `quokka`, `ref1065905`, `repository`, `strand`, `the`, `zephyr`.
# Read those against the base's — `archive`(0), the shared head, the pair, 148,905 `ref` terms,
# `日本語`(last) — and every disagreement the entries need is there: each layer's ordinal 0 is a
# term no other layer holds, `quokka` and `repository` are in both extents at different ordinals and
# in no base dictionary, and `東京` sits past the base's last ordinal so an out-of-range ordinal
# clamped rather than refused has a carrier to return.
BATCH_A: list[str | None] = [
    "aalborg quiet harbour",
    "harbour beside the quiet estuary",
    f"repository {BASE_AND_A_TERM} 東京",
    None,
    "zymurgy quokka",
]
BATCH_B: list[str | None] = [
    "aabenraa quiet harbour",
    "harbour beyond the quiet strand",
    f"repository {BASE_AND_B_TERM}",
    "quokka zephyr",
    "«»— ‡",
]

# The probes, run at every state. The bodies are `(label, filter, query, minimum, phrase)` — the
# last three being what the oracle needs to ask the same question, kept beside the wire form so the
# two cannot drift. The label names the layer relation each one attacks.
LAYER_EXPRESSIONS: list[tuple[str, dict, str, int | None, bool]] = [
    (
        "base only, at the base's ordinal 0",
        {"abstract": {"match": BASE_FIRST_TERM}},
        BASE_FIRST_TERM,
        None,
        False,
    ),
    (
        "base only, at the base's last ordinal",
        {"abstract": {"match": BASE_LAST_TERM}},
        BASE_LAST_TERM,
        None,
        False,
    ),
    (
        "base only, a term exactly one planted entity carries",
        {"abstract": {"match": cat.ABSTRACT_SOLE_WORD}},
        cat.ABSTRACT_SOLE_WORD,
        None,
        False,
    ),
    (
        "the base and extent A, at different ordinals",
        {"abstract": {"match": BASE_AND_A_TERM}},
        BASE_AND_A_TERM,
        None,
        False,
    ),
    (
        "the base and extent B, at different ordinals",
        {"abstract": {"match": BASE_AND_B_TERM}},
        BASE_AND_B_TERM,
        None,
        False,
    ),
    ("extent A only, at extent A's ordinal 0", {"abstract": {"match": "aalborg"}}, "aalborg", None, False),
    ("extent A only", {"abstract": {"match": "zymurgy"}}, "zymurgy", None, False),
    (
        "extent B only, at extent B's ordinal 0",
        {"abstract": {"match": "aabenraa"}},
        "aabenraa",
        None,
        False,
    ),
    ("extent B only", {"abstract": {"match": "zephyr"}}, "zephyr", None, False),
    ("both extents, no base", {"abstract": {"match": "quokka"}}, "quokka", None, False),
    (
        "extent A only, past the base's last ordinal",
        {"abstract": {"match": "東京"}},
        "東京",
        None,
        False,
    ),
    ("a term no layer holds", {"abstract": {"match": cat.ABSTRACT_ABSENT_WORD}}, cat.ABSTRACT_ABSENT_WORD, None, False),
    # The pair, as a conjunction every layer answers and as the phrase that is a strict subset of
    # it. Each extent adds one carrier to the phrase and two to the conjunction.
    ("match on the pair, in either arrangement", {"abstract": {"match": "quiet harbour"}}, "quiet harbour", None, False),
    ("phrase on the pair", {"abstract": {"phrase": "quiet harbour"}}, "quiet harbour", None, True),
    # A phrase whose leading word only one extent holds: the conjunction narrows to that extent's
    # own row and the verify must then read that extent's record blob, not the base's.
    ("phrase reaching extent A's blob", {"abstract": {"phrase": "aalborg quiet"}}, "aalborg quiet", None, True),
    ("phrase reaching extent B's blob", {"abstract": {"phrase": "aabenraa quiet"}}, "aabenraa quiet", None, True),
    # m-of-n over three terms living in three different places: `aalborg` in extent A alone,
    # `quokka` in both extents, `harbour` in the base and both. Every m is a different set.
    (
        "m-of-n, one of three across the layers",
        {"abstract": {"match": {"query": "aalborg quokka harbour", "minimum_should_match": 1}}},
        "aalborg quokka harbour",
        1,
        False,
    ),
    (
        "m-of-n, two of three across the layers",
        {"abstract": {"match": {"query": "aalborg quokka harbour", "minimum_should_match": 2}}},
        "aalborg quokka harbour",
        2,
        False,
    ),
    (
        "m-of-n, three of three is the conjunction",
        {"abstract": {"match": {"query": "aalborg quokka harbour", "minimum_should_match": 3}}},
        "aalborg quokka harbour",
        3,
        False,
    ),
    # A query analysing to no tokens matches nothing at any layer count — never everything.
    ("match on punctuation alone", {"abstract": {"match": "«»— ‡"}}, "«»— ‡", None, False),
]

# Where an ingested row's oracle id starts. Ids in this module's namespace are the fixture's own
# handle on a row and are never compared with anything the engine issues — the join to the wire is
# `fx_key`, exactly as it is for a planted entity. Far above `N_ITEMS` so the planted range and this
# one cannot silently overlap, and deliberately not `PLANTED_ID_BASE`, which is a different thing
# for a different reason (the offset a planted *value* applies before writing a number into text).
INGEST_ID_BASE = 9_000_000


@dataclass(frozen=True)
class Stage:
    """One corpus state, and everything needed to check it.

    `served` is what the engine answered, keyed by probe label and reduced to `fx_key`s — the
    fixture's own join, planted rows and ingested rows alike. `column` and `candidate` are the
    oracle's side: the prose the fixture planted or submitted, tokenised through the shipped
    analyser, and the entity set the principal holds. Nothing here is derived from the artefact.
    """

    name: str
    served: dict[str, frozenset[int]]
    unfiltered: frozenset[int]
    tiles: dict[int, tuple[int, int, int]]
    column: txt.TextColumn
    candidate: frozenset[int]
    fx_of: dict[int, int]
    segments: int


def _tiles_by_id(tiles) -> dict[int, tuple[int, int, int]]:
    return {t: (v, m, s) for t, v, m, s in tiles}


def _served_fx(raw: bytes) -> frozenset[int]:
    """The served set as `fx_key`s. A response that served nothing carries no points frame at
    all under the streamed format, and the empty set is a legitimate answer here."""
    if not any(kind == 3 for kind, _ in split_frames(raw)):
        return frozenset()
    return frozenset(decode_viewport_points(raw).column("fx_key").to_pylist())


def _ingest_body(prose: list[str | None], fx: list[int], external_base: int) -> bytes:
    """One `/control/ingest` batch carrying the catalogue's whole declared column set.

    Every declared column must be present (contracts §2.2) — the scalar tail is read back
    positionally, so an omitted column shifts every later scalar rather than defaulting to absent.
    Only `abstract` and `fx_key` carry anything this module reads; the rest are filled with
    declared, well-formed values, and `submitter` deliberately takes one constant key so this
    module's batches disagree with nothing the keyword layering asserts.

    Prose arrives as **prose**. The tokens, the dictionary this batch's terms are numbered against
    and the postings over it are the flush's to derive, on the pool, from this string.
    """
    import pyarrow as pa  # noqa: PLC0415
    import pyarrow.ipc as ipc  # noqa: PLC0415

    n = len(prose)
    schema = pa.schema(
        [
            pa.field("external_id", pa.binary()),
            pa.field("x", pa.float32()),
            pa.field("y", pa.float32()),
            pa.field("access", pa.utf8()),
            pa.field("fx_key", pa.uint64()),
            pa.field("department", pa.utf8()),
            pa.field("archive", pa.utf8()),
            pa.field("title", pa.utf8()),
            pa.field("submitter", pa.utf8()),
            pa.field("shelf", pa.utf8()),
            pa.field("abstract", pa.utf8()),
            pa.field("note", pa.utf8()),
            pa.field("pages", pa.uint32()),
        ]
    )
    batch = pa.record_batch(
        [
            pa.array([(external_base + i).to_bytes(8, "little") for i in range(n)], pa.binary()),
            # Well inside the extent, and spread far enough apart that no two rows share a depth-3
            # tile boundary. Geometry is not what this module tests; being served is.
            pa.array([4_000.0 + 37.0 * i for i in range(n)], pa.float32()),
            pa.array([9_000.0 + 53.0 * i for i in range(n)], pa.float32()),
            pa.array([INGEST_ACCESS] * n, pa.utf8()),
            pa.array(fx, pa.uint64()),
            pa.array(["alpha"] * n, pa.utf8()),
            pa.array(["red"] * n, pa.utf8()),
            # Offset past the entity-id range for `test_byte_scan`'s reason: a planted value that
            # reaches a client as text must not embed a number an entity id could equal.
            pa.array([f"ingested-{cat.PLANTED_ID_BASE + i}" for i in range(n)], pa.utf8()),
            pa.array(["hub-emea-d"] * n, pa.utf8()),
            pa.array(["north"] * n, pa.utf8()),
            pa.array(prose, pa.utf8()),
            pa.array([f"ingested-note-{cat.PLANTED_ID_BASE + i}" for i in range(n)], pa.utf8()),
            pa.array([100 + i for i in range(n)], pa.uint32()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


@pytest.fixture(scope="module")
def base_column() -> txt.TextColumn:
    """The whole planted corpus's prose, tokenised once through the shipped analyser.

    Whole rather than restricted to the principal, because two claims below are about the **base
    dictionary**, which the build wrote from every document: that `archive` is its first term and
    `日本語` its last. A vocabulary derived from one principal's slice could agree with both by
    accident.
    """
    prose = {
        e: value for e in range(cat.N_ITEMS) if (value := cat.abstract_of(e)) is not None
    }
    return txt.TextColumn.from_prose(prose, CLI_BIN)


@pytest.fixture(scope="module")
def layers(tmp_path_factory, private_catalogue_bundle, base_column):
    """Drive the corpus through its three states and record what the engine answered at each.

    **Its own server on its own copy of the bundle**: a flush publishes a new segment set *into the
    bundle prefix*, so a driver that wrote into the shared cached fixture would leave every later
    module and every later run reading a corpus nobody built.

    **A session is authorised only after the flush it is meant to see, and never before the
    first.** A session's visible set is materialised once, at authorise. That used to be load
    bearing against a defect — a credential naming the ingest's access descriptor *before* the flush
    which promoted it kept its pre-flush visible set thereafter, even across a re-authorise — which
    was diagnosed and fixed on 2026-08-14 (#112, and `dict_generation.rs` pins it). The habit is
    kept because it is the right one anyway: it says what each stage is meant to see. Every stage
    below authorises fresh after its flush, and asserts the unfiltered served count is the corpus it
    expects before checking any text answer, so a stale generation fails as its own precondition rather than as an
    unexplained disagreement about terms.
    """
    tmp_dir = tmp_path_factory.mktemp("text-layers")
    srv, proc = spawn_server(private_catalogue_bundle("text-layers"), tmp_dir)

    case = {c.name: c for c in cat.catalogue()}[BASE_CASE]
    planted_fx = cat.fx_keys()
    ingest_fx = cat.ingest_fx_keys(len(BATCH_A) + len(BATCH_B))

    # The oracle's side, in one id namespace: planted entities keep their entity id, ingested rows
    # get one of this module's own. Both are only ever mapped to `fx_key` before meeting the wire.
    tokens: dict[int, list[str]] = dict(base_column.tokens)
    fx_of: dict[int, int] = dict(enumerate(planted_fx))
    candidate: set[int] = set(case.entities)

    stages: dict[str, Stage] = {}

    def observe(name: str, expected_visible: int) -> None:
        token = srv.authorise([*case.grants, INGEST_ACCESS])["token"]
        plain = srv.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
        unfiltered = _served_fx(plain)
        assert len(unfiltered) == expected_visible, (
            f"{name}: this principal was served {len(unfiltered)} points where the corpus it "
            f"holds is {expected_visible}. Nothing about the text column has been checked yet — "
            "the generation the session resolved against is not the one that was just published"
        )
        served = {
            label: _served_fx(
                srv.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr)
            )
            for label, expr, _query, _minimum, _phrase in LAYER_EXPRESSIONS
        }
        stages[name] = Stage(
            name=name,
            served=served,
            unfiltered=unfiltered,
            tiles=_tiles_by_id(decode_viewport(plain)[0]),
            column=txt.TextColumn(tokens=dict(tokens)),
            candidate=frozenset(candidate),
            fx_of=dict(fx_of),
            segments=srv.status()["segments"][0]["count"],
        )

    def submit(batch: list[str | None], fx: list[int], external_base: int, offset: int) -> None:
        resp = srv.ingest(_ingest_body(batch, fx, external_base), f"text-layers-{offset}")
        assert resp.status_code == 200, resp.text
        assert resp.json()["accepted"] == len(batch)
        # One subprocess for the batch, through the same analyser the flush is about to use.
        streams = txt.tokenise([p for p in batch if p is not None], CLI_BIN)
        stream_of = iter(streams)
        for i, prose in enumerate(batch):
            oracle_id = INGEST_ID_BASE + offset + i
            fx_of[oracle_id] = fx[i]
            candidate.add(oracle_id)
            # A row carrying no value is absent from the column entirely; a row whose prose
            # analyses to no terms carries a value and an empty stream, and the two must not be
            # collapsed here — that distinction is the whole of what the flush's presence bitmap
            # records, and asserting it needs an oracle that keeps them apart.
            if prose is not None:
                tokens[oracle_id] = next(stream_of)
        srv.flush()

    try:
        base_visible = len(case.entities)
        submit(BATCH_A, ingest_fx[: len(BATCH_A)], 970_000_000, 0)
        observe("base+A", base_visible + len(BATCH_A))
        submit(BATCH_B, ingest_fx[len(BATCH_A) :], 980_000_000, len(BATCH_A))
        observe("base+A+B", base_visible + len(BATCH_A) + len(BATCH_B))
        srv.compact()
        observe("folded", base_visible + len(BATCH_A) + len(BATCH_B))
        yield stages
    finally:
        stop_server(proc)


@pytest.fixture(scope="module")
def tokens_of():
    """Query text → its token list, through the same analyser, memoised per module."""
    cache: dict[str, list[str]] = {}

    def get(query: str) -> list[str]:
        if query not in cache:
            cache[query] = txt.tokenise([query], CLI_BIN)[0]
        return cache[query]

    return get


def _expected(stage: Stage, query: list[str], minimum: int | None, phrase: bool) -> frozenset[int]:
    """The oracle's answer for one probe at one state, as `fx_key`s.

    A per-entity walk over the token streams the fixture's own prose produced — no dictionary, no
    ordinals, no notion that the corpus has layers at all. That last part is the point: the
    definition does not change when a flush publishes, so an engine whose answer *does* change for
    any reason but the new entities disagrees here.
    """
    if phrase:
        selected = {e for e in stage.candidate if stage.column.has_phrase(e, query)}
    else:
        selected = {e for e in stage.candidate if stage.column.matches(e, query, minimum)}
    return frozenset(stage.fx_of[i] for i in selected)


# ---------------------------------------------------------------------------------------------
# The states are the states they claim to be
# ---------------------------------------------------------------------------------------------


def test_the_three_states_are_reached_and_are_distinct(layers):
    """The fixture's own precondition. Each stage must have actually happened, or every assertion
    below is made against a corpus that never grew a second layer.

    The fold's evidence is the segment count collapsing: a fold rewrites the corpus into one
    segment per partition-slice, where two flushes had left three. The text column's own
    single-layer-again claim is not visible from here — it is an artefact fact, and
    `tests/fold_text.rs` opens the artefacts to assert it — and what this suite can check is that
    every answer is still right afterwards, which the tests below do.
    """
    assert set(layers) == {"base+A", "base+A+B", "folded"}
    assert layers["base+A"].segments == 2, layers["base+A"].segments
    assert layers["base+A+B"].segments == 3, layers["base+A+B"].segments
    assert layers["folded"].segments == 1, (
        f"the corpus still has {layers['folded'].segments} segments after a fold — no fold "
        "happened, and the folded column below is the layered one under another name"
    )
    assert len(layers["base+A"].unfiltered) < len(layers["base+A+B"].unfiltered)
    assert layers["base+A+B"].unfiltered == layers["folded"].unfiltered, (
        "the fold changed which items this principal can see"
    )


def test_the_layers_have_the_dictionary_shapes_the_probes_need(layers, base_column, tokens_of):
    """Every claim the probes rest on, derived from the fixture's prose and checked.

    A layering differential whose batches quietly stopped disagreeing with the base passes while
    testing nothing: if no extent's dictionary held a term at an ordinal the base gave to a
    different term, a single-dictionary engine would agree with the oracle everywhere. Each claim
    below is named for the relation it makes reachable.
    """
    base_terms = {term for stream in base_column.tokens.values() for term in stream}

    # The base's own ordinal boundaries, which the two boundary probes assume.
    assert min(base_terms) == BASE_FIRST_TERM, min(base_terms)
    assert max(base_terms) == BASE_LAST_TERM, max(base_terms)

    # The two planted entities whose per-document term an extent re-uses: each is the sole planted
    # carrier of its term, is visible to this principal, and carries prose at all.
    stage = layers["base+A"]
    for entity, term in [(BASE_AND_A_ID, BASE_AND_A_TERM), (BASE_AND_B_ID, BASE_AND_B_TERM)]:
        assert cat.abstract_of(entity) is not None
        assert entity in stage.candidate, f"entity {entity} is outside the principal {BASE_CASE}"
        planted = [e for e in base_column.tokens if term in base_column.tokens[e]]
        assert planted == [entity], planted

    # Each batch's own first term is one no other layer holds, which is what makes an ordinal-0
    # confusion return the wrong entities rather than none.
    a_terms = {t for prose in BATCH_A if prose for t in tokens_of(prose)}
    b_terms = {t for prose in BATCH_B if prose for t in tokens_of(prose)}
    assert min(a_terms) == "aalborg" and min(b_terms) == "aabenraa"
    assert "aalborg" not in base_terms and "aalborg" not in b_terms
    assert "aabenraa" not in base_terms and "aabenraa" not in a_terms
    assert BASE_FIRST_TERM not in a_terms and BASE_FIRST_TERM not in b_terms, (
        "a batch carries the base's first term, so the ordinal-0 probes no longer separate the "
        "layers"
    )
    assert BASE_LAST_TERM not in a_terms and BASE_LAST_TERM not in b_terms

    # A term both extents hold and no base dictionary does, at two different ordinals.
    assert "quokka" in a_terms and "quokka" in b_terms and "quokka" not in base_terms
    assert sorted(a_terms).index("quokka") != sorted(b_terms).index("quokka")

    # And extent A holds a term that sorts *past* the base's last, so an ordinal that ran off the
    # end of a dictionary and was clamped rather than refused has a carrier to return.
    assert max(a_terms) == "東京" > BASE_LAST_TERM

    # The two absence shapes, which only exist because a batch planted them: a row with no value in
    # this column, and a row whose value analyses to no terms at all.
    assert sum(1 for p in BATCH_A + BATCH_B if p is None) == 1
    assert [p for p in BATCH_B if p is not None and not tokens_of(p)] == ["«»— ‡"]


# ---------------------------------------------------------------------------------------------
# Every probe, every state
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("label", "expr", "query", "minimum", "phrase"),
    LAYER_EXPRESSIONS,
    ids=[e[0] for e in LAYER_EXPRESSIONS],
)
@pytest.mark.parametrize("state", ["base+A", "base+A+B", "folded"])
def test_every_probe_agrees_with_the_oracle_at_every_state(
    layers, tokens_of, state, label, expr, query, minimum, phrase
):
    """The matrix: twenty probes × three corpus states, exact set equality.

    θ is saturated on this server, so the served set is the filtered set and equality is the
    assertion — a subset check would pass an engine that resolved in the wrong layer and returned a
    strict subset, which is precisely the recolouring shape this module exists to catch.
    """
    stage = layers[state]
    want = _expected(stage, tokens_of(query), minimum, phrase)
    assert stage.served[label] == want, f"{state} / {label}"


def test_the_folded_index_answers_exactly_what_the_layered_one_did(layers):
    """**Folded against layered, on every probe at once** — epic #86's fourth gate, at the service.

    The fold discards both extents' dictionaries and the base's, rebuilds one dictionary from the
    surviving terms, renumbers every posting against it, and rewrites the record blob the phrase
    verify reads. Nothing about the *answers* may move: the same queries must select the same
    items. Asserted as a whole-matrix equality rather than per probe, because the failure this
    guards against — a renumbering that shifted a block of terms — moves several answers at once
    and is easiest to read as a diff of the whole set.

    This is the property `tests/fold_text.rs` cannot state. That module proves the merged artefact
    is the one a fresh build would write; this one proves a *search* returns the same items before
    and after a compaction, over a server, including the phrase route that reads prose back out of
    a blob the fold rewrote.
    """
    before = layers["base+A+B"]
    after = layers["folded"]
    assert after.served == before.served, (
        "the fold changed a text answer: "
        + ", ".join(
            label for label in before.served if before.served[label] != after.served[label]
        )
    )


# ---------------------------------------------------------------------------------------------
# The relations this module owns
# ---------------------------------------------------------------------------------------------


def test_a_term_one_layer_holds_and_another_does_not_is_resolved_per_layer(layers):
    """**A term present in one layer's dictionary and absent from another's** (records §10) — the
    shape a flush creates, and the one a cross-layer ordinal comparison breaks on.

    The entity *counts* are what make each relation a trap rather than a tautology:

    * `archive` is the base's ordinal 0 and no extent holds it. An engine that resolved it in the
      base and then scanned the extents with ordinal 0 would return extent A's `aalborg` row and
      extent B's `aabenraa` row as well — so the answer must not grow by a single item when an
      extent arrives.
    * `aalborg` is extent A's ordinal 0 and neither the base nor extent B holds it; `aabenraa` is
      extent B's. The same confusion in the other direction would drag in the base's `archive`
      carriers, which is every planted document this principal can see.
    * `ref1065903` is in the base *and* in extent A, at a mid-dictionary ordinal and a low one. One
      resolve reused across both layers finds it in one and misses in the other, so the count is
      short by exactly the flushed row.
    * `quokka` is in both extents and in no base dictionary, at two different ordinals — the
      flush-to-flush direction of the same entry.
    """
    first, second = layers["base+A"], layers["base+A+B"]

    # The base's ordinal 0: a large answer, and the assertion is that it does not move at all.
    anchor = "base only, at the base's ordinal 0"
    assert first.served[anchor] == second.served[anchor] == layers["folded"].served[anchor], (
        "the base's first term selected a different set once an extent existed — an ordinal "
        "resolved in one layer was scanned for in another"
    )
    assert len(first.served[anchor]) > 1_000, "the ordinal-0 probe selects too little to be a trap"

    for label, size in [
        ("extent A only, at extent A's ordinal 0", 1),
        ("extent B only, at extent B's ordinal 0", 1),
        ("base only, a term exactly one planted entity carries", 1),
        ("the base and extent A, at different ordinals", 2),
        ("the base and extent B, at different ordinals", 2),
        ("both extents, no base", 2),
    ]:
        assert len(second.served[label]) == size, (
            f"{label}: {len(second.served[label])} entities, expected {size} — a term resolved in "
            "one layer was scanned for in another, so the answer names items carrying a different "
            "term"
        )

    # And the same relations *before* extent B existed, which is where each is a state that
    # changes rather than a count that happens to be right.
    assert len(first.served["extent A only, at extent A's ordinal 0"]) == 1
    assert first.served["extent B only, at extent B's ordinal 0"] == frozenset()
    assert len(first.served["the base and extent A, at different ordinals"]) == 2
    assert len(first.served["the base and extent B, at different ordinals"]) == 1
    assert len(first.served["both extents, no base"]) == 1


def test_a_phrase_is_verified_against_each_layers_own_prose(layers):
    """**The phrase route crosses all three per-layer artefacts**, and this is the only place that
    is checked over a server.

    `phrase` narrows by the postings and then re-reads each survivor's prose out of the record blob
    to check adjacency (records §4.5). A flushed row's prose is in *that flush's* blob extent, so a
    verify that read only the base's would drop every extent row — silently, since dropping is the
    conservative direction and every remaining answer would still be correct.

    Three things are asserted: each extent adds exactly one carrier to the phrase and two to its own
    conjunction (the witness and the counter-witness), the phrase stays a strict subset, and a
    phrase whose leading term only one extent holds selects exactly that extent's row.
    """
    counts = {
        state: (
            len(layers[state].served["match on the pair, in either arrangement"]),
            len(layers[state].served["phrase on the pair"]),
        )
        for state in ("base+A", "base+A+B", "folded")
    }
    (match_a, phrase_a), (match_ab, phrase_ab) = counts["base+A"], counts["base+A+B"]
    assert match_ab - match_a == 2, counts
    assert phrase_ab - phrase_a == 1, counts
    assert counts["folded"] == counts["base+A+B"], counts

    for state in ("base+A", "base+A+B", "folded"):
        stage = layers[state]
        assert (
            stage.served["phrase on the pair"] < stage.served["match on the pair, in either arrangement"]
        ), f"{state}: the phrase is not a strict subset of its own conjunction"

    # The extent-local phrases: present from the flush that carries them, and nowhere before.
    assert len(layers["base+A"].served["phrase reaching extent A's blob"]) == 1
    assert layers["base+A"].served["phrase reaching extent B's blob"] == frozenset()
    for state in ("base+A+B", "folded"):
        assert len(layers[state].served["phrase reaching extent A's blob"]) == 1, state
        assert len(layers[state].served["phrase reaching extent B's blob"]) == 1, state


def test_a_term_no_layer_holds_answers_empty_at_every_state(layers):
    """**A needle absent from every dictionary** (records §10, §4.4): it must still answer, and
    answer empty — at one layer, at three, and after the fold.

    The failure mode is an unresolved term falling through to a sentinel ordinal that some layer's
    postings actually hold, which a single-layer suite cannot produce: with three dictionaries there
    are three chances for the sentinel to name something. A query analysing to no tokens at all is
    beside it, because it reaches the same empty answer by a different route — nothing to resolve
    rather than nothing found — and must not degenerate into *everything*.

    The positive control is the same column and the same principal at the same state: a term every
    layer's dictionary holds, which must select something everywhere.
    """
    for state in ("base+A", "base+A+B", "folded"):
        stage = layers[state]
        assert stage.served["a term no layer holds"] == frozenset(), state
        assert stage.served["match on punctuation alone"] == frozenset(), state
        assert stage.served["match on the pair, in either arrangement"], (
            f"{state}: the control selected nothing, so the empty answers above are a column that "
            "matches nothing at all"
        )


def test_an_ingested_row_with_no_terms_is_served_and_matches_nothing(layers):
    """The two absence shapes a batch can carry, which the base build cannot produce.

    One ingested row carries **no value** in this column; another carries prose that analyses to
    **no terms** — a value with no posting anywhere. Both must be served unfiltered and selected by
    no predicate, and the second is why the flush stores presence rather than deriving it from the
    postings: an engine deriving coverage from its postings reports that row absent.

    What cannot be asserted from here is the *difference* between the two, because no operand asks
    whether an item carries prose ([#123](https://github.com/jennis0/tessera-index/issues/123)).
    When one exists, this is where it gets its layered coverage.
    """
    stage = layers["base+A+B"]
    no_value = stage.fx_of[INGEST_ID_BASE + BATCH_A.index(None)]
    no_terms = stage.fx_of[INGEST_ID_BASE + len(BATCH_A) + BATCH_B.index("«»— ‡")]

    assert {no_value, no_terms} <= stage.unfiltered, (
        "a row carrying no terms was not served at all — absence in a text column is not absence "
        "from the corpus"
    )
    for label, served in stage.served.items():
        assert no_value not in served, f"a row with no value in this column matched {label!r}"
        assert no_terms not in served, f"a row whose prose has no terms matched {label!r}"
