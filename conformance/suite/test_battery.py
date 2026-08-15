"""The battery, recorded against a live server — membership, the marked absence, and the
determinism the whole suite rests on.

The load-bearing test here is the last one: recording the battery twice against unchanged state
compares equal, per query and per surface. Every mechanism the correctness suite specifies —
stage invariance most directly — is a comparison of recordings, so if two recordings of one
battery against one state could legitimately differ, every disagreement the suite ever reported
would be arguable. Byte determinism at a fixed configuration is a documented implementation
detail rather than a guarantee (design §10.4, decision 0030); the suite is entitled to lean on it
because the harness pins its own configuration, and this test is where a break in it surfaces
first — by design, before it surfaces as an unreproducible stage-invariance flake.

The server is the shared catalogue server (`conformance/conftest.py`), because the battery needs
a corpus that actually has category columns, filterable values and drill-down records — and
because one designed corpus shared across modules is that conftest's stated reason to exist.
"""

from __future__ import annotations

import pytest
import requests

from oracle import catalogue as cat
from oracle import wire

from .battery import (
    Absent,
    Categories,
    Item,
    Meta,
    Region,
    Viewport,
    build_battery,
    record,
    record_one,
)
from .canonical import Json, Streamed

BBOX = (0.0, 0.0, 65536.0, 65536.0)
K = 64
ZOOMS = (0, 3)


@pytest.fixture(scope="module")
def battery_token(catalogue_server) -> str:
    """One mid-coverage principal (10%) — enough visibility that every surface has content."""
    case = next(c for c in cat.catalogue() if c.name == "crossover_above")
    return catalogue_server.authorise(list(case.grants))["token"]


@pytest.fixture(scope="module")
def catalogue_battery(catalogue_server, battery_token):
    """The battery for the catalogue deployment, plus one tiles-form viewport.

    Item ids come from a served response, never from the fixture's entity ids: a `tessera_id` is
    a keyed permutation minted per build, and nothing may persist one across runs
    (`oracle.harness.fixture_recipe`'s note). The appended tiles-form query pins the recorder
    against contracts §3.2's second request form; `build_battery` itself emits the bbox form.
    """
    meta = catalogue_server.meta(battery_token)
    raw = catalogue_server.viewport(battery_token, cat.SLICE_ID, 3, BBOX, k=K)
    _tiles, points = wire.decode_viewport(raw)
    item_ids = sorted({tessera_id for tessera_id, _code in points})[:3]
    battery = build_battery(
        meta,
        slice_id=cat.SLICE_ID,
        item_ids=item_ids,
        bbox=BBOX,
        zooms=ZOOMS,
        k=K,
        underlay_offset=2,
        filters={"department": {"eq": sorted(cat.DEPARTMENT_CODES)[0]}},
    )
    tiles_form = Viewport(
        cat.SLICE_ID, 1, tiles=(0, 1, 2, 3), k=K, underlay_offset=2
    )
    return battery + (tiles_form,)


@pytest.fixture(scope="module")
def first_recording(catalogue_server, battery_token, catalogue_battery):
    return record(catalogue_server, battery_token, catalogue_battery)


def test_the_battery_covers_every_surface_the_design_names(catalogue_battery):
    """Correctness-suite §3's table, transcribed: schema, vocabulary, the three viewport
    surfaces, region, drill-down. A battery member per surface — and the underlay *requested* on
    every viewport, since an unrequested underlay silently drops out of every comparison."""
    by_kind = {}
    for entry in catalogue_battery:
        by_kind.setdefault(type(entry), []).append(entry)

    assert len(by_kind.get(Meta, [])) == 1
    assert by_kind.get(Categories), "no vocabulary surface — the catalogue declares categories"
    viewports = by_kind.get(Viewport, [])
    assert viewports, "no viewport queries"
    assert all(v.underlay_offset >= 1 for v in viewports)
    assert any(v.filters is not None for v in viewports), "the filtered surface is uncovered"
    assert any(v.tiles is not None for v in viewports), "the tiles request form is uncovered"
    assert by_kind.get(Item), "no drill-down — the only reader of all three homes (§3)"

    absences = by_kind.get(Absent, [])
    assert len(absences) == 1 and isinstance(absences[0].query, Region), (
        "/v1/region must ride the battery as exactly one marked absence — neither silently "
        "omitted nor duplicated"
    )


def test_region_is_still_a_true_absence(catalogue_server, battery_token, catalogue_battery):
    """The marker is only honest while the route is missing. The day `/v1/region` lands, this
    fails, and whoever reads it promotes the `Absent` entry to a live query and writes the
    `Batches` canonicalisation (`suite.canonical.Batches`'s own doc says the same)."""
    resp = requests.post(
        f"{catalogue_server.viewer_base}/v1/region",
        headers={"Authorization": f"Bearer {battery_token}"},
        json={"slice": cat.SLICE_ID, "bbox": list(BBOX)},
        timeout=10,
    )
    assert resp.status_code == 404, (
        f"/v1/region answered {resp.status_code} — the route exists now; promote the battery's "
        f"Absent marker to a live Region query"
    )

    absent = next(e for e in catalogue_battery if isinstance(e, Absent))
    with pytest.raises(NotImplementedError):
        record_one(catalogue_server, battery_token, absent.query)


def test_every_recorded_surface_carries_content(first_recording, catalogue_battery):
    """Not merely recorded but non-vacuous — a battery whose surfaces came back empty would
    compare equal for ever while covering nothing."""
    for query, canonical in first_recording.items():
        if isinstance(query, Viewport):
            assert isinstance(canonical, Streamed)
            assert canonical.tiles, f"{query}: empty tiles surface"
            assert canonical.points, f"{query}: empty points surface"
            # Requested on every battery viewport, so present even when no cell qualifies.
            assert canonical.underlay, f"{query}: empty underlay surface"
            assert canonical.trailer, f"{query}: empty trailer remainder"
        elif isinstance(query, Item):
            assert isinstance(canonical, Json)
            assert canonical.payload["status"] == 200, (
                f"{query} was recorded from a served point, so it must be visible"
            )
            assert canonical.payload["body"]["fields"], f"{query}: drill-down returned no fields"
        elif isinstance(query, Categories):
            assert isinstance(canonical, Json)
            assert canonical.payload["pages"], f"{query}: no pages recorded"
        elif isinstance(query, Meta):
            assert isinstance(canonical, Json)
            assert canonical.payload["declared_scalars"]
    absent_queries = {e.query for e in catalogue_battery if isinstance(e, Absent)}
    assert absent_queries.isdisjoint(first_recording), "an Absent entry was recorded"


def test_recording_the_battery_twice_against_unchanged_state_compares_equal(
    catalogue_server, battery_token, catalogue_battery, first_recording
):
    """The property the suite rests on (module doc). Compared per query and, for streamed
    responses, per surface, so a failure names where determinism broke rather than that it did."""
    second = record(catalogue_server, battery_token, catalogue_battery)
    assert set(first_recording) == set(second)
    for query, a in first_recording.items():
        b = second[query]
        if isinstance(a, Streamed):
            differing = [
                name for name, bytes_ in a.surfaces().items() if bytes_ != b.surfaces()[name]
            ]
            assert not differing, (
                f"{query}: two issues against unchanged state differ on {differing} — either "
                f"canonicalisation missed a source of legitimate variation, or response "
                f"determinism at this pinned configuration (decision 0030) has broken"
            )
        else:
            assert a == b, f"{query}: two issues against unchanged state differ"
    assert first_recording == second
