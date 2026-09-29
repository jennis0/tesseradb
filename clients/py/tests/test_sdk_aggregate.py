"""Aggregates from Python: `aggregate` on a viewer, a database and a selection, against a served
database.

Each table is checked against counts the test knows from the rows it inserted, and against
`count()` for the same selection. A result carried over many small responses is checked against
the same result read in one.
"""

from __future__ import annotations

import json
import urllib.request

import pytest

pytest.importorskip("pyarrow")

from tesseradb import Refusal  # noqa: E402
from test_sdk_records import papers  # noqa: E402


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


def test_a_refusal_says_what_the_server_said(db):
    with pytest.raises(Refusal, match="422"):
        db.aggregate("map", [{"by": {"field": "title", "top": 2}}])
    with pytest.raises(Refusal, match="404"):
        db.aggregate("nowhere", [{}])
