"""Total verification at fixture size — correctness-suite §9, end to end, over a corpus-generated
bundle.

One walk (the module-scoped fixture), every test reading the same evidence: materialise the
corpus through its own Rust materialisers, build the bundle through the CLI, serve it, ingest one
batch drawn from the same functions beyond the built prefix, accept three denies, record the
battery, and take one census viewport per principal. The row half then checks every row of every
recorded response against `tessera corpus items`, and the census half checks per-tile masked
counts — the battery's own tiles and underlay surfaces included — against `tessera corpus
census`, minus the denies this harness had accepted.

## The plan's shape, and why

- **The ingested batch rides the walk** so the row half covers flushed rows as well as built ones
  — the seam the suite exists for is producers above the build — and so the census's two
  §9.2 obligations are both live: the expected `--n` is the built prefix plus the posted range
  (prefix-stability makes that the same corpus), and the recording waits on the background
  refresh, without which every count reads short by exactly that batch (decision 0044 D1).
- **The denies are one of each retirement story** (write-path §5.4): a suppression, a deletion,
  and a suppress/unsuppress pair. The first two are subtracted from every principal they were
  visible to; the third must be subtracted from none — an unsuppression restores at acceptance,
  and a census that still discounted it would be conflating the two removal rules.
- **Principals span the grant spectrum** — broad (every level-0 term), mid (half the level-2
  slots), narrow (one level-4 term) — because a census over one principal tests one mask; the
  masked counts are per-principal quantities and the corpus prices each grant differently by
  construction.

## The negative controls

Total verification that has never rejected anything is indistinguishable from a stub, so each
half's ability to fail is asserted against real recordings: a deliberately corrupted expectation
— one item's declared value, another item's position — must be rejected by the row half, naming
the rows; and a census computed for the wrong principal must be rejected by the census half. A
third control corrupts nothing and pins the wire defect the first run of this mechanism found:
the points tail's column names are the full declaration's first *k* rather than the render
declaration's, held as a strict xfail so the fix is noticed the day it lands.
"""

from __future__ import annotations

import base64
import dataclasses
import time
from types import SimpleNamespace

import pytest

from oracle import wire
from oracle.harness import spawn_server, stop_server

from .battery import Item, Viewport, build_battery, record, record_one
from .verification import (
    Declaration,
    TotalVerificationFailure,
    build_bundle,
    check_points,
    expected_census,
    expected_items,
    materialise_corpus,
    subtract_denies,
    terms_of,
    tile_visible,
    underlay_counts,
    verify_census,
    verify_rows,
    _streams_table,
)

SEED = 20260815
N = 4096
INGEST = (N, N + 64)
BBOX = (0.0, 0.0, 65536.0, 65536.0)
SLICE_ID = "s0"
#: Saturation (§10's precondition, inherited): above every visible total, so the row half sees
#: every visible row and a denied row's absence is the corpus's, not the selection's.
K = 200_000
#: The census half's fixed depth: 256 tiles over the extent — fine enough to localise, small
#: enough that the comparison is readable when it fails.
CENSUS_DEPTH = 4

#: The grant spectrum (module doc). Term ids are `level·64 + slot` (the corpus's term space).
PRINCIPALS = {
    "broad": tuple(range(0, 64)),
    "mid": tuple(range(128, 160)),
    "narrow": (263,),
}
RECORDING_PRINCIPAL = "broad"


@pytest.fixture(scope="module")
def run(tmp_path_factory) -> SimpleNamespace:
    work = tmp_path_factory.mktemp("total-verification")
    files = materialise_corpus(SEED, N, work / "corpus", ingest=INGEST)
    declaration = Declaration.load(files.schema)
    bundle_root = work / "bundle"
    build_bundle(files, bundle_root, slice_id=SLICE_ID)

    server, proc = spawn_server(bundle_root, work)
    try:
        token = server.authorise([str(t) for t in PRINCIPALS[RECORDING_PRINCIPAL]])["token"]
        meta = server.meta(token)

        # The establishing viewport: served ids for the battery, and the tessera→fx join every
        # deny and every drill-down expectation needs (`tessera_id` is minted per build and never
        # persisted; `fx_key` is the identity that survives).
        raw = server.viewport(token, SLICE_ID, 3, BBOX, k=K, underlay_offset=2)
        points = wire.decode_viewport_points(raw)
        fx_of_tessera = dict(
            zip(
                points.column("tessera_id").to_pylist(),
                points.column("fx_key").to_pylist(),
            )
        )
        item_ids = tuple(sorted(fx_of_tessera)[:3])
        battery = build_battery(
            meta,
            slice_id=SLICE_ID,
            item_ids=item_ids,
            bbox=BBOX,
            zooms=(0, 3),
            k=K,
            underlay_offset=2,
            filters={"bay": {"eq": "cedar"}},
        )

        # Ingest the corpus's own wire batch, then barrier on the background refresh — the
        # census's second §9.2 obligation (decision 0044 D1). Sound while this is the only
        # resident session, which is why the other principals authorise afterwards.
        resp = server.ingest(files.ingest.read_bytes(), "total-verification-1")
        assert resp.status_code == 200, resp.text
        assert resp.json()["accepted"] == INGEST[1] - INGEST[0]
        refreshes_before = server.status()["write_executor"]["flush"]["refreshes"]
        server.flush()
        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline:
            if server.status()["write_executor"]["flush"]["refreshes"] > refreshes_before:
                break
            time.sleep(0.05)
        else:
            raise TimeoutError("the background refresh never replaced the resident projection")

        # The denies: suppress, delete, and suppress/unsuppress (module doc). Targets are battery
        # items, addressed by the fixture's external-id convention — the source id's
        # little-endian bytes, which is both what the build minted and what the ingest batch
        # carried.
        battery_fx = [fx_of_tessera[t] for t in item_ids]
        battery_expected = expected_items(SEED, battery_fx)

        def change(op: str, fx: int) -> None:
            e = battery_expected[fx].e
            external = base64.b64encode(e.to_bytes(8, "little")).decode()
            resp = server.change(external, op)
            assert resp.status_code == 200, f"{op} refused: {resp.text}"

        change("suppress", battery_fx[0])
        change("delete", battery_fx[1])
        change("suppress", battery_fx[2])
        change("unsuppress", battery_fx[2])
        denied_fx = frozenset(battery_fx[:2])
        denied = [battery_expected[fx] for fx in sorted(denied_fx)]
        denied_terms = terms_of(files, [item.e for item in denied])

        recorded = record(server, token, battery)

        # One census viewport per principal at the fixed depth. Fresh sessions for the
        # non-recording principals: authorised after the refresh barrier, they materialise
        # against the current state and need no barrier of their own.
        censuses = {}
        for name, grant in PRINCIPALS.items():
            grant_token = (
                token
                if name == RECORDING_PRINCIPAL
                else server.authorise([str(t) for t in grant])["token"]
            )
            canon = record_one(
                server, grant_token, Viewport(SLICE_ID, CENSUS_DEPTH, bbox=BBOX, k=2)
            )
            censuses[name] = tile_visible(canon)

        yield SimpleNamespace(
            files=files,
            declaration=declaration,
            recorded=recorded,
            fx_of_tessera=fx_of_tessera,
            item_ids=item_ids,
            battery_fx=battery_fx,
            denied_fx=denied_fx,
            denied=denied,
            denied_terms=denied_terms,
            censuses=censuses,
        )
    finally:
        stop_server(proc)


def _expected_census_for(run, grant: tuple[int, ...], depth: int) -> dict[int, int]:
    """The generator's count at the harness's own n_total, minus the harness's own denies —
    §9.2's expected side, assembled the way the design states it."""
    census = expected_census(SEED, run.files.n_total, depth, grant)
    return subtract_denies(census, run.denied, run.denied_terms, grant, depth)


# ---------------------------------------------------------------------------------------------
# The two halves
# ---------------------------------------------------------------------------------------------


def test_the_row_half_verifies_every_row_of_every_response(run):
    """§9.1: every served row compared at its own identity — code, coordinates and every declared
    field — with one expectation call per response. The floor guards against vacuity: two
    unfiltered full-extent viewports alone carry thousands of rows at this corpus size."""
    rows = verify_rows(
        run.recorded,
        seed=SEED,
        declaration=run.declaration,
        denied_fx=run.denied_fx,
        fx_of_tessera=run.fx_of_tessera,
    )
    assert rows > 4_000, f"only {rows} rows verified — the battery has stopped serving points"


def test_the_census_half_agrees_for_every_principal(run):
    """§9.2: per-tile masked counts at the fixed depth, per principal, against the generator —
    n_total for the corpus the harness fed the server, minus the denies it had accepted."""
    for name, grant in PRINCIPALS.items():
        compared = verify_census(
            run.censuses[name], _expected_census_for(run, grant, CENSUS_DEPTH), label=name
        )
        assert compared > 0 or name == "narrow", f"{name}: the census compared nothing"
    assert sum(run.censuses["broad"].values()) > 0, "the broad principal saw an empty corpus"


def test_the_recorded_count_surfaces_agree_with_the_census(run):
    """The battery's own tiles and underlay surfaces, against the same expected side: the
    underlay is the tiles claim at depth `zoom + underlay_offset` (§3 — the only other derived
    aggregate), and both are mask-level counts, so the filtered viewport is held to the same
    expectation as the unfiltered ones (I3/I12: filters move `matched`, never `visible`)."""
    grant = PRINCIPALS[RECORDING_PRINCIPAL]
    for query, canon in run.recorded.items():
        if not isinstance(query, Viewport):
            continue
        label = f"viewport(zoom={query.zoom}, filters={'yes' if query.filters else 'no'})"
        verify_census(
            tile_visible(canon),
            _expected_census_for(run, grant, query.zoom),
            label=f"{label} tiles",
        )
        if query.underlay_offset:
            depth = query.zoom + query.underlay_offset
            verify_census(
                underlay_counts(canon),
                _expected_census_for(run, grant, depth),
                label=f"{label} underlay at depth {depth}",
            )


def test_the_deny_lane_is_where_the_recording_says_it_is(run):
    """The three denies landed as three different stories: the suppressed and deleted items'
    drill-downs answer 404 and are subtracted from every census; the suppress/unsuppress pair
    left no residue — its item answers 200 and the censuses that just passed did not subtract it.
    Write-path §5.4's two removal rules, kept apart on the served surface."""
    statuses = {
        run.fx_of_tessera[q.tessera_id]: canon.payload["status"]
        for q, canon in run.recorded.items()
        if isinstance(q, Item)
    }
    suppressed, deleted, unsuppressed = run.battery_fx
    assert statuses[suppressed] == 404
    assert statuses[deleted] == 404
    assert statuses[unsuppressed] == 200
    assert unsuppressed not in run.denied_fx, (
        "an unsuppressed item held in the denied set would be subtracted from the census — "
        "conflating the two removal rules"
    )


# ---------------------------------------------------------------------------------------------
# The negative controls
# ---------------------------------------------------------------------------------------------


def test_negative_control_a_corrupted_expectation_is_rejected(run):
    """The row half against a deliberately corrupted expectation: one item's declared value moved
    by one, another item's position moved by one unit. Both must be rejected, each at its own
    row — a row half that accepted either would be comparing nothing (§8.1)."""
    query = next(
        q
        for q in run.recorded
        if isinstance(q, Viewport) and q.zoom == 3 and q.filters is None
    )
    canon = run.recorded[query]
    served_fx = _streams_table(canon.points).column("fx_key").to_pylist()
    expected = expected_items(SEED, set(served_fx))

    live = [fx for fx in served_fx if fx not in run.denied_fx]
    value_fx, position_fx = live[0], live[-1]
    tampered = dict(expected)
    victim = tampered[value_fx]
    tampered[value_fx] = dataclasses.replace(
        victim, fields={**victim.fields, "weight": (victim.fields["weight"] or 0) + 1}
    )
    victim = tampered[position_fx]
    tampered[position_fx] = dataclasses.replace(victim, x=victim.x + 1.0)

    reasons: list[str] = []
    check_points(
        "control",
        canon,
        tampered,
        declaration=run.declaration,
        denied_fx=run.denied_fx,
        reasons=reasons,
    )
    assert any("weight" in r and format(value_fx, "#x") in r for r in reasons), (
        f"the corrupted value was not rejected: {reasons[:3]}"
    )
    assert any("code" in r and format(position_fx, "#x") in r for r in reasons), (
        f"the corrupted position was not rejected: {reasons[:3]}"
    )
    assert len(reasons) == 2, f"unexpected extra disagreements: {reasons}"


def test_negative_control_a_census_for_the_wrong_principal_is_rejected(run):
    """The census half against the wrong principal's expectation: broad's served counts held to
    mid's computed ones must disagree — a comparator that passed this would pass any census."""
    assert sum(run.censuses["broad"].values()) > 0
    with pytest.raises(TotalVerificationFailure):
        verify_census(
            run.censuses["broad"],
            _expected_census_for(run, PRINCIPALS["mid"], CENSUS_DEPTH),
            label="wrong principal",
        )


@pytest.mark.xfail(
    strict=True,
    reason="the points tail is labelled with the full declaration's first k names rather than "
    "the render declaration's: the engine hands the serialiser the whole compiled schema "
    "(ViewportHead.declared_scalars) while the gather narrows to render columns, so this "
    "corpus's `bay` codes arrive under the name `seen_at`. Found by total verification's "
    "first run; the row half reads the tail positionally, which is correct before and "
    "after the fix. When this xpasses, delete the marker.",
)
def test_the_points_tail_is_named_by_its_render_declaration(run):
    query = next(
        q
        for q in run.recorded
        if isinstance(q, Viewport) and q.zoom == 3 and q.filters is None
    )
    table = _streams_table(run.recorded[query].points)
    render_names = [c.name for c in run.declaration.render_columns()]
    assert table.schema.names == ["tessera_id", "code", *render_names]
