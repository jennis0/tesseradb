"""**`POST /v1/aggregate`** — the counting route, driven over HTTP against `oracle.aggregate`.

Every table the server serves is compared, row for row, with the oracle's literal counts: the
set and the reference evaluated by `oracle.filters` inside the principal's visible set, each
entity's value as the fixture planted it, and each entity's position recomputed from the source
geometry by `oracle.bundle`, never read from the stored columns.

What this module covers:

- **Values**, over the mask catalogue: a `derived` indexed field (`department`), a `public`
  indexed one (`archive`) and a `public` drawn one (`shelf`), by `top` and by a named list that
  holds a hidden value, a declared value with no item and a key that does not exist; with `rest`
  and `none`; under three principals of very different coverage, with no filter, a category
  filter and a region, and with a reference of the whole visible set.
- **Cells**, alone and inside a value grouping, at depths 0, 3, 7 and 10 over the whole view and
  12 and 20 over an area, which lists only its own cells, including tables long enough to be read
  through the cursor.
- **The cell limit**: the whole view at depth 11 is refused.
- **Artifacts**, over the spatial-layer fixture of `test_member_of.py`: two flat spatial layers,
  by `top` and by a named list holding an id that names
  nothing, under that fixture's three principals and a numeric filter.
- **The size of the set** equals the viewport's matched count.
- **A field that cannot be counted** (a `keyword`) is refused with `422`.

- **Overlapping and withheld artifacts**, over `oracle.artifact_label_fixture`: a tree whose
  artifacts overlap, withheld by their own label, by a label no point carries, by the membership
  requirement and by the layer's default label, under that fixture's six principals, so `rest` and
  `none` count only what each principal is served.

What it does not cover, so the gap is stated: a suppression between pages, and timing.
"""

from __future__ import annotations

import io
import math

import pyarrow.ipc as ipc
import pytest

from oracle import aggregate as agg
from oracle import artifact_label_fixture as lfx
from oracle import catalogue as cat
from oracle import filters as filt
from oracle import morton
from oracle.filters import NumericColumn, RegionColumn
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport, decode_viewport_artifacts, split_aggregate_frames

from test_member_of import member_server  # noqa: F401  (a fixture this module shares)
from test_shape_membership import (
    BOXES,
    BOXES_DECLARED,
    POLYGONS,
    PRINCIPALS,
    SHAPES,
    VIEW_ID as SHAPES_VIEW,
    WHOLE_MAP,
    containing,
    q,
    visible,
)

CASES = ["full_100pct", "crossover_above", "sparse_0_01pct"]

REGION = {"region": {"bbox": [0.0, 0.0, 30000.0, 65536.0]}}
FILTERS = {
    "none": None,
    "category": {"archive": {"in": ["red", "blue"]}},
    "region": REGION,
}

GROUPINGS = [
    {},
    {"by": {"field": "department", "top": 3}},
    {"by": {"field": "department", "values": ["gamma", "omega", "hollow", "nope", "alpha", "gamma"]}},
    {"by": {"field": "archive", "top": 2}, "cells": {"depth": 3}},
    {"by": {"field": "archive", "values": ["void", "green"]}},
    {"by": {"field": "shelf", "top": 5}, "cells": {"depth": 7}},
    {"cells": {"depth": 0}},
    {"cells": {"depth": 10}},
    {"cells": {"depth": 12, "area": [0.0, 0.0, 16383.0, 16383.0]}},
    {"by": {"field": "archive", "top": 2}, "cells": {"depth": 12, "area": [8000.0, 3000.0, 20000.0, 9000.0]}},
    {"cells": {"depth": 20, "area": [30000.0, 30000.0, 30040.0, 30040.0]}},
]


def read_tables(server, token: str, body: dict) -> tuple[list[dict], list[list[agg.Row]]]:
    """Every table of a whole read, carried across responses by the trailer's cursor: each
    table's first head and its rows. A table continued from a cursor must say so."""
    heads: dict[int, dict] = {}
    rows: dict[int, list[agg.Row]] = {}
    body = dict(body)
    for _ in range(10_000):
        resp = server.aggregate(token, **body)
        assert resp.status_code == 200, resp.text
        decoded = split_aggregate_frames(resp.content)
        for head, pages in decoded.tables:
            grouping = head["grouping"]
            assert head["resumed"] == (grouping in heads), head
            heads.setdefault(grouping, head)
            for records, _end in pages:
                batch = ipc.open_stream(io.BytesIO(records)).read_next_batch()
                rows.setdefault(grouping, []).extend(_rows(batch))
        if decoded.trailer["next"] is None:
            count = len(body["groupings"])
            return [heads[g] for g in range(count)], [rows.get(g, []) for g in range(count)]
        body["cursor"] = decoded.trailer["next"]
    raise AssertionError("a read that never ends")


def _rows(batch) -> list[agg.Row]:
    columns = {name: batch.column(name).to_pylist() for name in batch.schema.names}
    n = batch.num_rows
    blank = [None] * n
    return [
        agg.Row(
            group=columns.get("group", blank)[i],
            key=columns.get("key", blank)[i],
            cell=columns.get("cell", blank)[i],
            count=columns["count"][i],
            reference_count=columns.get("reference_count", blank)[i],
            lift=columns.get("lift", blank)[i],
        )
        for i in range(n)
    ]


def assert_rows_equal(got: list[agg.Row], expected: list[agg.Row], what: str) -> None:
    assert len(got) == len(expected), f"{what}: {len(got)} rows served, {len(expected)} expected"
    for index, (a, b) in enumerate(zip(got, expected)):
        same_lift = (a.lift is None and b.lift is None) or (
            a.lift is not None and b.lift is not None and math.isclose(a.lift, b.lift, rel_tol=1e-12)
        )
        assert (a.group, a.key, a.cell, a.count, a.reference_count) == (
            b.group, b.key, b.cell, b.count, b.reference_count
        ) and same_lift, f"{what}, row {index}: served {a}, expected {b}"


# ---------------------------------------------------------------------------------------------
# Values and cells, over the mask catalogue
# ---------------------------------------------------------------------------------------------


@pytest.fixture(scope="module")
def catalogue_oracle(catalogue_bundle, catalogue_filter_columns):
    """What the oracle knows of the catalogue: each entity's position, its planted values for the
    three countable fields, and the filter columns a set is evaluated with."""
    entities = catalogue_bundle.row_entity_ids(cat.VIEW_ID)
    codes = catalogue_bundle.row_position_codes(cat.VIEW_ID)
    position = dict(zip(entities, codes))
    entity_of = cat.entities_by_source(catalogue_bundle)
    shelf = {
        entity_of[source]: value
        for source in range(cat.N_ITEMS)
        if (value := cat.shelf_of(source)) is not None
    }
    x_min, x_max, _y_min, _y_max = cat.EXTENT
    columns = dict(catalogue_filter_columns)
    columns["region"] = RegionColumn(
        positions={entity: morton.deinterleave64(code) for entity, code in position.items()},
        artifacts={},
        quantise=lambda v: morton.fixed32(v, x_min, x_max),
    )
    values = {
        "department": (catalogue_filter_columns["department"].values, cat.DEPARTMENT_CODES, True),
        "archive": (catalogue_filter_columns["archive"].values, cat.ARCHIVE_CODES, False),
        "shelf": (shelf, cat.SHELF_CODES, False),
    }
    return position, columns, values


def expected_tables(oracle, m_auth: set[int], filters, reference, groupings):
    position, columns, values = oracle
    items = filt.evaluate(filters, columns, m_auth) if filters is not None else set(m_auth)
    ref = None
    if reference is not None:
        ref = filt.evaluate(reference, columns, m_auth) if reference else set(m_auth)
    tables = []
    for grouping in groupings:
        by = grouping.get("by")
        depth = grouping.get("cells", {}).get("depth")
        bbox = grouping.get("cells", {}).get("area")
        area = agg.area_of(bbox, depth, cat.EXTENT) if bbox is not None else None
        if by is None:
            tables.append(agg.table(items=items, reference=ref, groups=None, pick=None,
                                    depth=depth, position=position, area=area))
            continue
        planted, codes, derived = values[by["field"]]
        groups: dict[str, set[int]] = {}
        for entity, key in planted.items():
            groups.setdefault(key, set()).add(entity)
        if derived:
            def listable(key, groups=groups):
                return key in codes and bool(groups.get(key, set()) & m_auth)
        else:
            def listable(key):
                return key in codes
        pick = ("top", by["top"]) if "top" in by else ("named", by["values"])
        tables.append(agg.table(items=items, reference=ref, groups=groups, pick=pick,
                                listable=listable, depth=depth, position=position, area=area))
    return items, tables


@pytest.mark.parametrize("filter_name", list(FILTERS))
@pytest.mark.parametrize("case_name", CASES)
def test_every_value_and_cell_table_is_the_oracles(
    catalogue_server, catalogue_oracle, case_name, filter_name
):
    case = next(c for c in cat.catalogue() if c.name == case_name)
    token = catalogue_server.authorise(list(case.grants))["token"]
    m_auth = set(case.entities)
    filters = FILTERS[filter_name]
    body = {"view": cat.VIEW_ID, "reference": {}, "groupings": GROUPINGS, "page_rows": 20_000,
            "pages": 3}
    if filters is not None:
        body["filters"] = filters
    heads, tables = read_tables(catalogue_server, token, body)
    items, expected = expected_tables(catalogue_oracle, m_auth, filters, {}, GROUPINGS)
    for index, ((head, rows), (want_head, want_rows)) in enumerate(zip(zip(heads, tables), expected)):
        what = f"{case_name} / {filter_name} / grouping {index} {GROUPINGS[index]}"
        served_head = {k: v for k, v in head.items() if k not in ("grouping", "resumed")}
        assert served_head == want_head, f"{what}: head {head}, expected {want_head}"
        assert_rows_equal(rows, want_rows, what)

    raw = catalogue_server.viewport(token, cat.VIEW_ID, 0, cat.FULL_VIEWPORT, k=0, filters=filters)
    matched = sum(tile[2] for tile in decode_viewport(raw)[0])
    assert heads[0]["total"] == matched == len(items), f"{case_name} / {filter_name}"


def test_a_table_without_a_reference_has_no_reference_columns(catalogue_server, catalogue_oracle):
    case = next(c for c in cat.catalogue() if c.name == "crossover_above")
    token = catalogue_server.authorise(list(case.grants))["token"]
    groupings = [GROUPINGS[1], GROUPINGS[3], GROUPINGS[7]]
    heads, tables = read_tables(
        catalogue_server, token,
        {"view": cat.VIEW_ID, "filters": FILTERS["category"], "groupings": groupings},
    )
    _items, expected = expected_tables(
        catalogue_oracle, set(case.entities), FILTERS["category"], None, groupings
    )
    for index, (rows, (want_head, want_rows)) in enumerate(zip(tables, expected)):
        assert "reference_total" not in heads[index]
        assert_rows_equal(rows, want_rows, f"grouping {index}")


def test_the_cell_limit_refuses_the_whole_view_past_depth_10(catalogue_server):
    case = next(c for c in cat.catalogue() if c.name == "full_100pct")
    token = catalogue_server.authorise(list(case.grants))["token"]
    ok = catalogue_server.aggregate(token, view=cat.VIEW_ID, groupings=[{"cells": {"depth": 10}}])
    assert ok.status_code == 200, ok.text
    over = catalogue_server.aggregate(token, view=cat.VIEW_ID, groupings=[{"cells": {"depth": 11}}])
    assert over.status_code == 422 and over.json()["error"] == "contract", over.text


def test_a_field_that_cannot_be_counted_is_refused(catalogue_server):
    case = next(c for c in cat.catalogue() if c.name == "full_100pct")
    token = catalogue_server.authorise(list(case.grants))["token"]
    for field in ("title", "no_such_field"):
        resp = catalogue_server.aggregate(
            token, view=cat.VIEW_ID, groupings=[{"by": {"field": field, "top": 1}}]
        )
        assert resp.status_code == 422, resp.text
        assert resp.json()["error"] == "contract"


# ---------------------------------------------------------------------------------------------
# Artifacts, over the spatial-layer fixture
# ---------------------------------------------------------------------------------------------


def _position(x: float, y: float) -> int:
    cell_code, residual = morton.split32(q(x), q(y))
    return (cell_code << 32) | residual


@pytest.mark.parametrize("terms", PRINCIPALS, ids=["-".join(t) for t in PRINCIPALS])
def test_every_artifact_table_is_the_oracles(member_server, terms):  # noqa: F811
    server, points = member_server
    token = server.authorise(terms)["token"]
    served = decode_viewport_artifacts(server.viewport(token, SHAPES_VIEW, 0, WHOLE_MAP, k=100_000))
    ids = {(a.layer, a.key): a.tessera_id for a in served}
    position = {p[0]: _position(p[1], p[2]) for p in points}
    m_auth = {p[0] for p in visible(points, terms)}
    fx = NumericColumn({p[0]: p[0] for p in points})
    filters = {"fx_key": {"range": {"lt": 3000}}}
    items = filt.evaluate(filters, {"fx_key": fx}, m_auth)

    for layer, declared in ((SHAPES, [k for k, _ in POLYGONS]), (BOXES, [k for k, _ in BOXES_DECLARED])):
        groups = {
            ids[(layer, key)]: {p[0] for p in points if key in containing(p[1], p[2])[layer]}
            for key in declared
            if (layer, key) in ids
        }
        assert groups, f"{layer}: the layer serves artifacts to {terms}"
        first = sorted(groups)[0]
        groupings = [
            {"by": {"layer": layer, "top": 2}},
            {"by": {"layer": layer, "artifacts": [str(first), 7, first]}},
            {"by": {"layer": layer, "top": 10}, "cells": {"depth": 5}},
        ]
        heads, tables = read_tables(
            server, token,
            {"view": SHAPES_VIEW, "filters": filters, "reference": {}, "groupings": groupings},
        )
        for index, grouping in enumerate(groupings):
            by = grouping["by"]
            pick = ("top", by["top"]) if "top" in by else ("named", [int(a) for a in by["artifacts"]])
            want_head, want_rows = agg.table(
                items=items, reference=m_auth, groups=groups, pick=pick,
                listable=lambda key: key in groups,
                depth=grouping.get("cells", {}).get("depth"), position=position,
            )
            what = f"{terms} / {layer} / {grouping}"
            served_head = {k: v for k, v in heads[index].items() if k not in ("grouping", "resumed")}
            assert served_head == want_head, f"{what}: head {heads[index]}"
            assert_rows_equal(tables[index], want_rows, what)


# ---------------------------------------------------------------------------------------------
# Overlapping artifacts and artifacts withheld, over the own-label fixture
# ---------------------------------------------------------------------------------------------


@pytest.fixture(scope="module")
def label_server(tmp_path_factory):
    bundle = lfx.build_bundle(tmp_path_factory.mktemp("aggregate-labels"), with_layers=True)
    server, proc = spawn_server(bundle, tmp_path_factory.mktemp("aggregate-labels-serve"))
    yield server
    stop_server(proc)


def _served_ids(server, terms) -> dict[tuple[str, str], int]:
    token = server.authorise(terms)["token"]
    body = server.viewport(token, lfx.VIEW_ID, 0, [0.0, 0.0, lfx.EXTENT_MAX, lfx.EXTENT_MAX],
                           k=1000, artifact_budget=1000)
    return {(a.layer, a.key): a.tessera_id for a in decode_viewport_artifacts(body)}


@pytest.mark.parametrize("terms", lfx.PRINCIPALS, ids=["-".join(t) for t in lfx.PRINCIPALS])
def test_overlapping_and_withheld_artifacts_are_the_oracles(label_server, terms):
    """`teams` is a tree whose artifacts overlap, some withheld by their own label and one by its
    membership requirement; `sealed` withholds its unlabelled artifact by the layer's default
    label. `rest` and `none` count only the artifacts this principal is served, so an item held
    only by a withheld artifact is in `none`, and a withheld artifact named by its id gets no
    row."""
    every = _served_ids(label_server, lfx.PRINCIPALS[-2])
    token = label_server.authorise(terms)["token"]
    served = lfx.served(terms)
    visible_items = lfx.visible_to(terms)
    rows_of = {
        lfx.TEAMS: {key: set(members) for key, members, _l, _p in lfx.TEAM_ROWS},
        lfx.SEALED: {key: set(members) for key, members, _l in lfx.SEALED_ROWS},
    }
    # The entity ids behind the tessera_ids differ from the sources, so the oracle's groups are
    # keyed by tessera_id and hold source ids, and the set is the visible sources.
    for layer, members_of in rows_of.items():
        assert all((layer, key) in every for key in members_of), f"{layer}: the widest is served all"
        groups = {every[(layer, key)]: members_of[key] for key in members_of if (layer, key) in served}
        named = [every[(layer, key)] for key in sorted(members_of)]
        groupings = [
            {"by": {"layer": layer, "top": 3}},
            {"by": {"layer": layer, "artifacts": [str(i) for i in named]}},
        ]
        heads, tables = read_tables(
            label_server, token, {"view": lfx.VIEW_ID, "reference": {}, "groupings": groupings}
        )
        for index, grouping in enumerate(groupings):
            by = grouping["by"]
            pick = ("top", by["top"]) if "top" in by else ("named", named)
            want_head, want_rows = agg.table(
                items=visible_items, reference=visible_items, groups=groups, pick=pick,
                listable=lambda key: key in groups, depth=None, position={},
            )
            what = f"{terms} / {layer} / {grouping}"
            served_head = {k: v for k, v in heads[index].items() if k not in ("grouping", "resumed")}
            assert served_head == want_head, f"{what}: head {heads[index]}"
            assert_rows_equal(tables[index], want_rows, what)
