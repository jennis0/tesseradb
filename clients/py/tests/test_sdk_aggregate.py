"""Aggregates from Python: `aggregate` on a viewer, a database and a selection, against a served
database.

Each table is checked against counts the test knows from the rows it inserted, and against
`count()` for the same selection. A result carried over many small responses is checked against
the same result read in one.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request

import pytest

pa = pytest.importorskip("pyarrow")

from conftest import post  # noqa: E402
from tesseradb import PartialRead, Refusal, connect  # noqa: E402
from test_sdk_records import Proxy, end_of, papers  # noqa: E402


@pytest.fixture
def db(served, corpus):
    return served(papers)


def head(table) -> dict:
    return json.loads(table.schema.metadata[b"tessera.head"])


def rows(table) -> list:
    return table.to_pylist()


def test_the_size_a_breakdown_and_a_density_surface(db):
    """Twenty papers, five to a venue: the size of the set, the two venues with most papers
    (ties by key) then the rest, and cells whose counts sum to the set."""
    size, venues, cells = db.aggregate(
        "map", [{}, {"by": {"field": "venue", "top": 2}}, {"cells": {"depth": 3}}]
    )
    assert rows(size) == [{"count": 20}]
    assert head(size) == {"grouping": 0, "total": 20}
    assert [(r["group"], r["key"], r["count"]) for r in rows(venues)] == [
        ("listed", "iclr", 5),
        ("listed", "icml", 5),
        ("rest", None, 10),
    ]
    assert head(venues) == {"grouping": 1, "total": 20, "groups": 4}
    assert sum(cells.column("count").to_pylist()) == 20
    assert len(cells) > 1
    assert size.schema.metadata[b"tessera.recomposed"] == b"false"
    assert b"tessera.region" not in size.schema.metadata


def test_a_filtered_set_against_a_reference(db):
    """Ten papers of twenty against all of them: the first two venues hold them all, so each has
    twice its share and the others none."""
    (venues,) = db.aggregate(
        "map",
        [{"by": {"field": "venue", "values": ["neurips", "icml", "kdd"]}}],
        filters={"n": {"range": {"lt": 10}}},
        reference={},
    )
    assert head(venues) == {"grouping": 0, "total": 10, "reference_total": 20, "groups": 2}
    assert [
        (r["group"], r["key"], r["count"], r["reference_count"], r["lift"]) for r in rows(venues)
    ] == [
        ("listed", "neurips", 5, 5, 2.0),
        ("listed", "icml", 5, 5, 2.0),
        ("listed", "kdd", 0, 5, 0.0),
        ("rest", None, 0, 5, 0.0),
    ]


def test_the_artifacts_of_a_layer(db):
    """Five clusters of four papers each, keyed by their `tessera_id`s as the artifacts read
    gives them."""
    (clusters,) = db.aggregate("map", [{"by": {"layer": "clusters", "top": 5}}])
    assert [r["count"] for r in rows(clusters)] == [4] * 5
    ids = db.artifacts("map", "clusters", []).column("tessera_id").to_pylist()
    assert sorted(clusters.column("key").to_pylist()) == sorted(ids)


def named_papers(db) -> None:
    """The papers, and two named clusters over the first ten."""
    papers(db)
    db.declare_layer(
        "named",
        kind="flat",
        supplied=[("name", "text", "inherited")],
        artifacts=[
            {"key": "a", "members": {"paper": [f"p{i}" for i in range(6)]}, "contents": [["Alpha"]]},
            {"key": "b", "members": {"paper": [f"p{i}" for i in range(6, 10)]}, "contents": [["Beta"]]},
        ],
    )


def test_a_layer_row_is_titled_with_the_name_browse_gives(served, corpus):
    db = served(named_papers)
    (named,) = db.aggregate("map", [{"by": {"layer": "named", "top": 5}}])
    listed = [r for r in rows(named) if r["group"] == "listed"]
    assert [(r["title"], r["count"]) for r in listed] == [("Alpha", 6), ("Beta", 4)]
    browsed = db.viewer().browse_artifacts("map", "named")["artifacts"]
    assert {r["key"]: r["title"] for r in listed} == {int(r["tessera_id"]): r["name"] for r in browsed}


def test_a_selection_counts_what_count_counts(db):
    """The selection's filters and box are the request's filters."""
    part = db.view("map").filter({"n": {"range": {"gte": 3}}}).within((0.0, 0.0, 12.0, 1.0))
    (size,) = part.aggregate([{}])
    assert head(size)["total"] == part.count() > 0
    assert size.schema.metadata[b"tessera.region"] == b"exact"
    (with_reference,) = part.aggregate([{}], reference={})
    assert head(with_reference)["reference_total"] == 20


def test_a_viewer_reads_every_response_and_joins_each_tables_pages(db, monkeypatch):
    """A page of one row and one page to a response: the result, over many responses, is the
    one a single response gives, and each later request is the first with its cursor."""
    groupings = [{"by": {"field": "venue", "top": 3}, "cells": {"depth": 2}}, {"cells": {"depth": 4}}]
    whole = db.aggregate("map", groupings, reference={})

    bodies = []
    urlopen = urllib.request.urlopen

    def paged(request, *args, **kwargs):
        if request.full_url.endswith("/v1/aggregate"):
            body = {**json.loads(request.data), "page_rows": 1, "pages": 1}
            bodies.append(body)
            request.data = json.dumps(body).encode()
            assert len(bodies) <= 200, "the read asked for more than 200 responses"
        return urlopen(request, *args, **kwargs)

    monkeypatch.setattr(urllib.request, "urlopen", paged)
    several = db.viewer().aggregate("map", groupings, reference={})
    assert len(bodies) > 3
    assert bodies[0] == {"view": "map", "groupings": groupings, "reference": {}, "page_rows": 1, "pages": 1}
    assert all(body == {**bodies[0], "cursor": body["cursor"]} for body in bodies[1:])
    assert [rows(t) for t in several] == [rows(t) for t in whole]
    assert [head(t) for t in several] == [head(t) for t in whole]


def test_a_number_in_bins(db):
    """`n`, a `u32`, runs from 0 to 19: four readable bins of five hold it, their edges `uint64`,
    with the same edges under a filter, and a range of two bins, whose last holds its upper edge,
    leaves the others in `rest`."""
    (whole,) = db.aggregate("map", [{"by": {"field": "n", "bins": 4}}])
    edges = [(0, 5), (5, 10), (10, 15), (15, 20)]
    assert [(r["group"], r["lower"], r["upper"], r["count"]) for r in rows(whole)] == [
        ("listed", lo, hi, 5) for lo, hi in edges
    ]
    assert whole.schema.field("lower").type == pa.uint64()
    assert head(whole) == {"grouping": 0, "total": 20, "groups": 4}
    (part,) = db.aggregate(
        "map", [{"by": {"field": "n", "bins": 4}}], filters={"n": {"range": {"lt": 10}}}, reference={}
    )
    assert [(r["lower"], r["upper"], r["count"], r["reference_count"]) for r in rows(part)] == [
        (lo, hi, 5 if lo < 10 else 0, 5) for lo, hi in edges
    ]
    (ranged,) = db.aggregate("map", [{"by": {"field": "n", "bins": 2, "range": [0, 10]}}])
    assert [(r["group"], r["lower"], r["upper"], r["count"]) for r in rows(ranged)] == [
        ("listed", 0, 5, 5),
        ("listed", 5, 10, 6),
        ("rest", None, None, 9),
    ]


def test_a_sampled_number_in_bins(db):
    """A sample of 8 of the twenty is more than one item in 64, which no identity band holds, so
    every item is counted, as no sample does, and the head says so; so does a sample of twenty.
    Without a range, the edges are the unsampled table's."""
    grouping = {"field": "n", "bins": 2, "range": [0, 10]}
    (exact,) = db.aggregate("map", [{"by": grouping}])
    assert "sample" not in head(exact)
    for s in (8, 20):
        (counted,) = db.aggregate("map", [{"by": {**grouping, "sample": s}}])
        assert head(counted)["sample"] == {"sampled": False, "items": 20}
        assert rows(counted) == rows(exact)
    (drawn,) = db.aggregate("map", [{"by": {"field": "n", "bins": 2, "sample": 8}}])
    assert head(drawn)["sample"] == {"sampled": False, "items": 20}
    (whole,) = db.aggregate("map", [{"by": {"field": "n", "bins": 2}}])
    assert [(r["lower"], r["upper"]) for r in rows(drawn)] == [
        (r["lower"], r["upper"]) for r in rows(whole)
    ]


def test_a_summary_of_a_number(db):
    """`n` runs from 0 to 19: its summary is every paper's, whatever the filter, and its smallest
    and largest value are `uint64` as the field's edges are."""
    (whole,) = db.aggregate("map", [{"by": {"field": "n", "summary": True}}])
    assert rows(whole) == [{"items": 20, "count": 20, "none": 0, "min": 0, "max": 19, "mean": 9.5}]
    assert whole.schema.field("min").type == pa.uint64()
    assert head(whole) == {"grouping": 0, "total": 20}
    (part,) = db.aggregate(
        "map", [{"by": {"field": "n", "summary": True}}], filters={"n": {"range": {"lt": 10}}}
    )
    assert rows(part) == rows(whole)
    assert head(part)["total"] == 10


def test_a_refusal_says_what_the_server_said(db):
    with pytest.raises(Refusal, match="422"):
        db.aggregate("map", [{"by": {"field": "title", "top": 2}}])
    with pytest.raises(Refusal, match="404"):
        db.aggregate("nowhere", [{}])


def test_a_response_cut_short_raises_the_tables_read_and_the_cursor_to_read_on(db):
    """Cut after the first table's page: the error holds that table whole and a cursor from which
    the server sends the rest, which is the rest of the result read in one."""
    groupings = [{}, {"by": {"field": "venue", "top": 2}, "cells": {"depth": 3}}]
    whole = db.aggregate("map", groupings)
    through = Proxy(db.viewer_url)
    try:
        through.plan.append(("cut", lambda body: end_of(body, 8, 1)))
        with pytest.raises(PartialRead) as stopped:
            connect(through.url, db.token().token).aggregate("map", groupings)
    finally:
        through.server.shutdown()
        through.server.server_close()
    assert [rows(t) for t in stopped.value.rows] == [rows(whole[0])]
    assert stopped.value.cursor is not None and not stopped.value.done
    rest = post(
        f"{db.viewer_url}/v1/aggregate",
        db.token().token,
        {"view": "map", "groupings": groupings, "cursor": stopped.value.cursor},
    )
    at, heads = 0, []
    while at < len(rest):
        length = int.from_bytes(rest[at + 1 : at + 5], "little")
        if rest[at] == 9:
            heads.append(json.loads(rest[at + 5 : at + 5 + length])["grouping"])
        at += 5 + length
    assert heads == [1]


def test_a_later_request_refused_raises_the_tables_read(db, monkeypatch):
    """One row to a response, and the third request refused: the error holds the rows before it
    and the cursor that request carried."""
    urlopen = urllib.request.urlopen
    bodies = []

    def refusing(request, *args, **kwargs):
        if request.full_url.endswith("/v1/aggregate"):
            body = {**json.loads(request.data), "page_rows": 1, "pages": 1}
            bodies.append(body)
            if len(bodies) == 3:
                raise urllib.error.HTTPError(request.full_url, 429, "busy", {}, None)
            request.data = json.dumps(body).encode()
        return urlopen(request, *args, **kwargs)

    monkeypatch.setattr(urllib.request, "urlopen", refusing)
    with pytest.raises(PartialRead) as stopped:
        db.viewer().aggregate("map", [{"cells": {"depth": 4}}])
    assert stopped.value.cursor == bodies[2]["cursor"]
    assert [len(t) for t in stopped.value.rows] == [2]


def tree_papers(db) -> None:
    """The papers, and a tree over them: a root over all twenty, two branches of ten and four
    leaves of five."""
    papers(db)
    db.declare_layer("tree", kind="nested", prune_children=True)
    nodes = [("root", None, range(20))]
    nodes += [(f"b{b}", "root", range(b * 10, b * 10 + 10)) for b in range(2)]
    nodes += [(f"l{leaf}", f"b{leaf // 2}", range(leaf * 5, leaf * 5 + 5)) for leaf in range(4)]
    db.insert(
        "tree",
        artifacts=pa.table(
            {
                "level": pa.array([0] * len(nodes), pa.uint32()),
                "key": pa.array([key for key, _, _ in nodes]),
                "parent": pa.array([parent for _, parent, _ in nodes]),
            }
        ),
        key="key",
        level="level",
        parent="parent",
    )
    members = [(key, f"p{i}") for key, _, held in nodes for i in held]
    db.insert(
        "tree",
        members=pa.table(
            {
                "level": pa.array([0] * len(members), pa.uint32()),
                "key": pa.array([key for key, _ in members]),
                "paper": pa.array([paper for _, paper in members]),
            }
        ),
        key="key",
        level="level",
    )


def test_a_tree_ranks_the_clusters_the_map_draws(served, corpus):
    """A tree's top artifacts are taken at the cut the map draws: the four leaves with no budget,
    the root under a budget of one; without a cut the server refuses the grouping."""
    db = served(tree_papers)
    whole = {"zoom": 0, "bbox": [-5.0, -5.0, 40.0, 40.0]}
    (leaves,) = db.aggregate("map", [{"by": {"layer": "tree", "top": 10, "cut": whole}}])
    assert [(r["group"], r["count"]) for r in rows(leaves)] == [("listed", 5)] * 4
    (root,) = db.aggregate("map", [{"by": {"layer": "tree", "top": 10, "cut": {**whole, "budget": 1}}}])
    assert [(r["group"], r["count"]) for r in rows(root)] == [("listed", 20)]
    with pytest.raises(Refusal, match="422"):
        db.aggregate("map", [{"by": {"layer": "tree", "top": 10}}])
