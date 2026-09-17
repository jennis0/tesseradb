"""The control plane as the SDK calls it (contracts §3.4, ingest.md §1).

`Control` is the routes and nothing else: the SDK keeps no record of what it sent, so what a
database holds is asked of the database (§6.4).

**A retry resends identical bytes.** A batch id maps to the SHA-256 of the raw request body, so a
retry that re-serialised its table would be a new batch rather than a replay (contracts §3.4). The
bodies are therefore built once, by the plan, and this module sends the bytes it was given. The id
is derived from the page rather than remembered, so the `429` retry and the resend of a lost
acknowledgement within one `commit()` carry the id the first attempt carried.

**A `429` is backpressure.** The buffer-occupancy refusal carries `Retry-After`, and the caller
waits that long and sends the same bytes again. Every other status is an answer: a `409` on a
differing part is reported and not retried, since an edit is a delete and a re-ingest
(decision 0047) and the SDK does not do that on the user's behalf.

The transport is `urllib`, so the SDK's only dependency outside the standard library stays
pyarrow.
"""

from __future__ import annotations

import base64
import hashlib
import json
import numbers
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any

ARROW = "application/vnd.apache.arrow.stream"
JSON = "application/json"

#: How long a `429` may hold one page, in seconds. The server's own figure is clamped to 1 to
#: 300 s, and the SDK waits at most this long between attempts.
MAX_BACKOFF = 30.0

#: How many times one page is re-sent after a `429` before the SDK reports it as a refusal.
MAX_ATTEMPTS = 600

#: The status an answer carries when the request reached no server at all. Not an HTTP status: the
#: request was never answered, so there is none to report.
UNANSWERED = 0


def external_id(value: Any) -> bytes:
    """The external id of a row: the bytes its id column holds (§3, configuration.md §8).

    A string's UTF-8, an integer's eight little-endian bytes, binary as it stands. The build reads
    the same column and takes the same bytes, so one row is one address at both doors.

    Any integer, not only Python's: a frame's own ids arrive as numpy or pyarrow scalars, and one
    of those spelled as text would address a row nobody wrote. A boolean is not an integer here,
    having no id space of its own.
    """
    if isinstance(value, bytes):
        return value
    if isinstance(value, bool) or not isinstance(value, numbers.Integral):
        return str(value).encode()
    number = int(value)
    return number.to_bytes(8, "little", signed=number < 0)


def addressed(value: Any) -> str:
    """The same id where a JSON route carries it: base64, external ids being bytes and not text."""
    return base64.b64encode(external_id(value)).decode()


def batch_id(source: str, index: int, body: bytes) -> str:
    """A page's batch id: the source name, the page index and a hash of the bytes (§6.4).

    Stable across runs, so a page re-sent after a `429` or a lost acknowledgement carries the id
    the first attempt carried and the server answers it as a replay. The hash is of the body that
    is sent, so two pages that differ in one row differ here.
    """
    digest = hashlib.sha256(body).hexdigest()[:16]
    return f"{source}-{index}-{digest}"


@dataclass
class Answer:
    """One request's answer: the status, the decoded body where there was one, and the detail."""

    status: int
    body: dict = field(default_factory=dict)
    detail: str = ""
    attempts: int = 1
    seconds: float = 0.0

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


class Control:
    """The operator plane of one served database."""

    def __init__(self, base: str, credential: str, timeout: float = 300.0) -> None:
        self.base = base.rstrip("/")
        self.credential = credential
        self.timeout = timeout
        self._limits: dict | None = None

    # ------------------------------------------------------------------ the transport

    def _send(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        headers: dict | None = None,
    ) -> Answer:
        head = {"authorization": f"Bearer {self.credential}"}
        head.update(headers or {})
        request = urllib.request.Request(
            self.base + path, data=body, method=method, headers=head
        )
        started = time.monotonic()
        attempts = 0
        while True:
            attempts += 1
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    text = response.read().decode(errors="replace")
                    return Answer(
                        response.status,
                        _decoded(text),
                        text,
                        attempts,
                        time.monotonic() - started,
                    )
            except urllib.error.HTTPError as refusal:
                text = refusal.read().decode(errors="replace")
                if refusal.code == 429 and attempts < MAX_ATTEMPTS:
                    time.sleep(_retry_after(refusal.headers, _decoded(text)))
                    continue
                return Answer(
                    refusal.code, _decoded(text), text, attempts, time.monotonic() - started
                )
            except (urllib.error.URLError, OSError) as unreachable:
                # A connection that never answered is a refusal the report carries, not an
                # exception out of `commit()`: the pages already acknowledged stay acknowledged,
                # and an unanswered request is resent with identical bytes at the next commit.
                return Answer(
                    UNANSWERED,
                    {},
                    f"{self.base + path} did not answer: {unreachable}",
                    attempts,
                    time.monotonic() - started,
                )

    # ------------------------------------------------------------------ the routes

    def status(self) -> dict:
        answer = self._send("GET", "/control/status")
        return answer.body

    def limits(self) -> dict:
        """The pagination units every route publishes, read once and sized from (ingest §2.1)."""
        if self._limits is None:
            self._limits = self.status().get("limits", {})
        return self._limits

    def ingest(
        self, body: bytes, batch: str, view: str | None = None
    ) -> Answer:
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", "/control/ingest", body, headers)

    def values(
        self, body: bytes, batch: str, view: str | None = None
    ) -> Answer:
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", "/control/values", body, headers)

    def declare_layer(self, payload: dict) -> Answer:
        return self._send(
            "PUT", "/control/layers", json.dumps(payload).encode(), {"content-type": JSON}
        )

    def declare_view_group(self, name: str, body: dict) -> Answer:
        """`PUT /control/view_groups/{name}`: the group, with an empty roster (contracts §3.4)."""
        return self._send("PUT", f"/control/view_groups/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def declare_view(self, name: str, body: dict) -> Answer:
        """`PUT /control/views/{name}`: one plain view while the service runs."""
        return self._send("PUT", f"/control/views/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def create_view(self, group: str, key: str, body: dict) -> Answer:
        """`PUT /control/views/{group}/{key}`: the roster record, which creates the view."""
        return self._send(
            "PUT",
            f"/control/views/{_segment(group)}/{_segment(key)}",
            _json(body),
            {"content-type": JSON},
        )

    def publish(self, layer: str, body: bytes) -> Answer:
        return self._send("PUT", _artifacts(layer), body, {"content-type": JSON})

    def grow(self, layer: str, body: bytes) -> Answer:
        return self._send("PATCH", _artifacts(layer), body, {"content-type": JSON})

    def changes(self, items: list[dict]) -> Answer:
        return self._send(
            "POST", "/control/changes", json.dumps(items).encode(), {"content-type": JSON}
        )

    def flush(self, wait: bool = False) -> Answer:
        """Arm a publication cycle, and with `wait` hold until it has completed.

        The flush's own `?wait=visible` waits on the number the cycle it arms will carry, so it
        covers every page sent before it: this is the commit's one wait (decision 0144).
        """
        return self._send("POST", "/control/flush" + _wait(wait), b"")


def _wait(wait: bool) -> str:
    """`?wait=visible` (contracts §3.4, decision 0144), where the caller asked for it.

    The route holds its answer until the publication its acknowledgement names has happened, and
    pulls the tick forward to get there. One request of a commit carries it, the closing flush: a
    waited request asks for a tick of its own, so a commit that waited on every page would publish
    per page.
    """
    return "?wait=visible" if wait else ""


def _json(body: dict) -> bytes:
    return json.dumps(body).encode()


def _segment(name: str) -> str:
    """One path segment, percent-encoded: a name the router must see whole."""
    return urllib.parse.quote(name, safe="")


def _artifacts(layer: str) -> str:
    """The artifacts route of one layer, its name percent-encoded.

    A layer name is path-shaped here (`clusters/kmeans`) and the route matches one segment, so an
    unencoded name is a 404 at the router rather than a refusal from the handler.
    """
    return f"/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts"


def _decoded(text: str) -> dict:
    try:
        value = json.loads(text)
    except ValueError:
        return {}
    return value if isinstance(value, dict) else {"value": value}


def _retry_after(headers, body: dict) -> float:
    """How long a `429` asks the caller to wait. The header and the body agree (contracts §3.4)."""
    named = headers.get("retry-after") if headers is not None else None
    if named is None:
        named = body.get("retry_after_s")
    try:
        seconds = float(named)
    except (TypeError, ValueError):
        seconds = 1.0
    return max(0.0, min(seconds, MAX_BACKOFF))


def arrow_body(table: Any) -> bytes:
    """One Arrow IPC stream carrying a table, which is what a row route takes (ingest §1.2)."""
    import pyarrow as pa
    import pyarrow.ipc as ipc

    batch = table.combine_chunks().to_batches()
    schema = table.schema
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, schema) as writer:
        for one in batch:
            writer.write_batch(one)
    return sink.getvalue().to_pybytes()
