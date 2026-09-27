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
    """One request's answer. `Control` returns one for every request, refused or unanswered.

    - `status`: the HTTP status, or 0 where no server answered.
    - `body`: the answer's JSON object. A JSON value that is not an object is under `"value"`,
      and a body that is not JSON gives `{}`.
    - `detail`: the answer's text, or why no answer came.
    - `attempts`: how many times the request was sent. It is more than 1 after a `429`.
    - `seconds`: how long the request took, from the first attempt to the answer.
    """

    status: int
    body: dict = field(default_factory=dict)
    detail: str = ""
    attempts: int = 1
    seconds: float = 0.0

    @property
    def ok(self) -> bool:
        """`True` when the status is in the 200s."""
        return 200 <= self.status < 300


class Control:
    """A client for the control plane of one served database, where the operator writes.

    `Database.control` returns one. Each route method sends one request with the operator
    credential and returns its `Answer`. `status()` and `limits()` return dictionaries, and
    `limits()` sends its request once and keeps the answer. A `429` is sent again after the wait
    it names, at most 30 seconds, up to 600 attempts in all. Every other status is returned as
    it came.

    - `base`: the control plane's address, such as `http://127.0.0.1:41234`.
    - `credential`: the operator credential.
    - `timeout`: how long to wait for one answer, in seconds. The default is 300.
    """

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
        """`GET /control/status`: the server's status report.

        A refusal gives the server's error body, `{"error": ..., "detail": ...}`, and `{}` comes
        only where no server answered.
        """
        return self.status_answer().body

    def status_answer(self) -> Answer:
        """`GET /control/status`, as its `Answer`, so a refusal's status and detail can be read."""
        return self._send("GET", "/control/status")

    def limits(self) -> dict:
        """The `limits` block of `status()`, read once and kept.

        It says, per write route, the most rows, bytes, annotations, members or changes one
        request may carry.
        """
        if self._limits is None:
            self._limits = self.status().get("limits", {})
        return self._limits

    def ingest(self, body: bytes, batch: str, view: str | None = None) -> Answer:
        """`POST /control/ingest`: create items, edit them, or add them to a view.

        A row names an item by its `tessera_id`, its `external_id` or a unique column's value. A
        row naming none creates an item at its position; one carrying what the item stores
        changes nothing; one naming an item with no row in the view adds it there; any other
        edits the item, which keeps its `tessera_id`. A row without coordinates changes only what
        it carries. Any column may be left out, keeping what the item stores, and a null clears
        it. The answer counts the rows `created`, `edited`, `added` and `unchanged`, and in
        `joined` the annotation memberships the rows added, and gives each row's `tessera_id`. A
        row that only places its item in an annotation changes the annotation, not the item, and
        is counted unchanged.

        - `body`: the rows, as an Arrow IPC stream.
        - `batch`: the request's batch id. A retry sends the same id with the same bytes, and the
          server answers it as a replay; the same id with other bytes is refused.
        - `view`: the view the rows are for. It may be left out where the database has one view.
        """
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", "/control/ingest", body, headers)

    def declare_layer(self, payload: dict) -> Answer:
        """`PUT /control/layers`: declare one annotation layer.

        `payload` is the layer's block as `tessera check --payloads` prints it. The answer carries
        the layer's `tessera_id`.
        """
        return self._send(
            "PUT", "/control/layers", json.dumps(payload).encode(), {"content-type": JSON}
        )

    def declare_view_group(self, name: str, body: dict) -> Answer:
        """`PUT /control/view_groups/{name}`: the group, with an empty roster."""
        return self._send("PUT", f"/control/view_groups/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def declare_attribute(self, body: dict) -> Answer:
        """`PUT /control/attributes`: declare one column, named in the body.

        A column declared here cannot be rendered.
        """
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
        """`PUT /control/views/{name}`: declare one view that is not in a view group."""
        return self._send("PUT", f"/control/views/{_segment(name)}", _json(body),
                          {"content-type": JSON})

    def create_view(self, group: str, key: str, body: dict) -> Answer:
        """`PUT /control/views/{group}/{key}`: add one view to a view group.

        `body` is the view's roster record: its metadata, and its `visibility` where it has one.
        """
        return self._send(
            "PUT",
            f"/control/views/{_segment(group)}/{_segment(key)}",
            _json(body),
            {"content-type": JSON},
        )

    def publish(self, layer: str, body: bytes) -> Answer:
        """`PUT /control/layers/{layer}/artifacts`: publish annotations into one level of a layer.

        `body` is the request's JSON, as bytes. The request is applied whole or not at all, and
        the answer gives each annotation's `tessera_id`.
        """
        return self._send("PUT", _artifacts(layer), body, {"content-type": JSON})

    def grow(self, layer: str, body: bytes) -> Answer:
        """`PATCH /control/layers/{layer}/artifacts`: change annotations a level already holds.

        It adds members, fills parts an annotation lacks, and takes items out of a label's
        generating set. `body` is the request's JSON, as bytes. The request is applied whole or
        not at all. A part that differs from the one held is refused with `409`.
        """
        return self._send("PATCH", _artifacts(layer), body, {"content-type": JSON})

    def changes(self, items: list[dict]) -> Answer:
        """`POST /control/changes`: delete, suppress or unsuppress items.

        `items` is one record per item: `op` (`"delete"`, `"suppress"` or `"unsuppress"`) and the
        item's address, as `Database.addresses` gives it. A deletion or suppression applies to
        every request from the moment it is accepted. This route never answers `429`.
        """
        return self._send(
            "POST", "/control/changes", json.dumps(items).encode(), {"content-type": JSON}
        )

    def drop_layer(self, name: str, wait: bool = False) -> Answer:
        """`DELETE /control/layers/{name}`: remove an annotation layer.

        The name cannot be declared again. `wait` holds the answer until readers no longer see
        the layer, as for `flush`.
        """
        return self._send("DELETE", f"/control/layers/{_segment(name)}" + _wait(wait))

    def drop_view(self, group: str, key: str, wait: bool = False) -> Answer:
        """`DELETE /control/views/{group}/{key}`: remove one view of a view group.

        The items it leaves in no view are deleted, and the answer's `deleted` counts them.
        `wait` is as for `flush`.
        """
        return self._send(
            "DELETE", f"/control/views/{_segment(group)}/{_segment(key)}" + _wait(wait)
        )

    def compact(self) -> Answer:
        """`POST /control/compact`: ask for a compaction, which removes deleted items' rows.

        The server answers `202` at once and compacts at its next cycle. A request made while a
        compaction runs is dropped, and the answer cannot say so. The `compaction` block of
        `status()` shows what happened.
        """
        return self._send("POST", "/control/compact", b"")

    def flush(self, wait: bool = False) -> Answer:
        """`POST /control/flush`: bring the next publication forward.

        A publication is the point at which writes become visible to readers. The answer's
        `publication` names the first one certain to include every request sent before this one.
        With `wait`, the answer comes once readers see that publication, or after the server's
        `serve.visible_wait_max_secs` with `visible` set to `false`.
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

    batch = table.combine_chunks().to_batches()
    schema = table.schema
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, schema) as writer:
        for one in batch:
            writer.write_batch(one)
    return sink.getvalue().to_pybytes()
