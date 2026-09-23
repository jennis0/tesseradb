"""The control plane as the SDK calls it.

`Control` is the routes and nothing else: the SDK keeps no record of what it sent, so what a
database holds is asked of the database.

**A batch id is a fresh id per request, and a retry resends both the id and the bytes.** The id is
the client's choice and says which request this is; the server holds it against the SHA-256 of the
body, so a retry that re-serialised its table would be refused as a different body under a held
id. The bodies are therefore built once, by the plan, and this module sends the bytes it was
given. Two requests carrying the same rows are two requests: nothing derives an id from what
a page contains, so a frame inserted and committed twice lands twice.

**A `429` is backpressure.** The buffer-occupancy refusal carries `Retry-After`, and the caller
waits that long and sends the same bytes again. Every other status is an answer: a `409` on a
differing part is reported and not retried, since an edit is a delete and a re-ingest, and the
SDK does not do that on the user's behalf.

The transport is `urllib`, so the SDK's only dependency outside the standard library stays
pyarrow.
"""

from __future__ import annotations

import base64
import json
import numbers
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
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
    """The external id of a row: the bytes its id column holds.

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


def batch_id(source: str, index: int) -> str:
    """A page's batch id: a fresh random id, made once when the request is built.

    The id is never derived from the request's content. Retries of one attempt inside one
    `commit()`, after a `429`, a timeout or a lost answer, reuse it, and nothing else does.
    Nothing is kept across commits, so the same table inserted and committed five times is
    loaded five times.

    The source name and the page index ride in front of the random half so that an operator
    reading a server log can tell which page a line is about; nothing reads them.
    """
    return f"{source}-{index}-{uuid.uuid4().hex}"


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
                # and the report names the one that was not. **A commit after it is a new
                # request**, under a new id: whether the unanswered page landed is the database's
                # to say, and a client that needs to ask carries an id column.
                return Answer(
                    UNANSWERED,
                    {},
                    f"{self.base + path} did not answer: {unreachable}",
                    attempts,
                    time.monotonic() - started,
                )

    # ------------------------------------------------------------------ the routes

    def status(self) -> dict:
        return self.status_answer().body

    def status_answer(self) -> Answer:
        """`GET /control/status`, with the status beside the body a refusal would leave empty."""
        return self._send("GET", "/control/status")

    def limits(self) -> dict:
        """The pagination units every route publishes, read once and sized from."""
        if self._limits is None:
            self._limits = self.status().get("limits", {})
        return self._limits

    def ingest(self, body: bytes, batch: str, view: str | None = None) -> Answer:
        """`POST /control/ingest`: one page of points, as an Arrow stream."""
        return self._rows("/control/ingest", body, batch, view)

    def values(self, body: bytes, batch: str, view: str | None = None) -> Answer:
        """`POST /control/values`: one page of cells on rows the database holds."""
        return self._rows("/control/values", body, batch, view)

    def _rows(self, path: str, body: bytes, batch: str, view: str | None) -> Answer:
        """The two row routes: the same headers, the view named where the page is of one."""
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", path, body, headers)

    def declare_layer(self, payload: dict) -> Answer:
        return self._send(
            "PUT", "/control/layers", json.dumps(payload).encode(), {"content-type": JSON}
        )

    def declare_view_group(self, name: str, body: dict) -> Answer:
        """`PUT /control/view_groups/{name}`: the group, with an empty roster."""
        return self._send("PUT", f"/control/view_groups/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def declare_attribute(self, body: dict) -> Answer:
        """`PUT /control/attributes`: one column, named in the body rather than in the path."""
        return self._send("PUT", "/control/attributes", _json(body), {"content-type": JSON})

    def declare_vocabulary(self, name: str, body: dict) -> Answer:
        """`PUT /control/vocabularies/{name}`: the value set, with a closed set's values on it."""
        return self._send("PUT", f"/control/vocabularies/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def vocabulary_values(self, name: str, body: dict) -> Answer:
        """`PATCH /control/vocabularies/{name}/values`: one page of `{key, title?}` rows."""
        return self._send(
            "PATCH",
            f"/control/vocabularies/{_segment(name)}/values",
            _json(body),
            {"content-type": JSON},
        )

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

    def drop_layer(self, name: str, wait: bool = False) -> Answer:
        """`DELETE /control/layers/{name}`: the inverse of `declare_layer`.

        The name is tombstoned rather than freed, so a later declaration under it is refused and
        no stale reference reaches a different layer.
        """
        return self._send("DELETE", f"/control/layers/{_segment(name)}" + _wait(wait))

    def drop_view(
        self, group: str, key: str, delete_dangling: bool = False, wait: bool = False
    ) -> Answer:
        """`DELETE /control/views/{group}/{key}`: the inverse of `create_view`.

        Dropping a view deletes no entity. `delete_dangling` submits the entities holding a row in
        no other view as ordinary deletions, which enter the overlay and retire at the fold; the
        answer says how many in `deleted`.
        """
        query = {}
        if delete_dangling:
            query["delete_dangling"] = "true"
        if wait:
            query["wait"] = "visible"
        path = f"/control/views/{_segment(group)}/{_segment(key)}"
        if query:
            path += "?" + urllib.parse.urlencode(query)
        return self._send("DELETE", path)

    def compact(self) -> Answer:
        """`POST /control/compact`: ask for a fold, which is what removes a deletion's rows.

        Accepted rather than performed: the fold runs behind the answer, and it is the same
        dispatch the schedule uses, so asking neither disturbs nor is disturbed by the window.
        """
        return self._send("POST", "/control/compact", b"")

    def flush(self, wait: bool = False) -> Answer:
        """Arm a publication cycle, and with `wait` hold until it has completed.

        The flush's own `?wait=visible` waits on the number the cycle it arms will carry, so it
        covers every page sent before it: this is the commit's one wait.
        """
        return self._send("POST", "/control/flush" + _wait(wait), b"")


def _wait(wait: bool) -> str:
    """`?wait=visible`, where the caller asked for it.

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
    """How long a `429` asks the caller to wait. The header and the body agree."""
    named = headers.get("retry-after") if headers is not None else None
    if named is None:
        named = body.get("retry_after_s")
    try:
        seconds = float(named)
    except (TypeError, ValueError):
        seconds = 1.0
    return max(0.0, min(seconds, MAX_BACKOFF))


def arrow_body(table: Any) -> bytes:
    """One Arrow IPC stream carrying a table, which is what a row route takes."""
    import pyarrow as pa
    import pyarrow.ipc as ipc

    # The build reads large strings, which pandas 3 makes of every string column, and the row
    # routes take `utf8` only.
    for at, column in enumerate(table.schema):
        small = _small_strings(column.type)
        if small != column.type:
            table = table.set_column(
                at, pa.field(column.name, small, column.nullable), table.column(at).cast(small)
            )
    batch = table.combine_chunks().to_batches()
    schema = table.schema
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, schema) as writer:
        for one in batch:
            writer.write_batch(one)
    return sink.getvalue().to_pybytes()


def _small_strings(dtype: Any) -> Any:
    import pyarrow as pa

    if pa.types.is_large_string(dtype):
        return pa.string()
    if (pa.types.is_list(dtype) or pa.types.is_large_list(dtype)) and pa.types.is_large_string(
        dtype.value_type
    ):
        return pa.list_(pa.string())
    return dtype
