"""The control plane as the SDK calls it (contracts §3.4, ingest.md §1).

One class per concern: `Control` is the routes, `CommitLog` is what this database has already had
acknowledged. Both are small because the plane is small: seven routes carry every later commit.

**A retry resends identical bytes.** A batch id maps to the SHA-256 of the raw request body, so a
retry that re-serialised its table would be a new batch rather than a replay (contracts §3.4). The
bodies are therefore built once, by the plan, and this module sends the bytes it was given.

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
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

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


def external_id(source_id: int) -> bytes:
    """The external id of a row: the source id in eight little-endian bytes (§3).

    The build mints it under `--mint-external-ids` and the SDK sends the same bytes, so one row is
    one address at both doors.
    """
    return int(source_id).to_bytes(8, "little")


def addressed(source_id: int) -> str:
    """The same id where a JSON route carries it: base64, external ids being bytes and not text."""
    return base64.b64encode(external_id(source_id)).decode()


def batch_id(source: str, index: int, body: bytes) -> str:
    """A page's batch id: the source name, the page index and a hash of the bytes (§6.4).

    Stable across runs, so a cell re-run derives the same id for the same page and the commit log
    recognises it. The hash is of the body that is sent, so two pages that differ in one row differ
    here.
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

    def publication(self) -> tuple[dict[str, int], int, int]:
        """What a publication moves: the segments version, the flush count and the tick count.

        The version is the contract's own figure (`partitions`, contracts §3.4) and says a tick
        published rows. A tick whose only work is filling cells writes no segment and moves no
        version, and one whose only work is publishing artifacts moves neither that nor the flush
        count, so the two counters beside it are read as well. They are outside the endpoint's
        contract and end a wait; they never decide whether a page was accepted.
        """
        status = self.status()
        versions = {
            str(block.get("partition")): int(block.get("segments_version", 0))
            for block in status.get("partitions", [])
        }
        flush = status.get("write_executor", {}).get("flush", {})
        return versions, int(flush.get("flushes", 0)), int(flush.get("ticks", 0))

    def ingest(self, body: bytes, batch: str, view: str | None = None) -> Answer:
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", "/control/ingest", body, headers)

    def values(self, body: bytes, batch: str, view: str | None = None) -> Answer:
        headers = {"content-type": ARROW, "x-tessera-batch-id": batch}
        if view is not None:
            headers["x-tessera-view"] = view
        return self._send("POST", "/control/values", body, headers)

    def declare_layer(self, payload: dict) -> Answer:
        return self._send(
            "PUT", "/control/layers", json.dumps(payload).encode(), {"content-type": JSON}
        )

    def publish(self, layer: str, body: bytes) -> Answer:
        return self._send("PUT", _artifacts(layer), body, {"content-type": JSON})

    def grow(self, layer: str, body: bytes) -> Answer:
        return self._send("PATCH", _artifacts(layer), body, {"content-type": JSON})

    def changes(self, items: list[dict]) -> Answer:
        return self._send(
            "POST", "/control/changes", json.dumps(items).encode(), {"content-type": JSON}
        )

    def flush(self) -> Answer:
        return self._send("POST", "/control/flush", b"")


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


class CommitLog:
    """What this database has had acknowledged, under `.tessera/` (§6.4).

    A page's batch id is recorded here at its acknowledgement, so a cell re-run past the WAL
    retention window skips the page rather than sending it again and relying on the server's replay
    window. Beside the batch ids it holds the layers this database has declared and the artifact
    keys it has published, which the pre-flight reads: a content gated `all` on an artifact
    published without one has no route to fill it, and the plan refuses that case by name.
    """

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self.batches: dict[str, dict] = {}
        self.layers: list[str] = []
        self.keys: dict[str, dict[str, dict]] = {}
        #: The digest of every set this database has sent whole, by layer and `key|rank`. A set is
        #: a delta on the wire, so a page of it re-sent is a lawful no-op; recording what was sent
        #: is what lets a cell re-run send nothing at all.
        self.sets: dict[str, dict[str, str]] = {}
        #: Every access label this database has staged, plus each view's default (§8).
        self.terms: list[str] = []
        if self.path.exists():
            document = json.loads(self.path.read_text(encoding="utf-8"))
            self.batches = document.get("batches", {})
            self.layers = document.get("layers", [])
            self.keys = document.get("keys", {})
            self.sets = document.get("sets", {})
            self.terms = document.get("terms", [])

    def holds(self, batch: str) -> bool:
        return batch in self.batches

    def acknowledge(self, batch: str, what: str, answer: Answer) -> None:
        self.batches[batch] = {"what": what, "status": answer.status}

    def declared(self, layer: str) -> bool:
        return layer in self.layers

    def declare(self, layers: Iterable[str]) -> None:
        for layer in layers:
            if layer not in self.layers:
                self.layers.append(layer)

    def published(self, layer: str) -> dict[str, dict]:
        """The keys this database has published into a layer, each with its content's digest.

        The digest is `None` where the artifact was published without content. A content gated
        `all` has no fill route, so the plan reads this to refuse one on an artifact published
        without it rather than sending a request the server would refuse, and to tell a content
        re-supplied unchanged from one that differs.
        """
        return self.keys.get(layer, {})

    def publish(self, layer: str, keys: Iterable[tuple[str, str | None, str | None]]) -> None:
        held = self.keys.setdefault(layer, {})
        for key, content, parts in keys:
            state = held.setdefault(key, {"content": None, "parts": None})
            state["content"] = state["content"] or content
            state["parts"] = state.get("parts") or parts


    def holds_set(self, layer: str, key: str, rank: int | None, digest: str) -> bool:
        """Whether this database has already sent that set whole, under that digest."""
        return self.sets.get(layer, {}).get(set_key(key, rank)) == digest

    def record_set(self, layer: str, key: str, rank: int | None, digest: str) -> None:
        self.sets.setdefault(layer, {})[set_key(key, rank)] = digest

    def add_terms(self, terms: Iterable[str]) -> None:
        for term in terms:
            if term not in self.terms:
                self.terms.append(term)

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(
            json.dumps(
                {
                    "batches": self.batches,
                    "layers": self.layers,
                    "keys": self.keys,
                    "sets": self.sets,
                    "terms": self.terms,
                },
                indent=1,
            ),
            encoding="utf-8",
        )


def set_key(key: str, rank: int | None) -> str:
    """How a set is named in the log: an artifact's key, and the rank of the content it belongs to.

    A rank of `None` is the membership and a rank of *k* is content *k*'s generating set, which is
    the member table's own grain (annotation-write-cycle §6.1).
    """
    return f"{key}|{'' if rank is None else rank}"


def members_digest(source_ids: Iterable[int]) -> str:
    """A stable digest of the ids a set holds, in the order the caller staged them."""
    return hashlib.sha256(
        b",".join(str(int(i)).encode() for i in source_ids)
    ).hexdigest()[:16]


def parts_digest(parent: Any, attached: Any) -> str | None:
    """A stable digest of an artifact's fixed parts, or `None` where it carries none.

    `parent` and `attached_to` are filled once and never replaced (ingest §1.5), so an artifact
    this database published with them takes no second record carrying the same ones.
    """
    if not parent and not attached:
        return None
    return hashlib.sha256(
        json.dumps({"parent": list(parent or []), "attached_to": attached}, sort_keys=True).encode()
    ).hexdigest()[:16]


def content_digest(content: Any) -> str | None:
    """A stable digest of an artifact's supplied content, or `None` where it carries none.

    A content is a fixed part: supplied once and replaced never. The digest is what lets a later
    commit tell a cell re-run, which supplies the same values again, from a caller supplying
    different ones, which no route can apply.
    """
    if not content:
        return None
    return hashlib.sha256(
        json.dumps([list(values) for values in content], sort_keys=True).encode()
    ).hexdigest()[:16]


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
