"""What more than one layer adds to the keyword family — records §4.3, §7, §10.

An ordinal is a position in **its own layer's** dictionary and means nothing anywhere else. The
base build numbers the keys it holds; every flush extent numbers the keys *its batch* holds; the
compaction fold throws both away and renumbers the survivors. So a needle must be resolved once per
layer, against that layer's own dictionary, and the layers unioned — and an engine that resolved
once and scanned everywhere would be recolouring, which records §7 calls a fault with no symptom.
This module is where that has symptoms.

The corpus is driven to three states over the control plane, on a private copy of the catalogue
bundle:

* **base + extent A** — one ingest, one flush;
* **base + extent A + extent B** — a second ingest and flush, so a value can be in two extents at
  two different ordinals and in neither the base nor the other extent;
* **folded** — `POST /control/compact`, after which there is one layer again and one dictionary
  rebuilt from the survivors.

The batches are chosen so that each layer's dictionary disagrees with the others in the way the
catalogue entries name. In particular the base's ordinal 0 (`aaa-ingress-anchor`, one carrier) is a
value neither extent holds, and each extent's own ordinal 0 is a value the base does not hold — so
an engine reusing one layer's resolve against another's ordinals returns *the wrong entities*
rather than none, which is the failure a subset assertion would miss.

The catalogue entries records §10 gives this family, and where each is covered:

| entry | here |
|---|---|
| a value present in one layer's dictionary and absent from another's | `test_a_value_one_layer_holds_and_another_does_not_is_resolved_per_layer` |
| a prefix range empty in one layer and non-empty in the next | `test_a_prefix_range_empty_in_one_layer_and_not_in_the_next` |
| the first and last value of a dictionary (ordinal boundaries) | `test_the_base_s_ordinal_boundaries_survive_the_extents_and_the_fold` |
| a needle absent from every dictionary | `test_a_needle_no_layer_holds_answers_empty_at_every_state` |
| an entity whose only value arrived in a coalesced extent | **not reachable — see below** |

**The coalesced state cannot be reached from this suite, and it is not faked.** The entity-space
coalesce deliberately does not take a column whose extents carry a dictionary: a coalesced extent
would sit in the live composition beside the dictionaries of the extents it replaced, its ordinals
renumbered against a dictionary no reader holds. Such a column waits for the fold instead. The rule
is in `tessera_engine::coalesce::plan_coalesce` and is pinned there by
`a_column_with_per_layer_dictionaries_waits_for_the_fold`; the merge itself
(`tessera_filter_write::coalesce_keyword_extents`) has its own differential in that module,
`a_coalesced_keyword_extent_reads_back_every_entitys_own_key`, which checks the `(entity, key)`
relation across a renumbering merge and across a second coalesce of the first's output. So
there is no HTTP-reachable state in which a keyword value lives in a coalesced extent, and the
right coverage for the entry is the Rust pair above rather than a hand-written artefact here. What
this module covers in its place is the **fold**, which is the route such a column actually takes.

Every operator is checked at all three states, so the layered and the folded answers are compared
against one definition and therefore against each other — records §10's folded-against-layered
differential, in the form this family can express it.
"""

from __future__ import annotations

import io
from dataclasses import dataclass

import pytest

from oracle import catalogue as cat
from oracle import filters as filt
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport, decode_viewport_points, split_frames

ZOOM = 3

# The access label every ingested row carries, and the descriptor a session is granted to see them.
# `builtin:passthrough` makes the two the same string.
INGEST_ACCESS = "kw-extent"

# The principal: one catalogue block plus the ingested rows. `cross_lo` because it holds both of
# the base dictionary's anchors and is small enough that a full-viewport response is a few thousand
# points rather than the whole corpus.
BASE_CASE = "crossover_below"

# The two batches' `submitter` values, in row order. `None` is an ingested row carrying no keyword
# value at all — presence is partial in an extent exactly as it is in the base.
#
# Sorted, extent A's dictionary is `hub-emea-d`(0), `relay-delta`(1), `relay-gamma`(2) and extent
# B's is `aab-early-relay`(0), `beacon-node`(1), `hub-apac-h`(2), `relay-gamma`(3), `relay-zeta`(4).
# Read those against the base's — `aaa-ingress-anchor`(0), the thirty-six `hub-` keys, thousands of
# `node-` keys, `zzz-egress-anchor`(7,871) — and every disagreement the entries need is there:
# `hub-emea-d` is ordinal 16 in the base and 0 in A, `hub-apac-h` is 8 in the base and 2 in B,
# `relay-gamma` is 2 in A and 3 in B and in no base dictionary at all, and each extent's own
# ordinal 0 is a key the base has never held.
BATCH_A = ["hub-emea-d", "relay-gamma", "relay-gamma", "relay-delta", None]
BATCH_B = ["aab-early-relay", "beacon-node", "hub-apac-h", "relay-gamma", "relay-zeta"]

# A needle no layer's dictionary holds and no key contains.
ABSENT_NEEDLE = "no-such-submitter"

# The probes, run at every state. The label names the layer relation each one attacks.
LAYER_EXPRESSIONS: list[tuple[str, dict]] = [
    ("base only, at the base's ordinal 0", {"submitter": {"eq": cat.SUBMITTER_FIRST}}),
    ("base only, at the base's last ordinal", {"submitter": {"eq": cat.SUBMITTER_LAST}}),
    ("the base and extent A, at different ordinals", {"submitter": {"eq": "hub-emea-d"}}),
    ("the base and extent B, at different ordinals", {"submitter": {"eq": "hub-apac-h"}}),
    ("both extents, no base", {"submitter": {"eq": "relay-gamma"}}),
    ("extent A only", {"submitter": {"eq": "relay-delta"}}),
    ("extent B only, at extent B's ordinal 0", {"submitter": {"eq": "aab-early-relay"}}),
    ("extent B only", {"submitter": {"eq": "relay-zeta"}}),
    (
        "in, one value from each layer",
        {"submitter": {"in": [cat.SUBMITTER_FIRST, "relay-delta", "relay-zeta"]}},
    ),
    ("prefix empty in the base, not in either extent", {"submitter": {"prefix": "relay-"}}),
    ("prefix empty in the base and A, not in B", {"submitter": {"prefix": "beacon"}}),
    ("prefix in the base, empty in both extents", {"submitter": {"prefix": "node-emea-"}}),
    ("prefix every layer carries", {"submitter": {"prefix": "hub-"}}),
    ("contains, reaching the base and extent A", {"submitter": {"contains": "emea"}}),
    ("contains, reaching extent B alone", {"submitter": {"contains": "early"}}),
    ("a needle no layer holds", {"submitter": {"eq": ABSENT_NEEDLE}}),
    ("carries a value in this column at all", {"submitter": {"contains": ""}}),
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
    oracle's side: the strings the fixture planted or submitted, and the entity set the principal
    holds. Nothing here is derived from the artefact.
    """

    name: str
    served: dict[str, frozenset[int]]
    unfiltered: frozenset[int]
    tiles: dict[int, tuple[int, int, int]]
    column: filt.KeywordColumn
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


def _ingest_body(values: list[str | None], fx: list[int], external_base: int) -> bytes:
    """One `/control/ingest` batch carrying the catalogue's whole declared column set.

    Every declared column must be present (contracts §2.2) — the scalar tail is read back
    positionally, so an omitted column shifts every later scalar rather than defaulting to absent.
    Only `submitter` and `fx_key` carry anything this module reads; the rest are filled with
    declared, well-formed values.

    A keyword arrives as its **value**. The ordinal is the flush's to assign, against the
    dictionary it is about to write for this batch alone.
    """
    import pyarrow as pa  # noqa: PLC0415
    import pyarrow.ipc as ipc  # noqa: PLC0415

    n = len(values)
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
            pa.array(values, pa.utf8()),
            pa.array(["north"] * n, pa.utf8()),
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
def layers(tmp_path_factory, private_catalogue_bundle):
    """Drive the corpus through its three states and record what the engine answered at each.

    **Its own server on its own copy of the bundle**: a flush publishes a new segment set *into the
    bundle prefix*, so a driver that wrote into the shared cached fixture would leave every later
    module and every later run reading a corpus nobody built.

    **A session is authorised only after the flush it is meant to see, and never before the
    first.** A session's visible set is materialised once, at authorise; and on this build a
    credential that named the ingest's access descriptor *before* the flush which promoted it keeps
    its pre-flush visible set thereafter, even across a re-authorise (`README.md`, "Known
    limitations" — observed, reported, not diagnosed). Every stage below therefore authorises fresh
    after its flush, and asserts the unfiltered served count is the corpus it expects before
    checking any keyword answer, so a stale generation fails as its own precondition rather than as
    an unexplained keyword disagreement.
    """
    tmp_dir = tmp_path_factory.mktemp("keyword-layers")
    srv, proc = spawn_server(private_catalogue_bundle("keyword-layers"), tmp_dir)

    case = {c.name: c for c in cat.catalogue()}[BASE_CASE]
    planted_fx = cat.fx_keys()
    ingest_fx = cat.ingest_fx_keys(len(BATCH_A) + len(BATCH_B))

    # The oracle's side, in one id namespace: planted entities keep their entity id, ingested rows
    # get one of this module's own. Both are only ever mapped to `fx_key` before meeting the wire.
    values: dict[int, str] = {
        e: key for e in range(cat.N_ITEMS) if (key := cat.submitter_of(e)) is not None
    }
    fx_of: dict[int, int] = dict(enumerate(planted_fx))
    candidate: set[int] = set(case.entities)

    stages: dict[str, Stage] = {}

    def observe(name: str, expected_visible: int) -> None:
        token = srv.authorise([*case.grants, INGEST_ACCESS])["token"]
        plain = srv.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT)
        unfiltered = _served_fx(plain)
        assert len(unfiltered) == expected_visible, (
            f"{name}: this principal was served {len(unfiltered)} points where the corpus it "
            f"holds is {expected_visible}. Nothing about the keyword column has been checked yet "
            "— the generation the session resolved against is not the one that was just published"
        )
        served = {
            label: _served_fx(
                srv.viewport(token, cat.SLICE_ID, ZOOM, cat.FULL_VIEWPORT, filters=expr)
            )
            for label, expr in LAYER_EXPRESSIONS
        }
        stages[name] = Stage(
            name=name,
            served=served,
            unfiltered=unfiltered,
            tiles=_tiles_by_id(decode_viewport(plain)[0]),
            column=filt.KeywordColumn(values=dict(values)),
            candidate=frozenset(candidate),
            fx_of=dict(fx_of),
            segments=srv.status()["segments"][0]["count"],
        )

    def submit(batch: list[str | None], fx: list[int], external_base: int, offset: int) -> None:
        resp = srv.ingest(_ingest_body(batch, fx, external_base), f"kw-layers-{offset}")
        assert resp.status_code == 200, resp.text
        assert resp.json()["accepted"] == len(batch)
        for i, key in enumerate(batch):
            oracle_id = INGEST_ID_BASE + offset + i
            fx_of[oracle_id] = fx[i]
            candidate.add(oracle_id)
            if key is not None:
                values[oracle_id] = key
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


def _expected(stage: Stage, expr: dict) -> frozenset[int]:
    """The oracle's answer for one expression at one state, as `fx_key`s.

    A per-entity walk over the strings the fixture planted or submitted — no dictionary, no
    ordinals, no notion that the corpus has layers at all. That last part is the point: the
    definition does not change when a flush publishes, so an engine whose answer *does* change for
    any reason but the new entities disagrees here.
    """
    selected = filt.evaluate(expr, {"submitter": stage.column}, set(stage.candidate))
    return frozenset(stage.fx_of[i] for i in selected)


# ---------------------------------------------------------------------------------------------
# The states are the states they claim to be
# ---------------------------------------------------------------------------------------------


def test_the_three_states_are_reached_and_are_distinct(layers):
    """The fixture's own precondition. Each stage must have actually happened, or every assertion
    below is made against a corpus that never grew a second layer.

    The fold's evidence is the segment count collapsing: a fold rewrites the corpus into one
    segment per partition-slice, where two flushes had left three. The keyword column's own
    single-layer-again claim is not visible from here — it is an artefact fact — and what this
    suite can check is that every answer is still right afterwards, which the tests below do.
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


# ---------------------------------------------------------------------------------------------
# Every operator, every state
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("label,expr", LAYER_EXPRESSIONS, ids=[n for n, _ in LAYER_EXPRESSIONS])
@pytest.mark.parametrize("state", ["base+A", "base+A+B", "folded"])
def test_every_operator_agrees_with_the_oracle_at_every_state(layers, state, label, expr):
    """The matrix: seventeen expressions × three corpus states, exact set equality.

    θ is saturated on this server, so the served set is the filtered set and equality is the
    assertion — a subset check would pass an engine that resolved in the wrong layer and returned
    a strict subset, which is precisely the recolouring shape this module exists to catch.
    """
    stage = layers[state]
    assert stage.served[label] == _expected(stage, expr), f"{state} / {label}"


def test_the_folded_column_answers_exactly_what_the_layered_one_did(layers):
    """Folded against layered, on every probe at once (records §10's differential).

    The fold discards both extents' dictionaries and the base's, rebuilds one dictionary from the
    surviving values and renumbers every ordinal against it. Nothing about the *answers* may move:
    the same needles must select the same items. Asserted as a whole-matrix equality rather than
    per expression, because the failure this guards against — a renumbering that shifted a block of
    keys — moves several answers at once and is easiest to read as a diff of the whole set.
    """
    before = layers["base+A+B"]
    after = layers["folded"]
    assert after.served == before.served, (
        "the fold changed a keyword answer: "
        + ", ".join(
            label
            for label in before.served
            if before.served[label] != after.served[label]
        )
    )


# ---------------------------------------------------------------------------------------------
# The catalogue entries this module owns
# ---------------------------------------------------------------------------------------------


def test_a_value_one_layer_holds_and_another_does_not_is_resolved_per_layer(layers):
    """**A value present in one layer's dictionary and absent from another's** (records §10) —
    the shape a flush creates, and the one a cross-layer ordinal comparison breaks on.

    Four relations are checked at the two-extent state, and the entity *counts* are what make each
    of them a trap rather than a tautology:

    * `aaa-ingress-anchor` is the base's ordinal 0 and no extent holds it. An engine that resolved
      it in the base and then scanned the extents with ordinal 0 would return extent A's
      `hub-emea-d` row and extent B's `aab-early-relay` row as well — so the answer must be one
      entity, not three.
    * `aab-early-relay` is extent B's ordinal 0 and neither the base nor extent A holds it. The
      same confusion in the other direction would drag in the base's anchor.
    * `hub-emea-d` is in the base *and* in extent A, at ordinals 16 and 0. One resolve reused
      across both layers finds it in one and misses in the other, so the count is short by exactly
      the flushed row.
    * `relay-delta` is in extent A alone — absent from the base's dictionary and from extent B's,
      which is the flush-to-flush direction of the same entry.

    Each is compared against the oracle's own walk as well as against the count, so a coincidence
    that satisfied the arithmetic still has to satisfy the identity.
    """
    stage = layers["base+A+B"]

    for label, expr, size in [
        ("base only, at the base's ordinal 0", {"submitter": {"eq": cat.SUBMITTER_FIRST}}, 1),
        (
            "extent B only, at extent B's ordinal 0",
            {"submitter": {"eq": "aab-early-relay"}},
            1,
        ),
        ("extent A only", {"submitter": {"eq": "relay-delta"}}, 1),
        ("both extents, no base", {"submitter": {"eq": "relay-gamma"}}, 3),
    ]:
        served = stage.served[label]
        assert served == _expected(stage, expr), label
        assert len(served) == size, (
            f"{label}: {len(served)} entities, expected {size} — a needle resolved in one layer "
            "was scanned for in another, so the answer names items that carry a different value"
        )

    # The value both the base and extent A hold. Its size is not written out — the base's carriers
    # are a property of the corpus — but the relation is: the two-layer answer must be exactly the
    # base-only answer plus the one flushed row.
    shared = stage.served["the base and extent A, at different ordinals"]
    assert shared == _expected(stage, {"submitter": {"eq": "hub-emea-d"}})
    base_only = {
        stage.fx_of[i]
        for i in filt.evaluate(
            {"submitter": {"eq": "hub-emea-d"}},
            {"submitter": stage.column},
            {i for i in stage.candidate if i < cat.N_ITEMS},
        )
    }
    assert shared - base_only, (
        "`hub-emea-d` selected no ingested row, although extent A carries it — the needle was "
        "resolved against the base's dictionary and that ordinal names a different key in the "
        "extent"
    )
    assert base_only < shared


def test_a_prefix_range_empty_in_one_layer_and_not_in_the_next(layers):
    """**A prefix range empty in one layer and non-empty in the next** (records §10).

    A prefix is answered as a contiguous *ordinal range*, found by two searches in the layer's own
    dictionary. An empty range is the interesting one: it must select nothing in that layer while
    the same needle selects in another, and it must not degenerate into the whole range — a
    range whose bounds collapsed the wrong way returns every entity that carries a value.

    Three directions, each observed as a state that changes:

    * `relay-` is empty in the base and non-empty in both extents;
    * `beacon` is empty in the base *and* in extent A, and non-empty in extent B — so it selects
      nothing at the first state and something at the second, which is the entry in its literal
      form;
    * `node-emea-` is non-empty in the base and empty in both extents, and its answer must
      therefore not move at all when the extents arrive.
    """
    first, second = layers["base+A"], layers["base+A+B"]

    relay = "prefix empty in the base, not in either extent"
    assert first.served[relay] == _expected(first, dict(LAYER_EXPRESSIONS)[relay])
    assert second.served[relay] == _expected(second, dict(LAYER_EXPRESSIONS)[relay])
    assert len(first.served[relay]) == 3, first.served[relay]
    assert len(second.served[relay]) == 5, second.served[relay]

    beacon = "prefix empty in the base and A, not in B"
    assert first.served[beacon] == frozenset(), (
        "a prefix no layer holds yet selected something — an empty ordinal range is matching slots"
    )
    assert second.served[beacon] == _expected(second, dict(LAYER_EXPRESSIONS)[beacon])
    assert len(second.served[beacon]) == 1

    base_side = "prefix in the base, empty in both extents"
    assert first.served[base_side] == _expected(first, dict(LAYER_EXPRESSIONS)[base_side])
    assert first.served[base_side] == second.served[base_side], (
        "a prefix that is empty in every extent changed its answer when an extent arrived"
    )
    assert first.served[base_side], "the base side of this entry selects nothing — it is vacuous"


def test_the_base_s_ordinal_boundaries_survive_the_extents_and_the_fold(layers):
    """**The first and last value of a dictionary** (records §10), across the layering.

    Both anchors are carried by exactly one entity and neither extent holds either, so at every
    state the answer must be that one entity. The extents shift nothing about the base's
    dictionary; the fold rewrites it entirely, giving both anchors new ordinals — the first key of
    the folded dictionary is still `aaa-ingress-anchor` only because no extent introduced a smaller
    one, and the last is no longer `zzz-egress-anchor`'s old position at all.
    """
    for state in ("base+A", "base+A+B", "folded"):
        stage = layers[state]
        for label, value in [
            ("base only, at the base's ordinal 0", cat.SUBMITTER_FIRST),
            ("base only, at the base's last ordinal", cat.SUBMITTER_LAST),
        ]:
            served = stage.served[label]
            assert served == _expected(stage, {"submitter": {"eq": value}}), f"{state} / {label}"
            assert len(served) == 1, (
                f"{state}: `{value}` served {len(served)} entities, not the one that carries it — "
                "a dictionary boundary is off by one"
            )


def test_a_needle_no_layer_holds_answers_empty_at_every_state(layers):
    """**A needle absent from every dictionary** (records §10, §4.3): it must still answer, and
    answer empty — at one layer, at three, and after the fold.

    The scan runs regardless of whether the resolve found anything, which is what keeps *no item
    has this value* from being cheaper than *some do*. That is a work property and is deliberately
    not asserted here (surface §9's C11 row); what is asserted is that the answer exists, is empty,
    and does not become non-empty as layers accumulate — the failure mode being an unresolved
    needle that falls through to a sentinel ordinal some layer's slots actually hold.

    The positive control is the same column and the same principal at the same state: a needle
    every layer's dictionary does hold, which must select something everywhere.
    """
    for state in ("base+A", "base+A+B", "folded"):
        stage = layers[state]
        assert stage.served["a needle no layer holds"] == frozenset(), state
        assert stage.served["prefix every layer carries"], (
            f"{state}: the control selected nothing, so the empty answer above is a column that "
            "matches nothing at all"
        )
