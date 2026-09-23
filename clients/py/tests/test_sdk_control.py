"""The control-plane client on its own: the retry rule and the batch id.

The `429` is the one status the SDK retries, and it must resend identical bytes: a batch id maps to
the SHA-256 of the raw request body, so a body re-serialised before the retry would be a new batch
rather than a replay. That is what this file fakes a server for. Everything else about the plane is
tested against a real one.
"""

from __future__ import annotations

import http.server
import json
import threading

import pytest

from tesseradb import _control
from tesseradb._control import (
    MAX_ATTEMPTS,
    UNANSWERED,
    Control,
    addressed,
    batch_id,
    external_id,
)
from tesseradb._refusal import Refusal


class _Backpressure(http.server.BaseHTTPRequestHandler):
    """Answers `429` with a `Retry-After` until `refusals` is spent, then `200`."""

    refusals = 2
    bodies: list = []
    batches: list = []

    def do_POST(self) -> None:  # noqa: N802, the base class's spelling
        length = int(self.headers.get("content-length", "0"))
        _Backpressure.bodies.append(self.rfile.read(length))
        _Backpressure.batches.append(self.headers.get("x-tessera-batch-id"))
        if _Backpressure.refusals > 0:
            _Backpressure.refusals -= 1
            answer = json.dumps({"error": "busy", "retry_after_s": 0.01}).encode()
            self.send_response(429)
            self.send_header("retry-after", "0.01")
            self.send_header("content-length", str(len(answer)))
            self.end_headers()
            self.wfile.write(answer)
            return
        answer = json.dumps({"accepted": 1, "minted": 0, "tessera_ids": [7]}).encode()
        self.send_response(200)
        self.send_header("content-length", str(len(answer)))
        self.end_headers()
        self.wfile.write(answer)

    def log_message(self, *args) -> None:
        return


def test_a_429_is_retried_after_its_retry_after_with_identical_bytes():
    _Backpressure.refusals = 2
    _Backpressure.bodies = []
    _Backpressure.batches = []
    server = http.server.HTTPServer(("127.0.0.1", 0), _Backpressure)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        control = Control(f"http://127.0.0.1:{server.server_port}", "credential")
        body = b"one page of rows"
        answer = control.ingest(body, batch_id("points", 0), view="s0")
    finally:
        server.shutdown()
        server.server_close()
    assert answer.ok and answer.attempts == 3
    # Identical bytes under one batch id: a retry is a replay, never a second batch.
    assert _Backpressure.bodies == [body] * 3
    assert len(set(_Backpressure.batches)) == 1


def test_a_batch_id_is_fresh_per_request_and_never_derived_from_the_body():
    """A request is identified by an id the client chose, and identity is never inferred from
    what the request contains. Two pages carrying the same bytes are two requests, so the same
    frame committed twice is loaded twice."""
    assert batch_id("points", 0) != batch_id("points", 0)
    assert batch_id("points", 0).startswith("points-0-")


def test_an_external_id_is_the_bytes_the_id_column_holds():
    """A string's UTF-8, an integer's eight little-endian bytes, binary as it stands."""
    assert external_id(1) == b"\x01\x00\x00\x00\x00\x00\x00\x00"
    assert len(external_id(2**63)) == 8
    assert external_id("p3") == b"p3"
    assert external_id(b"\x00\xff") == b"\x00\xff"
    # A frame's own ids arrive as numpy scalars, which are integers and are not `int`.
    numpy = pytest.importorskip("numpy")
    assert external_id(numpy.int64(5)) == external_id(5)
    assert external_id(numpy.uint32(5)) == external_id(5)
    assert addressed(1) == "AQAAAAAAAAA="
    assert addressed("p3") == "cDM="


def serving():
    """The fake plane, started, with its address."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _Backpressure)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}"


def test_a_429_that_never_lets_up_is_reported_rather_than_retried_for_ever(monkeypatch):
    """`MAX_ATTEMPTS` bounds the wait: the page comes back as the refusal the report carries."""
    monkeypatch.setattr(_control, "MAX_ATTEMPTS", 3)
    _Backpressure.refusals = 1_000
    _Backpressure.bodies = []
    _Backpressure.batches = []
    server, base = serving()
    try:
        answer = Control(base, "credential").ingest(b"rows", batch_id("points", 0))
    finally:
        server.shutdown()
        server.server_close()
    assert not answer.ok and answer.status == 429
    assert answer.attempts == 3 and len(_Backpressure.bodies) == 3
    assert MAX_ATTEMPTS > 3, "the SDK's own figure is the one a deployment waits under"


def test_a_request_that_reaches_no_server_is_an_answer_and_not_an_exception():
    """A page that was never answered is a refusal the report carries: whether it landed is the
    database's to say, and a commit after it is a new request."""
    server, base = serving()
    server.shutdown()
    server.server_close()
    answer = Control(base, "credential").ingest(b"rows", batch_id("points", 0))
    assert not answer.ok and answer.status == UNANSWERED
    assert "did not answer" in answer.detail


def test_a_commit_whose_pages_reach_no_server_reports_it_and_does_not_raise(tmp_path, monkeypatch):
    """A page that was never answered is a refusal the report carries, not an exception."""
    import pandas as pd

    from conftest import binary
    from tesseradb._database import Database, create

    binary()
    db = create(tmp_path / "db")
    db.declare_view("map", extent={"min": 0.0, "max": 8.0})
    rows = pd.DataFrame({"id": [1, 2], "x": [0.0, 1.0], "y": [0.0, 1.0],
                         "access": ["public", "public"]})
    db.insert("map", rows, id="id", x="x", y="y", access="access")
    try:
        assert db.commit().ok
        db.insert("map", rows, id="id", x="x", y="y", access="access")
        # The operator plane this commit's pages go to is an address nothing is listening on.
        server, base = serving()
        server.shutdown()
        server.server_close()
        monkeypatch.setattr(Database, "control", property(lambda self: Control(base, "c")))
        with pytest.raises(Refusal) as raised:
            db.commit()
        assert [one["status"] for one in raised.value.report.refusals] == [UNANSWERED]
    finally:
        db.close()
