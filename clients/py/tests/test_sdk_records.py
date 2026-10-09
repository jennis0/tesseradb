"""Bulk reads from Python: `items` and `artifacts` against a served database.

A read is carried across the server's responses by their cursors. What the client returns is
checked against the same read made over raw HTTP and decoded here, so a fault in how the client
follows cursors or joins pages cannot hide behind itself. A proxy in front of the server cuts a
body as the server does when it stops one part of the way, a chunked body whose connection closes
before it ends, or refuses a request.
"""

from __future__ import annotations

import http.client
import json
import socket
import threading
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

pytest.importorskip("pyarrow")

import pyarrow as pa  # noqa: E402
import pyarrow.ipc as ipc  # noqa: E402

from conftest import post  # noqa: E402
from mosaica import PartialRead, Refusal, connect  # noqa: E402

VENUES = ["neurips", "icml", "iclr", "kdd"]
PAPERS = [f"p{i}" for i in range(20)]
#: A read of every field kind here, one page to a response and three rows to a page, so that it
#: takes seven responses.
PAGED = {
    "view": "map",
    "fields": ["paper", "venue", "n", "title"],
    "system_fields": ["position"],
    "order": "map",
    "page_rows": 3,
    "pages": 1,
}
#: Three pages of three rows to a response: 9 rows in the first response, 9 in the second and 2
#: in the third.
IDS = {"view": "map", "fields": ["paper"], "page_rows": 3, "pages": 3}
FRAME_RECORDS, FRAME_TRAILER, FRAME_PAGE_END = 7, 4, 8


def papers(db) -> None:
    """Twenty papers in a line, named by the unique attribute `paper`, five to a venue, with a
    rendered number, a keyword held only in the records, and five clusters of four."""
    ids = list(PAPERS)
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("paper", type="keyword", unique=True)
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
                "y": pa.array([0.5 * (i % 3) for i in range(20)]),
                "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
                "venue": pa.array([VENUES[i // 5] for i in range(20)]),
                "n": pa.array(range(20), pa.uint32()),
                "title": pa.array([f"title {i}" for i in range(20)]),
            }
        ),
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
        key="key",
        level="level",
    )


@pytest.fixture
def db(served, corpus):
    return served(papers)


class Watched:
    """A response whose reads are counted, passed to the client in place of the response."""

    def __init__(self, response) -> None:
        self.response = response
        self.consumed = 0

    def readinto(self, buffer) -> int:
        read = self.response.readinto(buffer)
        self.consumed += read or 0
        return read

    def read(self, *size) -> bytes:
        data = self.response.read(*size)
        self.consumed += len(data)
        return data

    def close(self) -> None:
        self.response.close()

    def isclosed(self) -> bool:
        return self.response.isclosed()

    def __enter__(self) -> "Watched":
        return self

    def __exit__(self, *raised) -> None:
        self.close()


class Sent:
    """Every bulk read's request body, in order, and its response as the client read it."""

    def __init__(self) -> None:
        self.bodies: list = []
        self.responses: list = []


@pytest.fixture(autouse=True)
def sent(monkeypatch) -> Sent:
    """What each test sends. Past 100 bulk reads the test fails, so that a read which never ends
    stops."""
    record = Sent()
    urlopen = urllib.request.urlopen

    def counted(request, *args, **kwargs):
        response = urlopen(request, *args, **kwargs)
        if request.full_url.endswith(("/v1/items", "/v1/artifacts")):
            record.bodies.append(json.loads(request.data))
            assert len(record.bodies) <= 100, "a read asked for more than 100 responses"
            response = Watched(response)
            record.responses.append(response)
        return response

    monkeypatch.setattr(urllib.request, "urlopen", counted)
    return record


class Proxy:
    """A proxy in front of a served database's viewer plane. It passes each request on and the
    answer back as a chunked body, unless a step is planned for that request: `("cut", at)`
    closes the connection at the offset `at(body)` gives, `("rewrite", change)` sends
    `change(body)`, and `("refuse", status)` answers with that status."""

    def __init__(self, upstream: str) -> None:
        self.plan: list = []
        target = urllib.parse.urlsplit(upstream)
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args) -> None:
                pass

            def do_POST(self) -> None:
                data = self.rfile.read(int(self.headers["content-length"]))
                connection = http.client.HTTPConnection(target.hostname, target.port, timeout=60)
                headers = {
                    "authorization": self.headers["authorization"],
                    "content-type": "application/json",
                }
                connection.request("POST", self.path, data, headers)
                answer = connection.getresponse()
                status, body = answer.status, answer.read()
                connection.close()
                step, value = proxy.plan.pop(0) if proxy.plan else (None, None)
                if step == "refuse":
                    status, body = value, b'{"error": "planned by the test"}'
                if step == "rewrite":
                    body = value(body)
                if status != 200:
                    self.send_response(status)
                    self.send_header("content-length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                self.send_response(200)
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                end = value(body) if step == "cut" else len(body)
                for at in range(0, end, 4096):
                    piece = body[at : min(end, at + 4096)]
                    self.wfile.write(f"{len(piece):x}\r\n".encode() + piece + b"\r\n")
                if step != "cut":
                    self.wfile.write(b"0\r\n\r\n")
                    return
                self.wfile.flush()
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


@pytest.fixture
def proxy(db):
    """A proxy in front of `db`, and a reader of `db` through it."""
    through = Proxy(db.viewer_url)
    yield through, connect(through.url, db.token().token)
    through.server.shutdown()
    through.server.server_close()


def end_of(body: bytes, kind: int, n: int) -> int:
    """Where the `n`th frame of `kind` in `body` ends."""
    at, seen = 0, 0
    while at < len(body):
        this = body[at]
        at += 5 + int.from_bytes(body[at + 1 : at + 5], "little")
        if this == kind:
            seen += 1
            if seen == n:
                return at
    raise AssertionError(f"no frame {n} of kind {kind}")


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
            if kind == FRAME_RECORDS:
                pages.append(ipc.open_stream(payload).read_all())
            elif kind == FRAME_TRAILER:
                trailer = json.loads(payload)
        assert trailer is not None, "a response with no trailer"
        if trailer["next"] is None:
            return pages
        request = following(body, trailer["next"])


def following(body: dict, cursor: str) -> dict:
    """A later request of the read `body` begins: the same with the cursor, and without
    `count`."""
    return {**{key: value for key, value in body.items() if key != "count"}, "cursor": cursor}


def papers_read(rows) -> list:
    """The `paper` of each of `rows`, a table or batches, in order."""
    if isinstance(rows, list):
        return [paper for batch in rows for paper in papers_read(batch)]
    return rows.column("paper").to_pylist()


def read_until_stopped(viewer, batches: bool, **request):
    """A read that stops part of the way: the rows it gave before the stop, and the stop."""
    if not batches:
        with pytest.raises(PartialRead) as stopped:
            viewer.items(**request)
        return papers_read(stopped.value.rows), stopped.value
    taken = []
    pages = viewer.items(**request, batches=True)
    with pytest.raises(PartialRead) as stopped:
        for page in pages:
            taken.append(page)
    assert stopped.value.rows is None
    assert pages.next == stopped.value.cursor
    return papers_read(taken), stopped.value


# ---------------------------------------------------------------------------- items


def test_a_whole_read_is_every_page_of_the_read_in_order(db, sent):
    """Seven responses joined into one table: the pages the server sends, in the order sent,
    which is the order one response holding them all gives."""
    table = db.items(**PAGED)
    assert len(sent.bodies) >= 7
    assert sorted(papers_read(table)) == sorted(PAPERS)

    by_page = http_pages(db, "items", PAGED)
    assert len(by_page) >= 7
    assert table.to_pylist() == pa.concat_tables(by_page).to_pylist()
    whole = {key: value for key, value in PAGED.items() if key not in ("page_rows", "pages")}
    one = pa.concat_tables(http_pages(db, "items", whole))
    assert table.to_pylist() == one.to_pylist()


def test_every_request_is_the_arguments_given(db, sent):
    """The first request holds exactly the arguments given; each later one adds the cursor and
    leaves out `count`, which the server takes on a read's first request only."""
    db.items("map", ["n"])
    assert sent.bodies == [{"view": "map", "fields": ["n"]}]

    asked = {
        "view": "map",
        "fields": ["venue"],
        "system_fields": ["labels"],
        "filters": {"n": {"range": {"lt": 12}}},
        "keep_unmatched": True,
        "count": True,
        "order": "stored",
        "page_rows": 2,
        "pages": 1,
        "compression": "zstd",
    }
    sent.bodies.clear()
    db.items(**asked)
    assert len(sent.bodies) >= 10
    assert sent.bodies[0] == asked
    for later in sent.bodies[1:]:
        assert later == following(asked, later["cursor"])

    sent.bodies.clear()
    db.artifacts("map", "clusters", ["key"], q="c", count=True, page_rows=2, pages=1)
    asked = {
        "view": "map",
        "layer": "clusters",
        "fields": ["key"],
        "q": "c",
        "count": True,
        "page_rows": 2,
        "pages": 1,
    }
    assert sent.bodies[0] == asked
    for later in sent.bodies[1:]:
        assert later == following(asked, later["cursor"])


def test_the_pages_taken_one_at_a_time_are_the_whole_read(db):
    """`batches=True` gives the same rows, a page at a time."""
    pages = list(db.items(**PAGED, batches=True))
    assert len(pages) >= 7
    assert all(page.num_rows <= 3 for page in pages)
    assert pa.Table.from_batches(pages).to_pylist() == db.items(**PAGED).to_pylist()


def test_the_values_read_are_the_values_inserted(db):
    """Each paper's category key, number and keyword, whatever page it arrived on."""
    rows = db.items(**PAGED).to_pylist()
    got = {row["paper"]: (row["venue"], row["n"], row["title"]) for row in rows}
    assert got == {paper: (VENUES[i // 5], i, f"title {i}") for i, paper in enumerate(PAPERS)}


def test_a_position_is_the_coordinates_the_item_was_inserted_with(db):
    """In the view's own coordinates, to one step of its grid: 45 units over 2^32 steps."""
    rows = db.items("map", ["n"], system_fields=["position"], page_rows=4, pages=1).to_pylist()
    assert len(rows) == 20
    for row in rows:
        i = row["n"]
        assert row["mosaica:x"] == pytest.approx(float(i), abs=1e-6)
        assert row["mosaica:y"] == pytest.approx(0.5 * (i % 3), abs=1e-6)


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
    head = json.loads(table.schema.metadata[b"mosaica.head"])
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


def test_pages_are_requested_only_as_they_are_taken_and_close_ends_the_read(db, sent):
    """The first response is requested at once and each later one when it is needed. `close()`
    closes the response being read, and the cursor after the pages taken reads the rest."""
    pages = db.viewer().items("map", ["paper"], page_rows=3, pages=1, batches=True)
    assert len(sent.bodies) == 1
    taken = [next(pages), next(pages)]
    assert len(sent.bodies) == 2
    assert not sent.responses[-1].isclosed()
    pages.close()
    assert sent.responses[-1].isclosed()
    assert list(pages) == []
    assert len(sent.bodies) == 2
    assert not pages.done

    rest = db.items("map", ["paper"], cursor=pages.next)
    assert sorted(papers_read(taken) + papers_read(rest)) == sorted(PAPERS)

    unread = db.items(**IDS, batches=True)
    assert not sent.responses[-1].isclosed()
    unread.close()
    assert sent.responses[-1].isclosed()


def test_frames_are_read_as_they_arrive(db, sent):
    """The first page is given before the rest of its response has been read."""
    pages = db.items(**{**IDS, "pages": 7}, batches=True)
    next(pages)
    consumed = sent.responses[0].consumed
    list(pages)
    assert len(sent.bodies) == 1
    assert consumed < sent.responses[0].consumed


def test_next_is_the_cursor_the_read_began_from_until_a_page_arrives(db):
    """`next` before any page is the read's own cursor, and `done` says when no row is left."""
    fresh = db.items(**IDS, batches=True)
    assert (fresh.next, fresh.done) == (None, False)
    first = next(fresh)
    assert fresh.next is not None and not fresh.done
    fresh.close()

    resumed = db.items(**IDS, cursor=fresh.next, batches=True)
    assert (resumed.next, resumed.done) == (fresh.next, False)
    rest = list(resumed)
    assert (resumed.next, resumed.done) == (None, True)
    assert sorted(papers_read([first] + rest)) == sorted(PAPERS)


CUTS = {
    "inside a frame": lambda body: end_of(body, FRAME_PAGE_END, 2) + 10,
    "on a frame boundary": lambda body: end_of(body, FRAME_PAGE_END, 2),
    "before a page end": lambda body: end_of(body, FRAME_RECORDS, 3),
}


@pytest.mark.parametrize("batches", [False, True], ids=["table", "batches"])
@pytest.mark.parametrize("cut", list(CUTS))
def test_a_cut_read_gives_its_whole_pages_and_the_cursor_to_read_on_from(proxy, batches, cut):
    """The second response is cut two pages in: the first response's three pages and the
    second's first two are given, a page with no page end after it is not, and the cursor reads
    the rest with nothing lost or repeated."""
    through, viewer = proxy
    through.plan = [(None, None), ("cut", CUTS[cut])]
    read, stopped = read_until_stopped(viewer, batches, **IDS)
    assert len(read) == 15
    assert not stopped.done and stopped.cursor is not None
    rest = viewer.items(**IDS, cursor=stopped.cursor)
    assert sorted(read + papers_read(rest)) == sorted(PAPERS)


@pytest.mark.parametrize("batches", [False, True], ids=["table", "batches"])
def test_a_read_cut_after_its_last_page_says_every_row_arrived(proxy, batches):
    through, viewer = proxy
    through.plan = [("cut", lambda body: end_of(body, FRAME_PAGE_END, 1))]
    read, stopped = read_until_stopped(viewer, batches, view="map", fields=["paper"])
    assert sorted(read) == sorted(PAPERS)
    assert stopped.done and stopped.cursor is None


@pytest.mark.parametrize("batches", [False, True], ids=["table", "batches"])
def test_a_later_request_refused_gives_the_pages_before_it_and_the_cursor(proxy, batches):
    """The bulk-read lane full at the second request: the first response's pages are given,
    and the cursor after them reads the rest."""
    through, viewer = proxy
    through.plan = [(None, None), ("refuse", 429)]
    read, stopped = read_until_stopped(viewer, batches, **IDS)
    assert len(read) == 9
    assert stopped.cursor is not None and not stopped.done
    rest = viewer.items(**IDS, cursor=stopped.cursor)
    assert sorted(read + papers_read(rest)) == sorted(PAPERS)


def test_a_page_end_given_twice_is_refused(proxy):
    """A page end with no page before it stops the read after the pages before it."""
    through, viewer = proxy

    def twice(body: bytes) -> bytes:
        start = end_of(body, FRAME_RECORDS, 1)
        end = end_of(body, FRAME_PAGE_END, 1)
        return body[:end] + body[start:end] + body[end:]

    through.plan = [("rewrite", twice)]
    read, stopped = read_until_stopped(viewer, True, **IDS)
    assert len(read) == 3
    assert stopped.cursor is not None


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


def test_a_read_that_returns_no_row_has_the_columns_asked_for(db):
    """A table of no rows, typed as a read with rows types it; from `batches=True`, one batch of
    no rows."""
    asked = {"view": "map", "fields": ["paper", "venue", "n", "title"], "system_fields": ["labels"]}
    schema = db.items(**asked).schema
    nothing = {**asked, "filters": {"venue": {"eq": "absent"}}}
    table = db.items(**nothing)
    assert table.num_rows == 0
    assert table.schema.remove_metadata() == schema.remove_metadata()
    pages = list(db.items(**nothing, batches=True))
    assert [page.num_rows for page in pages] == [0]
    assert pages[0].schema.remove_metadata() == schema.remove_metadata()

    empty = db.artifacts("map", "clusters", ["key", "masked_count"], q="absent")
    assert (empty.num_rows, empty.column_names) == (0, ["tessera_id", "key", "masked_count"])


def numbered(db) -> None:
    """Four papers named by a signed integer unique attribute."""
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("paper", type="i64", unique=True)
    db.insert(
        "map",
        pa.table(
            {
                "paper": pa.array([-3, 0, 7, 2**40], pa.int64()),
                "x": pa.array([1.0, 2.0, 3.0, 4.0]),
                "y": pa.array([0.0] * 4),
                "labels": pa.array([["public"]] * 4, pa.list_(pa.string())),
            }
        ),
        x="x",
        y="y",
        access="labels",
    )


def test_an_integer_join_value_comes_back_as_the_integer_inserted(served, corpus):
    one = served(numbered)
    table = one.items("map", ["paper"], page_rows=1)
    assert table.schema.field("paper").type == pa.int64()
    ids = table.column("paper").to_pylist()
    assert sorted(ids) == [-3, 0, 7, 2**40]
    for tessera_id, inserted in zip(table.column("tessera_id").to_pylist(), ids):
        assert one.item(tessera_id)["fields"]["paper"] == inserted
    pages = one.items("map", ["paper"], page_rows=1, batches=True)
    assert sorted(i for page in pages for i in page.column(1).to_pylist()) == sorted(ids)


# ---------------------------------------------------------------------------- a selection


def test_a_selection_reads_the_items_it_counts(db, sent):
    """Its filters and its box go as the read's filters, and nothing else is added."""
    selection = db.view("map").filter({"venue": {"in": ["icml", "iclr"]}}).within((0, -1, 12.5, 2))
    table = selection.items(["n", "paper"], page_rows=2)
    assert table.num_rows == selection.count() == 8
    assert sorted(table.column("n").to_pylist()) == list(range(5, 13))
    assert sorted(papers_read(table)) == sorted(f"p{i}" for i in range(5, 13))
    assert sent.bodies[0] == {
        "view": "map",
        "fields": ["n", "paper"],
        "page_rows": 2,
        "filters": {
            "all_of": [
                {"venue": {"in": ["icml", "iclr"]}},
                {"region": {"bbox": [0.0, -1.0, 12.5, 2.0]}},
            ]
        },
    }
    pages = selection.items(["n"], batches=True)
    assert sum(page.num_rows for page in pages) == 8

    everything = db.view("map").items(["n"])
    assert everything.num_rows == db.view("map").count() == 20
    with pytest.raises(Refusal):
        selection.items(["n"], filters={"n": {"eq": 1}})


# ---------------------------------------------------------------------------- artifacts


def test_artifacts_are_read_whole_across_responses(db, sent):
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
    assert len(sent.bodies) >= 5
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
    head = json.loads(table.schema.metadata[b"mosaica.head"])
    assert (head["served"], head["matched"]) == (5, 2)
