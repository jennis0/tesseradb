"""Stage invariance at fixture size — correctness-suite §10 over §12.3's driver, end to end.

One plan, walked once (the module-scoped fixture), every stage judged from the same recorded
evidence: the battery before, the battery after, and the delta between them compared against the
stage's entitlement. Eight writes, two merges, a coalesce, the three deny ops, a WAL rotation,
two reloads and a fold — every §2 stage class the suite can currently drive.

## Why the writes are counted the way they are

A pulled tick dispatches *everything* eligible (§12.3), and the driver's server runs at the
default ladder widths (`tier_width` 4 and `coalesce_width` 8, which its config does not set), so
isolation is arithmetic:

- flush extents and their merged outputs all clamp to the 16 MiB floor tier, and a merge selects
  from them only, never the base segment — so merge eligibility is simply
  "four of them exist". Four writes, then the merge tick; three more (the merged segment counts as
  one), then the second merge tick. No write's own tick ever sees four, because the extent that
  write publishes is not yet published when its tick plans.
- every flush appends one entry per entity-space axis and nothing before the coalesce consumes
  them, so after the eighth write — and only then — the delta-tier axis is at the width. The
  coalesce tick follows the eighth write; at that tick the segment ladder holds three (base,
  second merge, eighth extent), below the merge width.
- rotation needs WAL growth since the last flush publication's own rotation, which is what the
  three denies provide; its tick dispatches nothing else (one delta tier, three segments).

The tick stages' barriers assert the flush counter did not move, so if this arithmetic ever
drifts from the engine's, the failure names the plan rather than mis-attributing a delta.

## The negative controls

A stage-invariance suite that has never rejected anything proves nothing, so the checker's
ability to fail is asserted alongside the claims it passes: a flush claiming `Nothing` is
rejected (the strongest claim, made falsely, against real recordings); a merge claiming `Rows` is
rejected; an undeclared deny is rejected; and a recording whose points surface was tampered —
one served row dropped while every count surface still claims it — is `Unexplained`, which no
entitlement equals. The last is §8.1's argument made executable: the corruption a count cannot
see is exactly what the row surfaces must catch.

## What this module fixes about the corpus

Deny targets are battery items (so the drill-down surface flips are exercised in both
directions), resolved to the fixture's external ids through the served `fx_key` join — the
catalogue's entity id *is* its source id, and its external id is that id's little-endian bytes.
Ingested rows use established vocabulary values only (`/v1/categories` must not move — the diff
treats a vocabulary change as unexplained, and this plan is why it can), carry an access term the
battery principal already holds, and land inside the extent, so the flush's entitlement is
observable in full.
"""

from __future__ import annotations

import base64
import dataclasses
import io
from typing import NamedTuple

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from oracle import catalogue as cat

from .battery import Viewport
from .driver import (
    Build,
    Coalesce,
    Deny,
    Fold,
    Load,
    Merge,
    Rotate,
    StageInvarianceViolation,
    StageResult,
    SuiteHarness,
    Write,
    check,
    run_plan,
)
from .entitlement import Nothing, Rows, Unexplained, _tables, diff

BBOX = (0.0, 0.0, 65536.0, 65536.0)
#: Above the corpus total plus everything the plan ingests, so at this fixture size every tile is
#: saturated (served == visible, observed per response — §10) and every diff below is the exact
#: membership comparison rather than the capped fallback.
K = 200_000
#: The mid-coverage principal: 10%, two grants — enough visibility that every surface has content.
GRANTS = tuple(next(c for c in cat.catalogue() if c.name == "crossover_above").grants)
#: Ingested rows carry a term the principal already holds, so no new descriptor is minted and the
#: batch is visible to the established session the moment the refresh lands.
INGEST_ACCESS = cat.BLOCKS["cross_lo"].descriptor
#: An established department key (the battery's filter key, so the filtered surface moves too).
FILTER_DEPARTMENT = sorted(cat.DEPARTMENT_CODES)[0]

#: fx_key -> source id for every planted item — how a served row is traced back to the external
#: id the deny lane addresses.
_SOURCE_OF_FX = {fx: source for source, fx in enumerate(cat.fx_keys())}


class IngestItem(NamedTuple):
    external_id: int
    x: float
    y: float
    fx_key: int
    title: str
    submitter: str
    abstract: str
    note: str
    pages: int


def _ingest_body(items: list[IngestItem]) -> bytes:
    """One Arrow IPC stream in the wire shape `/control/ingest` takes.

    The schema is the catalogue's declaration, and every declared column must be present
    (contracts §2.2): the scalar tail is read back positionally, so an omitted column shifts
    every later scalar rather than defaulting. Category values are established keys — the
    vocabulary must not move under this plan (module doc).
    """
    schema = pa.schema(
        [
            pa.field("external_id", pa.binary()),
            pa.field("x", pa.float32()),
            pa.field("y", pa.float32()),
            # One list of labels per row, each element one label verbatim (contracts §3.4).
            pa.field("access", pa.list_(pa.utf8())),
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
            pa.array([i.external_id.to_bytes(8, "little") for i in items], pa.binary()),
            pa.array([i.x for i in items], pa.float32()),
            pa.array([i.y for i in items], pa.float32()),
            pa.array([[INGEST_ACCESS]] * len(items), pa.list_(pa.utf8())),
            pa.array([i.fx_key for i in items], pa.uint64()),
            pa.array([FILTER_DEPARTMENT] * len(items), pa.utf8()),
            pa.array([sorted(cat.ARCHIVE_CODES)[0]] * len(items), pa.utf8()),
            pa.array([i.title for i in items], pa.utf8()),
            pa.array([i.submitter for i in items], pa.utf8()),
            pa.array([sorted(cat.SHELF_CODES)[0]] * len(items), pa.utf8()),
            pa.array([i.abstract for i in items], pa.utf8()),
            pa.array([i.note for i in items], pa.utf8()),
            pa.array([i.pages for i in items], pa.uint32()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


def _write_stage(index: int, fx_pair: list[int]) -> Write:
    items = [
        IngestItem(
            external_id=910_000_000 + index * 100 + i,
            # Spread across the extent, away from its edges; nothing about the checker depends
            # on where they land, only that they are inside every battery viewport.
            x=137.0 + 977.0 * (index * 2 + i),
            y=251.0 + 631.0 * (index * 2 + i),
            fx_key=fx,
            title=f"ingested title {index}-{i}",
            submitter=f"relay-{index}-{i}",
            abstract=f"an ingested abstract {index} {i}",
            note=f"ingested-note-{index}-{i}",
            pages=100 + index * 2 + i,
        )
        for i, fx in enumerate(fx_pair)
    ]
    return Write(
        label=f"write-{index + 1}",
        body=_ingest_body(items),
        batch_id=f"stage-invariance-{index + 1}",
        fx_keys=fx_pair,
    )


def _battery_item(index: int):
    """Resolve the index-th battery item to (external_id_b64, fx) at apply time — the battery,
    and therefore the item list, does not exist when the plan is written down."""

    def pick(h: SuiteHarness) -> tuple[str, int]:
        tessera_id = h.item_ids[index]
        fx = h.fx_by_tessera[tessera_id]
        source = _SOURCE_OF_FX[fx]
        return base64.b64encode(source.to_bytes(8, "little")).decode(), fx

    return pick


#: The labels of every checked stage, in plan order. A literal list rather than a derivation from
#: the plan, so a stage silently dropped from the plan fails the coverage test instead of
#: shrinking it.
CHECKED_LABELS = [
    "write-1",
    "write-2",
    "write-3",
    "write-4",
    "merge-1",
    "write-5",
    "write-6",
    "write-7",
    "merge-2",
    "write-8",
    "coalesce",
    "suppress",
    "unsuppress",
    "delete",
    "rotate",
    "reload-live",
    "fold",
    "reload-folded",
]


@pytest.fixture(scope="module")
def plan_results(tmp_path_factory, private_catalogue_bundle) -> dict[str, StageResult]:
    """Walk the full plan once; every test reads the same evidence.

    A private bundle copy, because flushes, denies and the fold all publish *into the bundle
    prefix* (`conformance/conftest.py`'s private-copy rationale), and this module publishes more
    than any other.
    """
    fx = cat.ingest_fx_keys(16)
    h = SuiteHarness(
        bundle_root=private_catalogue_bundle("stage-invariance"),
        run_dir=tmp_path_factory.mktemp("stage-invariance-run"),
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
        filters={"department": {"eq": FILTER_DEPARTMENT}},
    )
    plan = [
        Build(),
        _write_stage(0, fx[0:2]),
        _write_stage(1, fx[2:4]),
        _write_stage(2, fx[4:6]),
        _write_stage(3, fx[6:8]),
        Merge("merge-1"),
        _write_stage(4, fx[8:10]),
        _write_stage(5, fx[10:12]),
        _write_stage(6, fx[12:14]),
        Merge("merge-2"),
        _write_stage(7, fx[14:16]),
        Coalesce("coalesce"),
        Deny("suppress", _battery_item(0)),
        Deny("unsuppress", _battery_item(0)),
        Deny("delete", _battery_item(1)),
        Rotate(),
        Load("reload-live"),
        Fold(),
        Load("reload-folded"),
    ]
    try:
        results = run_plan(h, plan)
        yield {r.label: r for r in results}
    finally:
        h.stop()


def test_the_plan_walked_every_stage_it_promised(plan_results):
    assert set(plan_results) == {"build", *CHECKED_LABELS}


def test_the_plan_covers_every_stage_class_the_design_names(plan_results):
    """§2's eight stages, transcribed: build, load, write, merge, deny, fold — and the two a
    harness forgets, the entity-space coalesce and rotation. (§10.1's kill modifier is absent
    *here* by scope, not by blocker — decision 0071 ruled and it is driven over the same stages in
    `test_crash_atomicity`. This plan asserts what a stage changes; that one asserts what a stage
    killed part-way leaves behind.)"""
    classes = {type(r.stage) for r in plan_results.values()}
    assert {Build, Load, Write, Merge, Coalesce, Deny, Fold, Rotate} <= classes
    ops = {r.stage.op for r in plan_results.values() if isinstance(r.stage, Deny)}
    assert ops == {"suppress", "unsuppress", "delete"}, (
        "the deny lane's three ops are three different retirement stories (write-path §5.4) and "
        "each must ride the plan"
    )


@pytest.mark.parametrize("label", CHECKED_LABELS)
def test_the_stage_changed_exactly_what_it_was_entitled_to(plan_results, label):
    """The suite's whole claim, one stage per case: `diff(before, after) == entitlement`.

    `Nothing` — bytes-equal on every canonical surface — for both merges, the coalesce, the
    rotation, both reloads and the fold; exactly the named entity for each deny; exactly the
    ingested rows for each write's flush.
    """
    check(plan_results[label])


# -- negative controls -------------------------------------------------------------------------
#
# Each control takes *real* recordings from the plan and shows the checker rejecting a false
# claim about them. A checker only ever seen agreeing has proven nothing about its ability to
# disagree.


def test_negative_control_a_flush_claiming_nothing_is_rejected(plan_results):
    """`Nothing` is the strongest claim in the suite; made falsely about the one stage entitled
    to change answers, it must be rejected — this is §10's judgement-call row (flush's delta
    entitlement) enforced from the other side."""
    with pytest.raises(StageInvarianceViolation):
        check(plan_results["write-1"], claimed=Nothing())


def test_negative_control_a_merge_claiming_rows_is_rejected(plan_results):
    """A merge that claimed a flush's entitlement must be rejected: its recordings are
    bytes-identical, and `Delta(∅, ∅)` equals no `Rows` claim."""
    with pytest.raises(StageInvarianceViolation):
        check(plan_results["merge-1"], claimed=Rows(cat.ingest_fx_keys(2)))


def test_negative_control_an_undeclared_deny_is_rejected(plan_results):
    """A suppress claiming it changed nothing is the fail-open shape — an acceptance that moved
    the served surface while declaring itself invisible — and must be rejected."""
    with pytest.raises(StageInvarianceViolation):
        check(plan_results["suppress"], claimed=Nothing())


def test_negative_control_a_tampered_points_surface_is_unexplained(plan_results):
    """Drop one served row from a points surface while every count still claims it: the diff
    must refuse to classify the change as any entitlement (§8.1 — the corruption a count cannot
    see is what the row surface exists to catch)."""
    result = plan_results["merge-1"]
    query = next(
        q
        for q in result.after
        if isinstance(q, Viewport) and q.zoom == 0 and q.filters is None and q.bbox is not None
    )
    canon = result.after[query]
    table = _tables(canon.points)
    trimmed = table.slice(0, table.num_rows - 1).combine_chunks()
    sink = io.BytesIO()
    with ipc.new_stream(sink, trimmed.schema) as writer:
        for batch in trimmed.to_batches():
            writer.write_batch(batch)
    tampered_after = dict(result.after)
    tampered_after[query] = dataclasses.replace(canon, points=sink.getvalue())

    delta = diff(result.before, tampered_after)
    assert isinstance(delta, Unexplained), (
        f"a dropped served row classified as {delta!r} — the diff laundered a corruption into "
        f"an entitlement"
    )
    forged = StageResult(result.label, result.stage, result.before, tampered_after, delta)
    with pytest.raises(StageInvarianceViolation):
        check(forged)
