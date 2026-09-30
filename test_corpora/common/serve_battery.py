"""The serve battery — view latency across a principal ladder, hot and cold, on one schema.

It drives a running `tessera serve` and writes the `serve` block of the rung's
`measurements.json`.

Each cell is measured under three conditions, stamped in every row:

* `cold` — bundle and cache pages evicted and a fresh session before every sample.
* `cold_pages_warm_engine` — the session's fragments built once, then pages evicted before
  every sample, isolating the storage cost from the per-session build.
* `hot` — one location repeated, nothing evicted.

The two cold conditions draw 40 distinct locations from the cell's own decile, since repeating a
location warms it. Locations are drawn by density decile, and each cell carries the density it
was drawn at, since the cost of a viewport is a function of how many points are in it.

A request is made at the budget's depth rather than the sample's own zoom, since a client spreads
its point budget over the tiles a viewport covers, and `k` is the deployment's own `max_k`.

Eviction is `posix_fadvise(DONTNEED)` over the bundle and cache files plus `memory.reclaim` on the
server's cgroup, and neither is guaranteed. Each cold sample reads the server's `majflt` either
side of its request; a sample with no major-fault delta is excluded from the headline percentiles
and counted in `eviction_failed` instead.

    python -m test_corpora.common.serve_battery \\
        --viewer http://127.0.0.1:8151 --session http://127.0.0.1:8152 \\
        --session-cred "$CRED" --bundle /path/to/bundle --cache /path/to/.tessera/cache \\
        --ranks /path/to/branch-ranks.json --server-pid 1234 --out serve-nocap.json \\
        [--cap-bytes 6442450944 --cgroup /sys/fs/cgroup/user.slice/.../scope]
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import math
import os
import random
import statistics
import sys
import time
from pathlib import Path
from typing import Callable, Iterable, Sequence

import requests
import urllib3
from pyarrow import ipc

#: The percentiles every cell reports, p25 and p75 included to show a cold cell's spread.
PERCENTILES = (25, 50, 75, 95, 99)

#: How much deeper than its own zoom a request is made: depth 9 covers 4**9 = 262,144 tiles,
#: `serve.max_tiles_per_request`'s default.
BUDGET_DEPTH = 9

#: The Morton grid has sixteen levels, so no request can be made below depth 16.
GRID_DEPTH = 16

#: `k` where `/v1/meta` publishes no `selection.max_k`. Every rung's deployment declares 5000.
DEFAULT_K = 5000

#: `max_tiles_per_request` where `/v1/meta` publishes none: the server's own default.
DEFAULT_MAX_TILES = 262_144

#: `k` on a ranking request. `k = 0` is the counts-only request: a decile ranks `visible`, which
#: no `k` changes.
RANK_K = 0

#: The frame kinds this module reads.
FRAME_TILES = 1
FRAME_TRAILER = 4

#: What a connection aborted part-way through a body raises, caught around the read alone.
SHED_ERRORS = (
    requests.exceptions.ChunkedEncodingError,
    requests.exceptions.ConnectionError,
    urllib3.exceptions.ProtocolError,
)


# ---------------------------------------------------------------------------------------------
# The wire
# ---------------------------------------------------------------------------------------------


def authorise(session_base: str, cred: str, terms: Sequence[str], timeout: float = 60.0):
    """`session/authorise` for one term set. Returns `(token, wall seconds)`."""
    auth_data = base64.b64encode(json.dumps({"terms": list(terms)}).encode()).decode()
    t0 = time.perf_counter()
    r = requests.post(
        f"{session_base}/session/authorise",
        headers={"Authorization": f"Bearer {cred}"},
        json={"auth_data": auth_data},
        timeout=timeout,
    )
    dt = time.perf_counter() - t0
    r.raise_for_status()
    return r.json()["token"], dt


def meta(viewer_base: str, token: str, timeout: float = 30.0) -> dict:
    r = requests.get(
        f"{viewer_base}/v1/meta",
        headers={"Authorization": f"Bearer {token}"},
        timeout=timeout,
    )
    r.raise_for_status()
    return r.json()


def frames(content: bytes) -> list[tuple[int, bytes]]:
    """The response's frames as `(kind, payload)` in wire order: `u8 kind, u32 little-endian
    length, payload`, repeated. A body cut mid-frame yields only the frames that were whole.
    """
    out: list[tuple[int, bytes]] = []
    at = 0
    while at + 5 <= len(content):
        kind = content[at]
        length = int.from_bytes(content[at + 1 : at + 5], "little")
        if at + 5 + length > len(content):
            break
        out.append((kind, content[at + 5 : at + 5 + length]))
        at += 5 + length
    return out


def response_figures(content: bytes) -> dict:
    """What the response counted, served and whether it arrived whole: the tiles frame's
    `visible`/`matched` are the masked counts, `served` is what the sweep selected."""
    counts = None
    trailer: dict | None = None
    received = frames(content)
    for kind, payload in received:
        if kind == FRAME_TILES:
            table = ipc.open_stream(io.BytesIO(payload)).read_all()
            names = table.column_names
            counts = {
                name: sum(int(v) for v in table.column(name).to_pylist())
                if name in names
                else None
                for name in ("visible", "matched", "served")
            }
            counts["n_tiles"] = table.num_rows
        elif kind == FRAME_TRAILER:
            trailer = json.loads(payload.decode())
    trailer = trailer or {}
    stream_us = trailer.get("stream_us")
    return {
        "counts": counts,
        "stream_ms": None if stream_us is None else stream_us / 1000.0,
        "points_delivered": trailer.get("points"),
        "flushes": trailer.get("flushes"),
        "frames_received": len(received),
        "complete": bool(trailer),
    }


def full_box(quant: dict) -> list[float]:
    """A view's whole extent as a bbox, from its quantisation."""
    return [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]


def drain(r: requests.Response) -> tuple[bytes, str | None]:
    """The body as it arrived, and the error where the server cut it mid-stream."""
    shed_error = None
    content = bytearray()
    try:
        r.raise_for_status()
        try:
            for chunk in r.iter_content(chunk_size=1 << 20):
                content.extend(chunk)
        except SHED_ERRORS as e:  # noqa: BLE001 — the cut is the measurement
            shed_error = f"{type(e).__name__}: {e}"[:300]
    finally:
        r.close()
    return bytes(content), shed_error


def viewport_request(
    viewer_base: str,
    token: str,
    view_id: str,
    zoom: int,
    bbox: Sequence[float],
    k: int,
    filters: dict | None = None,
    layers: str | None = "all",
    timeout: float = 300.0,
) -> requests.Response:
    """`POST /v1/viewport`, answered as a stream whose body the caller reads."""
    body: dict = {"view": view_id, "zoom": zoom, "bbox": list(bbox), "k": k}
    if layers is not None:
        body["layers"] = layers
    if filters is not None:
        body["filters"] = filters
    return requests.post(
        f"{viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=timeout,
        stream=True,
    )


def viewport(
    viewer_base: str,
    token: str,
    view_id: str,
    zoom: int,
    bbox: Sequence[float],
    k: int,
    filters: dict | None = None,
    layers: str | None = "all",
    timeout: float = 300.0,
    keep_body: bool = False,
) -> dict:
    """One `/v1/viewport` at the depth and `k` the caller has already chosen. A stream the
    server cuts mid-body is a result, not an exception: read chunk by chunk ([`drain`]) and
    returned with `shed` set; a shed sample still carries exact counts. `keep_body` adds the
    frames as they arrived under `body`, for a caller reading a frame this module does not
    summarise; a sample that is written to a result file asks for figures only."""
    t0 = time.perf_counter()
    r = viewport_request(viewer_base, token, view_id, zoom, bbox, k, filters, layers, timeout)
    content, shed_error = drain(r)
    wall = time.perf_counter() - t0
    out = {
        # Time to the cut, if there was one.
        "wall_ms": wall * 1000.0,
        # Time from admission to the sweep's end: the trailer's `stream_us` covers the rest.
        "server_ms": int(r.headers.get("x-tessera-server-us", "0")) / 1000.0,
        "bytes": len(content),
        "request_zoom": zoom,
        "k": k,
        "shed_error": shed_error,
    }
    out.update(response_figures(content))
    out["shed"] = shed_error is not None or not out["complete"]
    if keep_body:
        out["body"] = content
    return out


def _cell_index(v: float, lo: float, hi: float) -> int:
    """`tessera-spatial`'s 16-bit quantiser: the cell a coordinate falls in, clamped at both ends."""
    scaled = math.floor((v - lo) / (hi - lo) * 65536.0)
    if scaled <= 0:
        return 0
    return 65535 if scaled >= 65535 else int(scaled)


def covered_tiles(quantisation: dict, bbox: Sequence[float], depth: int) -> int:
    """How many depth-`depth` tiles `bbox` covers, as `tessera-spatial` counts them: the engine
    refuses a request above `max_tiles_per_request`, so the battery asks a shallower depth first.
    """
    if depth <= 0:
        return 1
    shift = 16 - depth
    x0, y0, x1, y1 = bbox
    xs = sorted(
        (
            _cell_index(x0, quantisation["x_min"], quantisation["x_max"]) >> shift,
            _cell_index(x1, quantisation["x_min"], quantisation["x_max"]) >> shift,
        )
    )
    ys = sorted(
        (
            _cell_index(y0, quantisation["y_min"], quantisation["y_max"]) >> shift,
            _cell_index(y1, quantisation["y_min"], quantisation["y_max"]) >> shift,
        )
    )
    return (xs[1] - xs[0] + 1) * (ys[1] - ys[0] + 1)


def budget_zoom(
    quantisation: dict,
    bbox: Sequence[float],
    zoom: int,
    budget_depth: int = BUDGET_DEPTH,
    max_tiles: int = DEFAULT_MAX_TILES,
) -> int:
    """The depth a client spending its point budget on `bbox` asks at: `zoom + budget_depth`,
    held to sixteen levels and reduced while the box covers more tiles than one request allows."""
    depth = min(GRID_DEPTH, zoom + budget_depth)
    while depth > 0 and covered_tiles(quantisation, bbox, depth) > max_tiles:
        depth -= 1
    return depth


# ---------------------------------------------------------------------------------------------
# Eviction, and the proof it worked
# ---------------------------------------------------------------------------------------------


class Evictor:
    """Drops the bundle's and cache's pages, and reads the server's major-fault counter, without
    root: `posix_fadvise(POSIX_FADV_DONTNEED)` needs only read access to the file.
    """

    def __init__(
        self,
        roots: Sequence[Path],
        server_pid: int | None,
        cgroup: Path | None,
        reclaim_bytes: int = 8 * 1024**3,
    ):
        self.roots = [Path(r) for r in roots if r]
        self.server_pid = server_pid
        self.cgroup = Path(cgroup) if cgroup else None
        self.reclaim_bytes = reclaim_bytes
        self.evictions = 0

    def _files(self) -> Iterable[Path]:
        for root in self.roots:
            if root.is_file():
                yield root
            elif root.is_dir():
                for path in root.rglob("*"):
                    if path.is_file():
                        yield path

    def evict(self) -> None:
        """Drop every page we can."""
        for path in self._files():
            try:
                fd = os.open(path, os.O_RDONLY)
            except OSError:
                continue
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            except OSError:
                pass
            finally:
                os.close(fd)
        if self.cgroup is not None:
            # Sized to the charge read fresh each call, the only mechanism here that reaches
            # pages the server holds mapped.
            try:
                charged = int((self.cgroup / "memory.current").read_text().strip())
            except OSError:
                charged = self.reclaim_bytes
            try:
                with open(self.cgroup / "memory.reclaim", "w") as f:
                    f.write(f"{max(charged, 1 << 20)}\n")
            except OSError:
                pass
        self.evictions += 1

    def majflt(self) -> int | None:
        """The server's process-wide major-fault count (`/proc/<pid>/stat` field 12)."""
        if self.server_pid is None:
            return None
        try:
            raw = Path(f"/proc/{self.server_pid}/stat").read_text()
        except OSError:
            return None
        # Everything after the last ')' is positionally safe; `majflt` is index 9 of that tail.
        tail = raw[raw.rindex(")") + 2 :].split()
        return int(tail[9])

    def cgroup_events(self) -> dict | None:
        if self.cgroup is None:
            return None
        out: dict = {}
        for name in ("memory.events", "memory.stat", "memory.peak", "memory.current"):
            try:
                text = (self.cgroup / name).read_text()
            except OSError:
                continue
            if name in ("memory.peak", "memory.current"):
                out[name] = int(text.strip())
            else:
                fields = dict(
                    line.split(maxsplit=1) for line in text.splitlines() if " " in line
                )
                if name == "memory.events":
                    out[name] = {k: int(v) for k, v in fields.items()}
                else:
                    out["anon"] = int(fields.get("anon", "0"))
                    out["file"] = int(fields.get("file", "0"))
        return out


# ---------------------------------------------------------------------------------------------
# The principal ladder
# ---------------------------------------------------------------------------------------------


def compose_ladder(ranks: list[dict], total_rows: int, targets: Sequence[float]) -> list[dict]:
    """Greedy term sets for each target fraction, from `branch-ranks.json`'s pairs (an upper
    bound on entities): fills descending by size under the budget, then compares the smallest
    overshoot against the total by ratio, so a rung lands within an order of magnitude."""
    ladder = []
    for target in targets:
        if target >= 1.0:
            ladder.append(
                {"target": target, "terms": sorted(r["term"] for r in ranks), "rule": "all"}
            )
            continue
        budget = target * total_rows
        chosen: list[str] = []
        total = 0
        for entry in sorted(ranks, key=lambda r: -r["pairs"]):
            if total + entry["pairs"] <= budget:
                chosen.append(entry["term"])
                total += entry["pairs"]
        remaining = [r for r in sorted(ranks, key=lambda r: r["pairs"]) if r["term"] not in chosen]
        rule = "fill"
        if remaining:
            over = total + remaining[0]["pairs"]
            under = max(total, 1)
            if abs(math.log(over / budget)) < abs(math.log(under / budget)):
                chosen.append(remaining[0]["term"])
                total = over
                rule = "fill+smallest-overshoot"
        ladder.append(
            {"target": target, "terms": sorted(chosen), "rule": rule, "target_pairs": total}
        )
    return ladder


# ---------------------------------------------------------------------------------------------
# Locations, drawn by density decile
# ---------------------------------------------------------------------------------------------


def candidate_boxes(quantisation: dict, zoom: int, n: int, rng: random.Random) -> list[list[float]]:
    """`n` boxes at one zoom, uniformly placed over the extent, span shrinking with zoom to a
    floor at zoom 6, drawn per call rather than fixed so the deciles have something to rank.
    """
    x0, y0 = quantisation["x_min"], quantisation["y_min"]
    x1, y1 = quantisation["x_max"], quantisation["y_max"]
    w, h = x1 - x0, y1 - y0
    span_w = w / (2 ** min(zoom, 6))
    span_h = h / (2 ** min(zoom, 6))
    boxes = []
    for _ in range(n):
        cx = x0 + rng.random() * w
        cy = y0 + rng.random() * h
        boxes.append([cx - span_w / 2, cy - span_h / 2, cx + span_w / 2, cy + span_h / 2])
    return boxes


def rank_by_density(
    viewer_base: str,
    token: str,
    view_id: str,
    zoom: int,
    boxes: Sequence[Sequence[float]],
    quantisation: dict,
    budget_depth: int,
    max_tiles: int,
    log: Callable[[str], None],
) -> list[tuple[list[float], int]]:
    """`(box, visible)` sorted ascending by `visible`, under the 100% principal, at the depth
    the cell's samples will ask at, at `RANK_K` and nothing else."""
    ranked = []
    for i, box in enumerate(boxes):
        depth = budget_zoom(quantisation, box, zoom, budget_depth, max_tiles)
        s = viewport(viewer_base, token, view_id, depth, box, k=RANK_K, layers=None)
        ranked.append((list(box), int((s["counts"] or {}).get("visible") or 0)))
        if i and i % 200 == 0:
            log(f"    ranked {i}/{len(boxes)} at zoom {zoom}")
    ranked.sort(key=lambda p: p[1])
    return ranked


def decile_pools(ranked: Sequence[tuple[list[float], int]]) -> list[list[tuple[list[float], int]]]:
    """Split a ranked list into ten equal pools, decile 0 the sparsest."""
    n = len(ranked)
    return [list(ranked[(d * n) // 10 : ((d + 1) * n) // 10]) for d in range(10)]


# ---------------------------------------------------------------------------------------------
# Percentiles
# ---------------------------------------------------------------------------------------------


def percentiles(values: Sequence[float]) -> dict:
    """p25/p50/p75/p95/p99 and max, by nearest rank rather than interpolation, which over a
    small n would report a number no request took."""
    if not values:
        return {f"p{p}": None for p in PERCENTILES} | {"max": None, "mean": None, "n": 0}
    s = sorted(values)
    out = {}
    for p in PERCENTILES:
        idx = min(int(math.ceil(p / 100.0 * len(s))) - 1, len(s) - 1)
        out[f"p{p}"] = round(s[max(idx, 0)], 4)
    out["max"] = round(s[-1], 4)
    out["mean"] = round(statistics.fmean(s), 4)
    out["n"] = len(s)
    return out


#: The per-sample figures a cell reports percentiles over: `server_ms` is time to first flush,
#: `stream_ms` to the last, and `served`/`response_bytes` are what the emit moved.
SAMPLE_FIGURES = ("server_ms", "stream_ms", "wall_ms", "served", "response_bytes")


def cell_figures(samples: Sequence[dict]) -> dict:
    """Server-side, whole-stream and end-to-end percentiles, separately, over the complete
    samples only: a shed stream's wall time is the time to the cut, counted instead of pooled."""
    ok = [s for s in samples if not s.get("failed") and not s.get("shed")]
    out = {
        "server_ms": percentiles([s["server_ms"] for s in ok if s.get("server_ms") is not None]),
        "stream_ms": percentiles([s["stream_ms"] for s in ok if s.get("stream_ms") is not None]),
        "wall_ms": percentiles([s["wall_ms"] for s in ok if s.get("wall_ms") is not None]),
        "served": percentiles(
            [
                (s.get("counts") or {}).get("served")
                for s in ok
                if (s.get("counts") or {}).get("served") is not None
            ]
        ),
        "response_bytes": percentiles([s["bytes"] for s in ok if s.get("bytes") is not None]),
    }
    out["failed"] = sum(1 for s in samples if s.get("failed"))
    out["shed"] = sum(1 for s in samples if s.get("shed") and not s.get("failed"))
    return out


def condition_figures(samples: Sequence[dict], cold: bool) -> dict:
    """One condition's figures, with the proven-cold samples separated from the rest: a zero
    delta under `cold_pages_warm_engine` can be genuine, while under `cold` it is more likely a
    failed eviction. `all` is kept over every sample regardless."""
    complete = [s for s in samples if not s.get("failed") and not s.get("shed")]
    proven = (
        [s for s in complete if s.get("majflt_delta") not in (None, 0)] if cold else complete
    )
    out = cell_figures(proven)
    out["all"] = cell_figures(samples)
    out["n_samples"] = len(samples)
    out["proven_cold"] = len(proven) if cold else None
    out["eviction_failed"] = (
        sum(1 for s in samples if s.get("eviction_failed")) if cold else 0
    )
    # A response with no trailer is a stream the server cut mid-body, counted rather than dropped.
    out["shed"] = sum(1 for s in samples if s.get("shed") and not s.get("failed"))
    # The counts frame is first on the wire, so a shed sample carries them exactly.
    first = next((s for s in samples if s.get("counts")), None)
    out["visible"] = first["counts"]["visible"] if first else None
    out["matched"] = first["counts"]["matched"] if first else None
    out["occupied_tiles"] = first["counts"]["n_tiles"] if first else None
    out["request_zoom"] = first.get("request_zoom") if first else None
    out["k"] = first.get("k") if first else None
    return out


# ---------------------------------------------------------------------------------------------
# The battery
# ---------------------------------------------------------------------------------------------


def _log(message: str) -> None:
    """Line-buffered by hand: Python block-buffers stdout when it is not a terminal, and a long
    run is watched through its redirected log."""
    print(message, flush=True)


class Battery:
    def __init__(self, args, log: Callable[[str], None] = _log):
        self.args = args
        self.log = log
        self.evictor = Evictor(
            [Path(args.bundle)] + ([Path(args.cache)] if args.cache else []),
            args.server_pid,
            Path(args.cgroup) if args.cgroup else None,
        )
        self.rng = random.Random(args.seed)
        self.oom_seen = False
        self.failures = 0
        self.died: dict | None = None
        # The sample shape, from the flags alone.
        self.zooms = [int(z) for z in args.zooms.split(",")]
        self.deciles = [int(d) for d in args.deciles.split(",")]
        self.conditions = args.conditions.split(",")
        self.cold_samples = args.cold_samples or args.samples
        # The request shape, settled in [`Battery.request_shape`] from `/v1/meta`.
        self.view_id = ""
        self.quant: dict = {}
        self.max_tiles = DEFAULT_MAX_TILES
        self.k = args.k if args.k is not None else DEFAULT_K
        #: The density deciles, ranked once under the 100% principal, by zoom.
        self.pools: dict[int, list[list[tuple[list[float], int]]]] = {}

    def _depth(self, box: Sequence[float], zoom: int) -> int:
        """The depth a box at `zoom` is asked at, under this deployment's own tile ceiling."""
        return budget_zoom(self.quant, box, zoom, self.args.budget_depth, self.max_tiles)

    def _fresh_token(self, terms: Sequence[str]) -> str | None:
        """A new session. `None` once the server has died — the caller stops rather than raising."""
        try:
            token, _ = authorise(self.args.session, self.args.session_cred, terms)
            return token
        except requests.exceptions.RequestException as e:
            self._record_death("session/authorise", e, terms)
            return None

    def _record_death(self, during: str, error: Exception, terms: Sequence[str]) -> None:
        if self.died is None:
            events = self.evictor.cgroup_events() or {}
            self.died = {
                "during": during,
                "principal_terms": list(terms),
                "error": f"{type(error).__name__}: {error}"[:400],
                "cgroup_events": events.get("memory.events"),
                "oom_killed": bool(events.get("memory.events", {}).get("oom_kill")),
            }

    def _watch_oom(self) -> None:
        events = self.evictor.cgroup_events()
        if events and events.get("memory.events", {}).get("oom_kill", 0):
            self.oom_seen = True

    def _sample(self, token, zoom, box, cold: bool, **kw) -> dict:
        """One request, with the cold proof around it when asked for. A failure is a sample
        with a `failed` field, not the end of the run; a shed stream is marked `shed` instead."""
        depth = self._depth(box, zoom)
        before = self.evictor.majflt() if cold else None
        t0 = time.perf_counter()
        try:
            s = viewport(self.args.viewer, token, self.view_id, depth, box, k=self.k, **kw)
        except Exception as e:  # noqa: BLE001 — the failure is the measurement
            self.failures += 1
            return {
                "failed": f"{type(e).__name__}: {e}"[:300],
                "wall_ms": (time.perf_counter() - t0) * 1000.0,
                "server_ms": None,
                "stream_ms": None,
                "bytes": None,
                "counts": None,
                "request_zoom": depth,
                "k": self.k,
                "shed": False,
                "shed_error": None,
                "complete": False,
                "frames_received": 0,
                "majflt_delta": None,
                "eviction_failed": False,
            }
        after = self.evictor.majflt() if cold else None
        if cold:
            delta = None if before is None or after is None else after - before
            s["majflt_delta"] = delta
            s["eviction_failed"] = delta == 0
        return s

    def _cold_samples(self, terms, zoom, pool, n, fresh_session: bool) -> list[dict]:
        """`n` samples, a distinct location each, evicted before every one; the pool wraps round
        where it holds fewer than `n`, recorded in `distinct_locations`.
        """
        token = None
        if not fresh_session:
            token = self._fresh_token(terms)
            if token is None:
                return []
            # Build this session's fragments once, on a location that is not one of the samples.
            self._sample(token, zoom, pool[0][0], cold=False)
        out = []
        for i in range(n):
            box = pool[i % len(pool)][0]
            self.evictor.evict()
            if fresh_session:
                token = self._fresh_token(terms)
            if token is None:
                break
            s = self._sample(token, zoom, box, cold=True)
            s["location"] = i % len(pool)
            out.append(s)
            self._watch_oom()
        return out

    def _hot_samples(self, token, zoom, box, n) -> list[dict]:
        return [self._sample(token, zoom, box, cold=False) for _ in range(n)]

    def request_shape(self, token: str) -> dict:
        """Settle the view, its frame and the deployment's ceilings from `/v1/meta`. Returns
        the `selection` block it read."""
        m = meta(self.args.viewer, token)
        self.view_id = self.args.view or m["views"][0]["id"]
        self.quant = next(v for v in m["views"] if v["id"] == self.view_id)["quantisation"]
        selection = m.get("selection") or {}
        self.max_tiles = int(selection.get("max_tiles_per_request") or DEFAULT_MAX_TILES)
        if self.args.k is None:
            self.k = int(selection.get("max_k") or DEFAULT_K)
        return selection

    def whole_extent(self, token: str) -> dict:
        """One budget request over the whole extent, logged."""
        box = full_box(self.quant)
        whole = viewport(
            self.args.viewer, token, self.view_id, self._depth(box, 0), box, k=self.k, layers=None
        )
        counts = whole["counts"] or {}
        served, occupied = int(counts.get("served") or 0), int(counts.get("n_tiles") or 0)
        self.log(
            f"  100% principal sees {int(counts.get('visible') or 0):,} over the whole extent; "
            f"the budget request at depth {whole['request_zoom']} with k={self.k} serves "
            f"{served:,} points over {occupied:,} occupied tiles "
            f"({served / max(occupied, 1):.1f} a tile) in "
            f"{(whole['bytes'] or 0) / 1e6:.1f} MB, first flush {whole['server_ms']:.0f} ms, "
            f"stream {(whole['stream_ms'] or 0):.0f} ms"
            + (f" — SHED after {whole['wall_ms']:.0f} ms" if whole["shed"] else "")
        )
        return whole

    def rank_deciles(self, token: str) -> None:
        """Fill `pools`: the density deciles at each zoom, ranked once under the 100% principal
        and reused by every rung."""
        for zoom in self.zooms:
            boxes = candidate_boxes(self.quant, zoom, self.args.candidates, self.rng)
            ranked = rank_by_density(
                self.args.viewer,
                token,
                self.view_id,
                zoom,
                boxes,
                self.quant,
                self.args.budget_depth,
                self.max_tiles,
                self.log,
            )
            self.pools[zoom] = decile_pools(ranked)
            self.log(
                f"  zoom {zoom}: visible over {self.args.candidates} candidates "
                f"min={ranked[0][1]:,} median={ranked[len(ranked)//2][1]:,} max={ranked[-1][1]:,}"
            )

    def measure_cell(self, token, terms, zoom: int, decile: int, which: int) -> dict:
        """One cell: one location of one decile, under each condition. Two cells in one decile
        are two orderings of the same pool, so their samples are distinct known densities."""
        pool = self.pools[zoom][decile]
        offset = (which * len(pool)) // max(self.args.cells_per_decile, 1)
        rotated = pool[offset:] + pool[:offset]
        box, density = rotated[0]
        cell = {
            "zoom": zoom,
            "decile": decile,
            "which": which,
            "box": box,
            "request_zoom": self._depth(box, zoom),
            "density_visible_100pc": density,
            "distinct_locations": min(self.cold_samples, len(rotated)),
            "conditions": {},
        }
        # Cold and hot use different sample counts, recorded per run.
        for condition in self.conditions:
            if condition == "hot":
                samples = self._hot_samples(token, zoom, box, self.args.samples)
            elif condition == "cold":
                samples = self._cold_samples(terms, zoom, rotated, self.cold_samples, True)
            elif condition == "cold_pages_warm_engine":
                samples = self._cold_samples(terms, zoom, rotated, self.cold_samples, False)
            else:
                raise SystemExit(f"unknown condition {condition!r}")
            cell["conditions"][condition] = condition_figures(samples, cold=condition != "hot")
        # `all` rather than the headline block, since the first condition may have proved nothing.
        served_figures = cell["conditions"][self.conditions[0]]["all"]["served"]
        shed = sum(figures.get("shed", 0) for figures in cell["conditions"].values())
        self.log(
            f"    zoom {zoom} decile {decile}.{which} "
            f"depth={cell['request_zoom']} density={density:,} "
            f"served_p50={served_figures['p50']} "
            + " ".join(
                f"{c}:p50={figures['wall_ms']['p50']}"
                for c, figures in cell["conditions"].items()
            )
            + (f" shed={shed}" if shed else "")
        )
        return cell

    def run_rung(self, rung: dict, token: str, authorise_s: float, total_rows: int) -> dict:
        """One principal's first viewport and every cell under it, returned even when the
        server died mid-rung: `died` says where it stopped."""
        # The first viewport carries the fragment build and must not be averaged in.
        t0 = time.perf_counter()
        first = self._sample(token, 0, full_box(self.quant), cold=False)
        first_s = time.perf_counter() - t0
        first_counts = first.get("counts") or {}
        measured = int(first_counts.get("visible") or 0)
        self.log(
            f"    authorise {authorise_s*1000:.0f} ms, first viewport {first_s:.2f} s at "
            f"depth {first.get('request_zoom')} with k={first.get('k')}: "
            f"{int(first_counts.get('served') or 0):,} points over "
            f"{int(first_counts.get('n_tiles') or 0):,} occupied tiles in "
            f"{(first.get('bytes') or 0) / 1e6:.1f} MB, first flush "
            f"{(first.get('server_ms') or 0):.0f} ms, stream "
            f"{(first.get('stream_ms') or 0):.0f} ms, measured coverage "
            f"{measured/max(total_rows,1):.4%}"
            + (" — SHED" if first.get("shed") else "")
        )

        cells = [
            self.measure_cell(token, rung["terms"], zoom, decile, which)
            for zoom in self.zooms
            for decile in self.deciles
            if self.pools[zoom][decile]
            for which in range(self.args.cells_per_decile)
        ]
        return {
            "target": rung["target"],
            "terms": rung["terms"],
            "terms_n": len(rung["terms"]),
            "rule": rung.get("rule"),
            "measured": round(measured / max(total_rows, 1), 6),
            "first_viewport_failed": first.get("failed"),
            "measured_visible": measured,
            "authorise_s": round(authorise_s, 4),
            "first_viewport_s": round(first_s, 4),
            "first_viewport_request_zoom": first.get("request_zoom"),
            "first_viewport_k": first.get("k"),
            "first_viewport_served": first_counts.get("served"),
            "first_viewport_occupied_tiles": first_counts.get("n_tiles"),
            "first_viewport_bytes": first.get("bytes"),
            "first_viewport_server_ms": first.get("server_ms"),
            "first_viewport_stream_ms": first.get("stream_ms"),
            "first_viewport_shed": bool(first.get("shed")),
            "cells": cells,
            "battery": battery_figures(cells),
        }

    def run(self) -> dict:
        args = self.args
        ranks = json.loads(Path(args.ranks).read_text())

        # The 100% principal first: the denominator of every coverage figure.
        all_terms = sorted(r["term"] for r in ranks)
        broad_token, broad_authorise_s = authorise(args.session, args.session_cred, all_terms)
        selection = self.request_shape(broad_token)
        whole = self.whole_extent(broad_token)
        total_rows = int((whole["counts"] or {}).get("visible") or 0)
        self.rank_deciles(broad_token)

        token = broad_token
        ladder_out = []
        for rung in compose_ladder(
            ranks, total_rows, [float(t) for t in args.targets.split(",")]
        ):
            # A dead server ends the run without losing it: the rung in flight is recorded in
            # `died`.
            if self.died is not None:
                break
            self.log(
                f"  principal target {rung['target']:.0%}: {len(rung['terms'])} term(s) "
                f"{rung['terms']}"
            )
            try:
                token, authorise_s = authorise(args.session, args.session_cred, rung["terms"])
            except requests.exceptions.RequestException as e:
                self._record_death("session/authorise", e, rung["terms"])
                break
            ladder_out.append(self.run_rung(rung, token, authorise_s, total_rows))

        text = (
            {"skipped": "the server died earlier in the run"}
            if self.died is not None
            else self._text_and_drilldown(token)
        )
        whole_counts = whole["counts"] or {}

        return {
            "cap": args.cap_bytes,
            "cgroup": str(args.cgroup) if args.cgroup else None,
            "view": self.view_id,
            "total_rows": total_rows,
            "request": {
                "budget_depth": args.budget_depth,
                "grid_depth": GRID_DEPTH,
                "k": self.k,
                "rank_k": RANK_K,
                "max_k": selection.get("max_k"),
                "k_max_marks": selection.get("k_max_marks"),
                "theta_target_marks": selection.get("theta_target_marks"),
                "max_tiles_per_request": self.max_tiles,
                "whole_extent_zoom": whole["request_zoom"],
                "whole_extent_served": int(whole_counts.get("served") or 0),
                "whole_extent_occupied_tiles": int(whole_counts.get("n_tiles") or 0),
                "whole_extent_bytes": whole["bytes"],
                "whole_extent_shed": bool(whole["shed"]),
            },
            "all_terms_authorise_s": round(broad_authorise_s, 4),
            "candidates_per_zoom": args.candidates,
            "samples_per_cell": args.samples,
            "cold_samples_per_cell": self.cold_samples,
            "conditions": self.conditions,
            "zooms": self.zooms,
            "deciles": self.deciles,
            "cells_per_decile": args.cells_per_decile,
            "evictions": self.evictor.evictions,
            "cgroup_at_end": self.evictor.cgroup_events(),
            "oom_kill_seen": self.oom_seen,
            "request_failures": self.failures,
            "died": self.died,
            "ladder": ladder_out,
            "text_and_drilldown": text,
        }

    def _text_and_drilldown(self, token) -> dict:
        """A `match` on the rung's text column, common and rare, and one drill-down, under the
        same three conditions as the battery, at one location.
        """
        full = full_box(self.quant)
        out: dict = {"column": self.args.text_column, "matches": {}}
        for label, word in (("common", self.args.common_token), ("rare", self.args.rare_token)):
            per_condition = {}
            for condition in ("hot", "cold"):
                samples = []
                for _ in range(self.args.text_samples):
                    if condition == "cold":
                        self.evictor.evict()
                    samples.append(
                        self._sample(
                            token,
                            0,
                            full,
                            cold=(condition == "cold"),
                            filters={self.args.text_column: {"match": word}},
                        )
                    )
                per_condition[condition] = condition_figures(
                    samples, cold=condition == "cold"
                )
            out["matches"][label] = {"token": word, "conditions": per_condition}

        # One drill-down; a 404 is a result, not a failure, since most low handles are not live.
        drill = []
        for handle in range(1, self.args.drilldowns + 1):
            t0 = time.perf_counter()
            r = requests.post(
                f"{self.args.viewer}/v1/items/{handle}",
                headers={"Authorization": f"Bearer {token}"},
                json={},
                timeout=30,
            )
            drill.append(
                {
                    "status": r.status_code,
                    "wall_ms": (time.perf_counter() - t0) * 1000.0,
                    "server_ms": int(r.headers.get("x-tessera-server-us", "0")) / 1000.0,
                }
            )
        out["drilldown"] = {
            "n": len(drill),
            "statuses": sorted({d["status"] for d in drill}),
            "wall_ms": percentiles([d["wall_ms"] for d in drill]),
            "server_ms": percentiles([d["server_ms"] for d in drill]),
        }
        return out


def battery_figures(cells: Sequence[dict]) -> dict:
    """Battery-level figures over cells, not over pooled samples: pooling every sample would
    weight the battery by how many samples each cell happened to contribute. Each cell instead
    contributes its own p50 and its own p99, and these figures are percentiles over those.
    """
    out: dict = {}
    conditions = sorted({c for cell in cells for c in cell["conditions"]})
    for condition in conditions:
        block = {}
        for side in SAMPLE_FIGURES:
            block[side] = {
                "of_cell_p50": percentiles(
                    [
                        cell["conditions"][condition][side]["p50"]
                        for cell in cells
                        if cell["conditions"][condition][side]["p50"] is not None
                    ]
                ),
                "of_cell_p99": percentiles(
                    [
                        cell["conditions"][condition][side]["p99"]
                        for cell in cells
                        if cell["conditions"][condition][side]["p99"] is not None
                    ]
                ),
            }
        block["cells"] = len(cells)
        block["eviction_failed"] = sum(
            cell["conditions"][condition].get("eviction_failed", 0) for cell in cells
        )
        block["shed"] = sum(cell["conditions"][condition].get("shed", 0) for cell in cells)
        out[condition] = block
    return out


def add_arguments(ap: argparse.ArgumentParser) -> None:
    ap.add_argument("--viewer", help="a running server's viewer base URL")
    ap.add_argument("--session")
    ap.add_argument("--session-cred")
    ap.add_argument("--bundle", help="the bundle directory, for eviction")
    ap.add_argument("--cache", default=None, help="the deployment's cache directory")
    ap.add_argument("--ranks", help="the rung's branch-ranks.json; defaults to <boot-rung>/branch-ranks.json")
    ap.add_argument("--server-pid", type=int, default=None)
    ap.add_argument("--cgroup", default=None, help="the server's cgroup v2 directory, when capped")
    ap.add_argument("--cap-bytes", type=int, default=None, help="MemoryMax, recorded in the result")
    ap.add_argument("--view", default=None)
    ap.add_argument("--targets", default="0.01,0.05,0.10,0.25,0.50,1.0")
    ap.add_argument("--zooms", default="0,3,6,9,12")
    ap.add_argument("--deciles", default="0,1,2,3,4,5,6,7,8,9")
    ap.add_argument("--cells-per-decile", type=int, default=2)
    ap.add_argument("--samples", type=int, default=40)
    ap.add_argument(
        "--cold-samples",
        type=int,
        default=None,
        help="samples per cold cell; defaults to --samples. A cold sample at 3.6e7 rows is tens "
        "of seconds, so the two budgets are not the same budget",
    )
    ap.add_argument("--candidates", type=int, default=500, help="candidate boxes per zoom")
    ap.add_argument("--conditions", default="cold,cold_pages_warm_engine,hot")
    ap.add_argument(
        "--k",
        type=int,
        default=None,
        help="k on every request; defaults to the deployment's own selection.max_k from /v1/meta, "
        f"else {DEFAULT_K}. The engine clamps it per tile to k_max_marks",
    )
    ap.add_argument(
        "--budget-depth",
        type=int,
        default=BUDGET_DEPTH,
        help="how much deeper than its own zoom a request is made; the whole extent at depth "
        f"{BUDGET_DEPTH} is 4**{BUDGET_DEPTH} tiles",
    )
    ap.add_argument("--text-column", default="title")
    ap.add_argument("--common-token", default="of")
    ap.add_argument("--rare-token", default="zzzxyq_rare_token_probe")
    ap.add_argument("--text-samples", type=int, default=10)
    ap.add_argument("--drilldowns", type=int, default=20)
    ap.add_argument("--seed", type=int, default=0)
    # Booted here rather than handed one, since a battery needs the server's pid and cgroup.
    ap.add_argument("--boot-rung", help="a rung directory to serve, instead of an already-running server")
    ap.add_argument("--boot-bundle", help="the bundle to serve; defaults to <rung>/bundle")
    ap.add_argument("--boot-scratch", help="where the scratch deployment, cache and WAL go")
    ap.add_argument("--boot-binary", help="the tessera binary")
    ap.add_argument("--boot-port0", type=int, default=8151)
    ap.add_argument(
        "--boot-serve-config",
        default=None,
        help='extra [serve] keys for the booted deployment, as JSON: '
        '\'{"stream_deadline_ms": 1}\' cuts every streamed response, which is how the shed path '
        "is exercised on a corpus whose responses would otherwise finish inside the budget",
    )


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    add_arguments(ap)
    ap.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    started = time.time()
    served = None
    if args.boot_rung:
        from .deployment import Deployment

        rung = Path(args.boot_rung)
        bundle = Path(args.boot_bundle) if args.boot_bundle else rung / "bundle"
        served = Deployment(
            rung,
            bundle,
            Path(args.boot_scratch),
            (args.boot_port0, args.boot_port0 + 1, args.boot_port0 + 2),
            Path(args.boot_binary),
            cap_bytes=args.cap_bytes,
            serve=json.loads(args.boot_serve_config) if args.boot_serve_config else None,
        )
        served.clear_scratch()
        open_at = time.time()
        served.start()
        open_s = round(time.time() - open_at, 2)
        args.viewer, args.session = served.viewer, served.session
        args.session_cred = served.credential("session")
        args.bundle, args.cache = str(bundle), str(served.cache)
        args.server_pid = served.pid
        args.cgroup = str(served.cgroup) if served.cgroup else None
        args.ranks = args.ranks or str(rung / "branch-ranks.json")
        print(f"served pid={served.pid} cgroup={served.cgroup}", flush=True)
    try:
        result = Battery(args).run()
    finally:
        if served is not None:
            served.stop()
    if served is not None:
        # The open: tessera serve from launch to /readyz, measured around start() alone.
        result["open_s"] = open_s
    result["ran_s"] = round(time.time() - started, 1)
    Path(args.out).write_text(json.dumps(result, indent=2))
    print(f"wrote {args.out} in {result['ran_s']} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
