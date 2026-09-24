"""Bulk reads from Python: `items` and `artifacts` against a served database.

A read is carried across the server's responses by their cursors. What the client returns is
checked against the same read made over raw HTTP and decoded here, so a fault in how the client
follows cursors or joins pages cannot hide behind itself.
"""

from __future__ import annotations

import io
import json
import urllib.request

import pytest

pytest.importorskip("pyarrow")

import pyarrow as pa  # noqa: E402
import pyarrow.ipc as ipc  # noqa: E402

from conftest import post  # noqa: E402
from tesseradb import Refusal  # noqa: E402

VENUES = ["neurips", "icml", "iclr", "kdd"]
PAPERS = [f"p{i}".encode() for i in range(20)]
#: A read of every field kind here, one page to a response and three rows to a page, so that it
#: takes seven responses.
PAGED = {
    "view": "map",
    "fields": ["venue", "n", "title"],
    "system_fields": ["external_id"],
    "order": "map",
    "page_rows": 3,
    "pages": 1,
}


def papers(db) -> None:
    """Twenty papers in a line, five to a venue, with a rendered number, a keyword held only in
    the records, and five clusters of four."""
    ids = [paper.decode() for paper in PAPERS]
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_vocabulary("venue")
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    db.declare_attribute("n", type="u32", render=True)
    db.declare_attribute("title", type="keyword")
    db.declare_layer("clusters", kind="flat")
    db.insert(
        "map",
        pa.table(
            {
                "paper": pa.array(ids),
                "x": pa.array([float(i) for i in range(20)]),
                "y": pa.array([0.0] * 20),
                "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
                "venue": pa.array([VENUES[i // 5] for i in range(20)]),
                "n": pa.array(range(20), pa.uint32()),
                "title": pa.array([f"title {i}" for i in range(20)]),
            }
        ),
        id="paper",
        x="x",
        y="y",
        access="labels",
    )
    db.insert(
        "clusters",
        artifacts=pa.table(
            {
                "level": pa.array([0] * 5, pa.uint32()),
                "key": pa.array([f"c{k}" for k in range(5)]),
            }
        ),
        key="key",
        level="level",
    )
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * 20, pa.uint32()),
                "key": pa.array([f"c{i // 4}" for i in range(20)]),
                "paper": pa.array(ids),
            }
        ),
        id="paper",
        key="key",
        level="level",
    )


@pytest.fixture
def db(served, corpus):
    return served(papers)


@pytest.fixture(autouse=True)
def requests(monkeypatch):
    """The body of every bulk read a test sends. Past 100 of them the test fails, so that a read
    which never ends stops."""
    sent = []
    urlopen = urllib.request.urlopen

    def counted(request, *args, **kwargs):
        if request.full_url.endswith(("/v1/items", "/v1/artifacts")):
            sent.append(json.loads(request.data))
            assert len(sent) <= 100, "a read asked for more than 100 responses"
        return urlopen(request, *args, **kwargs)

    monkeypatch.setattr(urllib.request, "urlopen", counted)
    return sent


def http_pages(db, route: str, body: dict) -> list:
    """Every page of a read made over raw HTTP, following each response's cursor."""
    token = db.token().token
    pages, request = [], dict(body)
    while True:
        content = post(f"{db.viewer_url}/v1/{route}", token, request)
        trailer, at = None, 0
        while at < len(content):
            kind = content[at]
            length = int.from_bytes(content[at + 1 : at + 5], "little")
            payload = content[at + 5 : at + 5 + length]
            at += 5 + length
            if kind == 7:
                pages.append(ipc.open_stream(payload).read_all())
            elif kind == 4:
                trailer = json.loads(payload)
        assert trailer is not None, "a response with no trailer"
        if trailer["next"] is None:
            return pages
        request = {key: value for key, value in body.items() if key != "count"}
        request["cursor"] = trailer["next"]


def papers_read(rows) -> list:
    """The external ids of `rows`, a table or batches, in order."""
    if isinstance(rows, list):
        return [paper for batch in rows for paper in papers_read(batch)]
    return rows.column("tessera:external_id").to_pylist()


# ---------------------------------------------------------------------------- items


def test_a_whole_read_is_every_page_of_the_read_in_order(db, requests):
    """Seven responses joined into one table: the pages the server sends, in the order sent,
    which is the order one response holding them all gives."""
    table = db.items(**PAGED)
    assert len(requests) >= 7
    assert sorted(papers_read(table)) == sorted(PAPERS)

    by_page = http_pages(db, "items", PAGED)
    assert len(by_page) >= 7
    assert table.to_pylist() == pa.concat_tables(by_page).to_pylist()
    whole = {key: value for key, value in PAGED.items() if key not in ("page_rows", "pages")}
    assert table.to_pylist() == pa.concat_tables(http_pages(db, "items", whole)).to_pylist()


def test_the_pages_taken_one_at_a_time_are_the_whole_read(db):
    """`batches=True` gives the same rows, a page at a time."""
    pages = list(db.items(**PAGED, batches=True))
    assert len(pages) >= 7
    assert all(page.num_rows <= 3 for page in pages)
    assert pa.Table.from_batches(pages).to_pylist() == db.items(**PAGED).to_pylist()


def test_the_values_read_are_the_values_inserted(db):
    """Each paper's category key, number and keyword, whatever page it arrived on."""
    rows = db.items(**PAGED).to_pylist()
    got = {row["tessera:external_id"]: (row["venue"], row["n"], row["title"]) for row in rows}
    assert got == {paper: (VENUES[i // 5], i, f"title {i}") for i, paper in enumerate(PAPERS)}


def test_a_filter_narrows_the_read_and_count_is_read_from_the_head(db):
    """Only the matching items are returned; `count` puts both counts in the head, which the
    whole read keeps in its schema metadata."""
    table = db.items(
        "map",
        ["n"],
        filters={"venue": {"eq": "icml"}},
        count=True,
        page_rows=2,
        pages=1,
    )
    assert sorted(table.column("n").to_pylist()) == [5, 6, 7, 8, 9]
    head = json.loads(table.schema.metadata[b"tessera.head"])
    assert (head["visible"], head["matched"]) == (20, 5)


def test_category_dictionaries_that_differ_by_page_join_into_one_column(db):
    """Each page's dictionary holds only its own keys. The whole table holds one column that
    pyarrow groups by and pandas reads as one categorical."""
    pages = list(db.items(**PAGED, batches=True))
    assert len({tuple(page.column("venue").dictionary.to_pylist()) for page in pages}) > 1

    table = db.items(**PAGED)
    counted = table.group_by("venue").aggregate([("tessera_id", "count")])
    by_venue = zip(counted.column("venue").to_pylist(), counted.column("tessera_id_count"))
    assert {venue: count.as_py() for venue, count in by_venue} == {venue: 5 for venue in VENUES}

    pd = pytest.importorskip("pandas")
    frame = table.to_pandas()
    assert isinstance(frame["venue"].dtype, pd.CategoricalDtype)
    assert sorted(frame["venue"].cat.categories) == sorted(VENUES)
    assert frame["venue"].value_counts().to_dict() == {venue: 5 for venue in VENUES}


def test_a_compressed_read_equals_an_uncompressed_one(db):
    plain = db.items(**PAGED)
    packed = db.items(**PAGED, compression="zstd")
    assert packed.schema == plain.schema
    assert packed.to_pylist() == plain.to_pylist()


def test_pages_are_requested_only_as_they_are_taken(db, requests):
    """The first response is requested at once and each later one when it is needed. The cursor
    after the pages taken reads the rest, and nothing is lost or repeated."""
    pages = db.viewer().items(
        "map", [], system_fields=["external_id"], page_rows=3, pages=1, batches=True
    )
    assert len(requests) == 1
    taken = [next(pages), next(pages)]
    assert len(requests) == 2
    pages.close()
    assert list(pages) == []
    assert len(requests) == 2

    rest = db.items("map", [], system_fields=["external_id"], cursor=pages.next)
    read = papers_read(taken) + papers_read(rest)
    assert sorted(read) == sorted(PAPERS)


def test_a_response_cut_short_raises_and_the_read_goes_on_from_its_last_whole_page(db, monkeypatch):
    """A body that ends before its trailer: the whole pages before the cut are given, then a
    refusal, and the cursor they leave reads the rest."""
    urlopen = urllib.request.urlopen
    cut = []

    def cut_once(request, *args, **kwargs):
        response = urlopen(request, *args, **kwargs)
        if request.full_url.endswith("/v1/items") and not cut:
            cut.append(request)
            return io.BytesIO(response.read()[:-1])
        return response

    monkeypatch.setattr(urllib.request, "urlopen", cut_once)
    pages = db.items("map", [], system_fields=["external_id"], page_rows=3, pages=3, batches=True)
    taken = []
    with pytest.raises(Refusal):
        for page in pages:
            taken.append(page)
    assert len(taken) == 3

    rest = db.items("map", [], system_fields=["external_id"], cursor=pages.next)
    assert sorted(papers_read(taken) + papers_read(rest)) == sorted(PAPERS)


def test_a_refused_read_raises_a_refusal(db):
    """The server's refusals, raised where the read is asked for."""
    with pytest.raises(Refusal):
        db.items("map", ["no_such_field"])
    with pytest.raises(Refusal):
        db.items("map", ["no_such_field"], batches=True)
    with pytest.raises(Refusal):
        db.artifacts("map", "no/such/layer", ["key"])

    pages = db.items("map", ["n"], page_rows=3, pages=1, batches=True)
    next(pages)
    with pytest.raises(Refusal):
        db.items("map", ["n"], count=True, cursor=pages.next)


def test_a_read_that_returns_no_row_is_a_table_of_ids(db):
    table = db.items("map", ["n"], filters={"venue": {"eq": "absent"}})
    assert table.num_rows == 0
    assert table.column_names == ["tessera_id"]


# ---------------------------------------------------------------------------- artifacts


def test_artifacts_are_read_whole_across_responses(db, requests):
    """A layer of five, one artifact to a response, read whole and equal to the pages over raw
    HTTP."""
    body = {
        "view": "map",
        "layer": "clusters",
        "fields": ["key", "masked_count"],
        "page_rows": 1,
        "pages": 1,
    }
    table = db.artifacts(**body)
    assert len(requests) >= 5
    assert sorted(table.column("key").to_pylist()) == [f"c{k}" for k in range(5)]
    assert table.column("masked_count").to_pylist() == [4] * 5
    assert table.to_pylist() == pa.concat_tables(http_pages(db, "artifacts", body)).to_pylist()


def test_artifacts_under_a_filter_carry_their_matched_count(db):
    """Only the clusters with a matching paper, each with how many match."""
    table = db.artifacts(
        "map", "clusters", ["key"], filters={"venue": {"eq": "neurips"}}, count=True
    )
    matched = table.column("matched_count").to_pylist()
    assert dict(zip(table.column("key").to_pylist(), matched)) == {"c0": 4, "c1": 1}
    head = json.loads(table.schema.metadata[b"tessera.head"])
    assert (head["served"], head["matched"]) == (5, 2)
