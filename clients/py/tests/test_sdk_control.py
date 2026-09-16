"""The control-plane client on its own: the retry rule, the batch id and the commit log (§6.4).

The `429` is the one status the SDK retries, and it must resend identical bytes: a batch id maps to
the SHA-256 of the raw request body, so a body re-serialised before the retry would be a new batch
rather than a replay. That is what this file fakes a server for. Everything else about the plane is
tested against a real one.
"""

from __future__ import annotations

import http.server
import json
import threading

from tesseradb._control import CommitLog, Control, addressed, batch_id, external_id


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
        answer = control.ingest(body, batch_id("points", 0, body), view="s0")
    finally:
        server.shutdown()
        server.server_close()
    assert answer.ok and answer.attempts == 3
    # Identical bytes under one batch id: a retry is a replay, never a second batch.
    assert _Backpressure.bodies == [body] * 3
    assert len(set(_Backpressure.batches)) == 1


def test_a_batch_id_is_stable_across_runs_and_moves_with_the_bytes():
    body = b"the same bytes"
    assert batch_id("points", 0, body) == batch_id("points", 0, body)
    assert batch_id("points", 0, body) != batch_id("points", 1, body)
    assert batch_id("points", 0, body) != batch_id("points", 0, b"other bytes")
    assert batch_id("points", 0, body).startswith("points-0-")


def test_an_external_id_is_the_source_id_in_eight_little_endian_bytes():
    assert external_id(1) == b"\x01\x00\x00\x00\x00\x00\x00\x00"
    assert len(external_id(2**63)) == 8
    assert addressed(1) == "AQAAAAAAAAA="


def test_the_commit_log_holds_what_was_acknowledged_and_reopens_with_it(tmp_path):
    path = tmp_path / "commit-log.json"
    log = CommitLog(path)
    assert not log.holds("points-0-abc")
    log.acknowledge("points-0-abc", "points", type("A", (), {"status": 200})())
    log.declare(["clusters", "topics"])
    log.publish("clusters", [("c0", False), ("c0", True)])
    log.add_terms(["public", "public", "cs.LG"])
    log.save()

    reopened = CommitLog(path)
    assert reopened.holds("points-0-abc")
    assert reopened.declared("clusters") and not reopened.declared("other")
    assert reopened.published("clusters") == {"c0": {"content": True}}
    assert reopened.terms == ["public", "cs.LG"]
