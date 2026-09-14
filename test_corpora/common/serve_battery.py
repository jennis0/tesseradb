"""The serve battery — view latency across a principal ladder, hot and cold, on one schema.

Every rung of the ladder records the same three things (owner rulings, 2026-09-03): what the
build cost per stage, what a view costs a principal, and what online ingest sustains. This module
is the middle one. It drives a **running** `tessera serve` — it does not boot one — and writes the
`serve` block of the rung's `measurements.json` (`README.md` carries the schema).

What makes it a battery rather than a pan sequence
--------------------------------------------------

`probes/2026-09-02-serve-under-memory-cap/drive.py` panned five fixed fractions of the extent per
zoom. That answers "does it survive"; it cannot answer "what does a viewport cost", because the
five boxes' densities are whatever the corpus put there and a p99 over them is a p99 over an
unknown mixture. `crates/tessera-bench/src/arms/viewport.rs`' module doc makes the same argument
for the bench arms: the cost of a viewport is a function of how many points are in it, so the
locations are drawn by **density decile** and every cell carries the density it was drawn at.

Three conditions, split mechanically
------------------------------------

A single "latency" figure over a mixture of cold and warm requests is the number that hides both.
So each cell is measured three ways, and the condition is stamped in every row:

* ``cold`` — the bundle's and cache's pages evicted **and** a fresh session (a new
  ``session/authorise``) before every sample, so neither the page cache nor the session's own
  fragments carry anything over. This is a first request by a new viewer.
* ``cold_pages_warm_engine`` — the session's fragments built once, then pages evicted before every
  sample. This isolates the storage cost from the per-session build.
* ``hot`` — one location repeated, nothing evicted.

The two cold conditions draw **40 distinct locations from the cell's own decile**: repeating one
location warms it, and a "cold" sample of an already-faulted extent measures nothing. ``hot``
repeats one location, which is what makes it hot.

The request shape is the client's budget
----------------------------------------

A client draws one to two million marks a view and the request-time budget spreads them over the
tiles the viewport covers: `architecture.md` §7.2's arithmetic puts the tile count a request
carries at `B / m_target`, independent of zoom, and `caching.md` §3 prices a view at 60 k to 125 k
tiles. So a request is made at the **budget's depth** rather than at the sample's own zoom, and
`k` is the deployment's own ``max_k``, which the engine clamps per tile to ``k_max_marks``. Beside
the latency every sample records what its request served and moved: the depth, `k`, the points the
sweep selected, the bytes, the time to the first flush and the time to the trailer. At two million
points the body is tens of megabytes, and the split between the sweep and the emit is what a
viewer waits through.

The ranking pass asks at the same depth its samples will, so a cell's recorded density is the
visible count over the tiles the samples request rather than over a coarser cover of the same box.
It costs ``--candidates`` requests a zoom at that depth before the ladder starts, which is the
knob to lower on a large corpus.

Proof of cold, and why a failed eviction is not a result
--------------------------------------------------------

Eviction without root is ``posix_fadvise(…, POSIX_FADV_DONTNEED)`` over every file under the bundle
and the cache, plus ``memory.reclaim`` on the server's cgroup when one is given. Neither is
guaranteed: a page another process holds mapped is not dropped, and a cgroup may reclaim less than
asked. So every cold sample reads the server's ``majflt`` from ``/proc/<pid>/stat`` either side of
its request, and **a cold sample whose major-fault delta is zero is excluded from the cell's
headline percentiles** rather than reported as a fast cold read. Every cell carries `proven_cold`,
`eviction_failed` and an `all` block over every sample regardless, so a cell that proved nothing is
visible as such and its numbers are still there to read — see `condition_figures` for why a zero
delta means different things under the two cold conditions.

⊘ ``majflt`` is process-wide, so a fault another thread caused inside the sampling window counts
here too. It is a **lower bound on cold-ness being real**, not an attribution: what it rules out
is the case this check exists for, an "eviction" that dropped nothing at all.

Usage
-----

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

#: The percentiles every cell reports. p25 and p75 are here because the interesting shape at a
#: cold cell is the *spread*, and a p50/p99 pair cannot show it.
PERCENTILES = (25, 50, 75, 95, 99)

#: How much deeper than its own zoom a request is made. The whole extent at depth 9 covers
#: 4⁹ = 262,144 tiles, which is `serve.max_tiles_per_request`'s default and the depth at which
#: sixteen marks per occupied tile is nearest two million on the whole GBIF corpus; a box that is
#: one depth-`z` tile asked at `z + 9` covers the same 262,144.
BUDGET_DEPTH = 9

#: The Morton grid has sixteen levels (`tessera-spatial`'s `Tile::code_range`), so no request can
#: be made below depth 16.
GRID_DEPTH = 16

#: `k` where `/v1/meta` publishes no `selection.max_k`. Every rung's deployment declares 5000.
DEFAULT_K = 5000

#: `max_tiles_per_request` where `/v1/meta` publishes none: the server's own default.
DEFAULT_MAX_TILES = 262_144

#: `k` on a ranking request. `k = 0` is the counts-only request (contracts §3.2 r38): the tiles
#: frame is served exact as at any `k` and no points frame is emitted at all, so the engine's
#: `cap == 0` arm returns the counts without a selection or a gather. A decile is a ranking over
#: `visible`, which no `k` changes, and at the budget's depth a ranked candidate asked for points
#: would move tens of megabytes per candidate before the ladder had started.
RANK_K = 0

#: The frame kinds this module reads (contracts §3.2 r26; `clients/ts/core/src/frame.ts` carries
#: the whole grammar).
FRAME_TILES = 1
FRAME_TRAILER = 4

#: What a connection aborted part-way through a body raises. Caught around the read alone, never
#: around the request, so a server that is not there is still a failure rather than a shed stream.
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
    """The response's frames as `(kind, payload)` in wire order.

    `u8 kind, u32 little-endian length, payload`, repeated (contracts §3.2 r26). A body that ends
    part-way through a frame yields the frames that were whole: what a cut stream delivered is a
    measurement here, and the absent kind-4 trailer is how the cut is seen
    (`streamed-serving.md` §6).
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
    """What the response counted, what it served and whether it arrived whole.

    The tiles frame's `visible` and `matched` are the masked counts and its `served` is what the
    sweep selected; the trailer's `points` is what the emit delivered. **These sums, not a byte
    digest, are the correctness check**: `k` point sampling is not claimed deterministic across
    processes and is not observed to be, while the masked counts are exact. The two point figures
    differ exactly where the stream was cut, which the missing trailer says a second way.
    """
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
) -> dict:
    """One `/v1/viewport` at the depth and `k` the caller has already chosen.

    Neither has a default here. The depth is [`budget_zoom`]'s and `k` is the deployment's
    ceiling, and a default on either would be a second answer to a question this module answers
    once.

    **A stream the server cut is a result, not an exception.** The whole emit phase runs under
    `serve.stream_deadline_ms` from the first flush (`streamed-serving.md` §5), and a
    budget-sized response on a large corpus can outrun it: the connection aborts mid-body and no
    trailer is emitted. The body is therefore read chunk by chunk and what arrived is returned
    with `shed` set — the counts frame is first on the wire, so a shed sample still carries exact
    counts, and its delivered points are a sound prefix (§6). A response with no trailer is shed
    whether or not the read raised, because the trailer's presence is the completeness signal.
    """
    body: dict = {"view": view_id, "zoom": zoom, "bbox": list(bbox), "k": k}
    if layers is not None:
        body["layers"] = layers
    if filters is not None:
        body["filters"] = filters
    t0 = time.perf_counter()
    r = requests.post(
        f"{viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=timeout,
        stream=True,
    )
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
    wall = time.perf_counter() - t0
    out = {
        # To the cut where there was one, which is what a client waited before it knew.
        "wall_ms": wall * 1000.0,
        # Post-admission to the sweep's end, which under streaming is the time to the first flush
        # and the earliest byte a client can draw (`streamed-serving.md` §5). The whole stream is
        # the trailer's `stream_us`, and the two are named apart so neither stands in for the
        # other.
        "server_ms": int(r.headers.get("x-tessera-server-us", "0")) / 1000.0,
        "bytes": len(content),
        "request_zoom": zoom,
        "k": k,
        "shed_error": shed_error,
    }
    out.update(response_figures(bytes(content)))
    out["shed"] = shed_error is not None or not out["complete"]
    return out


def _cell_index(v: float, lo: float, hi: float) -> int:
    """`tessera-spatial`'s 16-bit quantiser: the cell a coordinate falls in, clamped at both ends."""
    scaled = math.floor((v - lo) / (hi - lo) * 65536.0)
    if scaled <= 0:
        return 0
    return 65535 if scaled >= 65535 else int(scaled)


def covered_tiles(quantisation: dict, bbox: Sequence[float], depth: int) -> int:
    """How many depth-`depth` tiles `bbox` covers, as `tessera-spatial` counts them.

    The engine counts this same product and refuses a request above `max_tiles_per_request`
    before it does any masking work, so the battery counts it too and asks a depth shallower
    rather than sending a request it knows will be refused.
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
    """The depth a client spending its point budget on `bbox` asks at.

    `zoom + budget_depth`, held to the grid's sixteen levels and then reduced while the box covers
    more tiles than the deployment allows one request. The reduction is not a corner case. A box
    drawn here is one depth-`zoom` tile's worth of area placed at random rather than a tile, so it
    straddles an extra row and column of tiles, and (2⁹ + 1)² is 1,025 above the default ceiling.
    The drawn span also stops shrinking at zoom 6, so above that zoom the sum on its own asks for
    four or sixteen times the budget.
    """
    depth = min(GRID_DEPTH, zoom + budget_depth)
    while depth > 0 and covered_tiles(quantisation, bbox, depth) > max_tiles:
        depth -= 1
    return depth


# ---------------------------------------------------------------------------------------------
# Eviction, and the proof it worked
# ---------------------------------------------------------------------------------------------


class Evictor:
    """Drops the bundle's and cache's pages, and reads the server's major-fault counter.

    **Without root**, which is the whole constraint: `/proc/sys/vm/drop_caches` needs it and
    `probes/2026-09-02-serve-under-memory-cap` had no genuinely cold run for exactly that reason.
    `posix_fadvise(POSIX_FADV_DONTNEED)` needs only read access to the file.
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
        self.files_evicted = 0
        self.evictions = 0
        self.reclaim_errors = 0

    def _files(self) -> Iterable[Path]:
        for root in self.roots:
            if root.is_file():
                yield root
            elif root.is_dir():
                for path in root.rglob("*"):
                    if path.is_file():
                        yield path

    def evict(self) -> int:
        """Drop every page we can, and return how many files were advised."""
        n = 0
        for path in self._files():
            try:
                fd = os.open(path, os.O_RDONLY)
            except OSError:
                continue
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
                n += 1
            except OSError:
                pass
            finally:
                os.close(fd)
        if self.cgroup is not None:
            # `memory.reclaim` takes a byte count; asking for more than is charged is not an
            # error, it reclaims what it can. Absent on a kernel without it, which is not a
            # failure of the run — the fadvise pass above is the primary mechanism.
            # **Sized to what the cgroup is actually holding**, read fresh each time, rather than
            # a fixed large number: `memory.reclaim` iterates until it has reclaimed what it was
            # asked for or run out of candidates, so an ask far above the charge spends real time
            # scanning for pages that are not there — at 4,000 evictions in a battery that is the
            # run's duration. This is the only mechanism here that reaches pages the server holds
            # mapped, and it runs before **every** cold sample.
            try:
                charged = int((self.cgroup / "memory.current").read_text().strip())
            except OSError:
                charged = self.reclaim_bytes
            try:
                with open(self.cgroup / "memory.reclaim", "w") as f:
                    f.write(f"{max(charged, 1 << 20)}\n")
            except OSError:
                self.reclaim_errors += 1
        self.files_evicted += n
        self.evictions += 1
        return n

    def majflt(self) -> int | None:
        """The server's process-wide major-fault count (`/proc/<pid>/stat` field 12)."""
        if self.server_pid is None:
            return None
        try:
            raw = Path(f"/proc/{self.server_pid}/stat").read_text()
        except OSError:
            return None
        # The comm field may contain spaces and parentheses; everything after the last ')' is
        # positionally safe. `majflt` is field 12 overall, so index 9 of the tail.
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
    """Greedy term sets for each target fraction of the corpus.

    `ranks` is the rung's `branch-ranks.json`: `[{"term": ..., "pairs": ...}, …]`, one entry per
    value of the point-visibility vocabulary, `pairs` being how many (item, term) pairs carry it.

    **Pairs are an upper bound on entities, never the entity count**: an article carries a mean of
    10.6 MeSH descriptors and so several branch letters, and the pair sums overlap. That is why
    the composition is a *target* and the number that gets reported beside it is **measured** —
    a zoom-0 whole-extent viewport under the composed principal, over the same under the 100%
    principal.

    The greedy is: fill descending by size while the running sum stays under the target's pair
    budget; then consider the one smallest unused term that would overshoot, and take whichever of
    the two sums is closer to the budget **in ratio** rather than in absolute difference. Ratio,
    because a ladder rung's job is to sit at an order of magnitude: for a 1% target on this rung
    the two candidates are 0.014% and 2.9%, and absolute difference picks 0.014%, which is not a
    1% rung by any reading.
    """
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
    """`n` boxes at one zoom, uniformly placed over the extent.

    The span shrinks with zoom and stops shrinking at zoom 6, matching the pan sequence the
    memory-cap probe used, so the request sizes here and there are comparable. What differs is
    that these are *drawn* rather than fixed, which is what gives the deciles something to rank.
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
    """`(box, visible)` sorted ascending by `visible`, under the **100% principal**.

    Under the widest principal on purpose: a decile is a property of the corpus's geometry, so
    that every principal's cells at decile *d* are the same places, and "the narrow principal is
    faster at decile 9" is a statement about masking rather than about two different maps.

    At the depth the cell's samples will ask at, because `visible` is summed over the tiles a
    request covers and a coarser cover of the same box reaches past it: a decile ranked at the
    zoom's own depth would rank boxes on ground the samples do not read.

    **At `RANK_K` and with the artifact channel off**, which is the counts frame and nothing else.
    A ranking pass is ``--candidates`` requests a zoom before any cell is measured; asked for
    points at the budget's depth each one is a whole view's body, and on a corpus large enough for
    that body to outrun `serve.stream_deadline_ms` the pass cannot complete at all. No count here
    depends on either.
    """
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
    """p25/p50/p75/p95/p99 and max, by nearest rank on the sorted samples.

    Nearest rank rather than an interpolating estimator: these are latencies with a long tail and
    a small n, and an interpolated p99 over 40 samples reports a number no request took.
    """
    if not values:
        return {f"p{p}": None for p in PERCENTILES} | {"max": None, "n": 0}
    s = sorted(values)
    out = {}
    for p in PERCENTILES:
        idx = min(int(math.ceil(p / 100.0 * len(s))) - 1, len(s) - 1)
        out[f"p{p}"] = round(s[max(idx, 0)], 4)
    out["max"] = round(s[-1], 4)
    out["mean"] = round(statistics.fmean(s), 4)
    out["n"] = len(s)
    return out


#: The per-sample figures a cell reports percentiles over. `server_ms` is the header's time to the
#: first flush and `stream_ms` the trailer's time to the last, so at a budget-sized response the
#: distance between them is the emit; `served` and `response_bytes` are what that emit moved.
SAMPLE_FIGURES = ("server_ms", "stream_ms", "wall_ms", "served", "response_bytes")


def cell_figures(samples: Sequence[dict]) -> dict:
    """Server-side, whole-stream and end-to-end percentiles, **separately**.

    The three answer different questions. A cold first request on this corpus is 22.7 s end to end
    against 0.12 ms server-side (`probes/2026-09-02-serve-under-memory-cap`), so one pooled figure
    is two measurements averaged into neither; and under streaming the server-side figure stops at
    the sweep, so what a viewer waits through is the stream figure beside it.

    **Over the complete samples.** A shed stream's wall time is the time to the cut, which is a
    property of `serve.stream_deadline_ms` rather than of the request, so averaging it in would
    report the deadline as a latency. The shed are counted instead.
    """
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
    """One condition's figures, with the proven-cold samples separated from the rest.

    **A zero major-fault delta is not always a failed eviction, and the two cannot be told apart
    from `majflt` alone.** Under `cold_pages_warm_engine` on this corpus most samples show one:
    the eviction drops the bundle's pages, and then the request reads nothing off disk at all,
    because a session's fragments and its row projection are heap structures and a masked count
    over them is bitmap arithmetic (`filter-index.md` §8). So a zero delta there is a *result* —
    the request needed no file page — and a zero delta under `cold`, where the session itself is
    new, is much more likely to be an eviction that dropped nothing.

    Both readings are served by carrying both sets: the top-level figures are over the
    **proven-cold** samples for a cold condition (and over everything for `hot`), and `all` is
    over every sample regardless. A cell whose `proven_cold` is 0 has `None` percentiles at the
    top level and real ones in `all`, which is the honest shape for "nothing here was proven
    cold" — never a fast cold read.
    """
    samples = [s for s in samples]
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
    # A response whose trailer never arrived is a stream the server cut mid-body
    # (`streamed-serving.md` §6). It is counted rather than dropped: on a corpus whose budget
    # response outruns `serve.stream_deadline_ms` that is the result, and the delivered prefix is
    # still a sound partial band. A cell every sample of which was shed has no percentiles and
    # this count, which is the honest shape for it.
    out["shed"] = sum(1 for s in samples if s.get("shed") and not s.get("failed"))
    # The counts frame is first on the wire, so a shed sample carries them exactly and is as good
    # a source for them as a complete one.
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
    """Line-buffered by hand: a long run is watched through its redirected log, and Python
    block-buffers stdout when it is not a terminal — a battery that had run for forty minutes
    looked, from the file, like one that had not started."""
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
        # The request shape, settled in `run` from `/v1/meta` before anything is sampled: the
        # view's own frame, the deployment's `k` ceiling and its tile guard. A request is built
        # from these three and from nothing else.
        self.quant: dict = {}
        self.selection: dict = {}
        self.max_tiles = DEFAULT_MAX_TILES
        self.k = args.k if args.k is not None else DEFAULT_K

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

    def _sample(self, token, view_id, zoom, box, cold: bool, **kw) -> dict:
        """One request, with the cold proof around it when the condition asks for one.

        **A request that fails is a sample with a `failed` field, not the end of the run.** A
        battery that died on one would throw away every cell it had already measured. The failure
        is recorded with its wall time and excluded from the percentiles. A stream the server cut
        mid-body does not reach here at all: [`viewport`] returns it marked `shed`, which is a
        sample with exact counts and no latency figure.
        """
        depth = budget_zoom(self.quant, box, zoom, self.args.budget_depth, self.max_tiles)
        before = self.evictor.majflt() if cold else None
        t0 = time.perf_counter()
        try:
            s = viewport(self.args.viewer, token, view_id, depth, box, k=self.k, **kw)
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

    def _cold_samples(self, terms, view_id, zoom, pool, n, fresh_session: bool) -> list[dict]:
        """`n` samples, a distinct location each, evicted before every one.

        A repeated location is warm, so the pool is walked rather than one box repeated; where the
        pool holds fewer than `n` it is walked round, and the run says so in `distinct_locations`.
        """
        token = None
        if not fresh_session:
            token = self._fresh_token(terms)
            if token is None:
                return []
            # Build this session's fragments once — that is what "warm engine" means — on a
            # location that is not one of the samples, so no sample's own extent is faulted here.
            self._sample(token, view_id, zoom, pool[0][0], cold=False)
        if token is None and not fresh_session:
            return []
        out = []
        for i in range(n):
            box = pool[i % len(pool)][0]
            self.evictor.evict()
            if fresh_session:
                token = self._fresh_token(terms)
            if token is None:
                break
            s = self._sample(token, view_id, zoom, box, cold=True)
            s["location"] = i % len(pool)
            out.append(s)
            self._watch_oom()
        return out

    def _hot_samples(self, token, view_id, zoom, box, n) -> list[dict]:
        out = []
        for _ in range(n):
            out.append(self._sample(token, view_id, zoom, box, cold=False))
        return out

    def run(self) -> dict:
        args = self.args
        ranks = json.loads(Path(args.ranks).read_text())
        targets = [float(t) for t in args.targets.split(",")]
        zooms = [int(z) for z in args.zooms.split(",")]
        deciles = [int(d) for d in args.deciles.split(",")]

        # The 100% principal first: it is the denominator of every coverage figure and the
        # principal the deciles are ranked under.
        all_terms = sorted(r["term"] for r in ranks)
        broad_token, broad_authorise_s = authorise(args.session, args.session_cred, all_terms)
        m = meta(args.viewer, broad_token)
        view_id = args.view or m["views"][0]["id"]
        quant = next(v for v in m["views"] if v["id"] == view_id)["quantisation"]
        # The two ceilings a request is built against, from the deployment that will serve it
        # rather than from a file beside it: `/v1/meta` publishes both for exactly this reason
        # (contracts §3.2), and a battery driving a server it did not boot has no other source.
        selection = m.get("selection") or {}
        self.quant = quant
        self.selection = selection
        self.max_tiles = int(selection.get("max_tiles_per_request") or DEFAULT_MAX_TILES)
        if args.k is None:
            self.k = int(selection.get("max_k") or DEFAULT_K)
        full_extent = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
        whole_zoom = budget_zoom(quant, full_extent, 0, args.budget_depth, self.max_tiles)
        whole = viewport(
            args.viewer, broad_token, view_id, whole_zoom, full_extent, k=self.k, layers=None
        )
        whole_counts = whole["counts"] or {}
        total_rows = int(whole_counts.get("visible") or 0)
        whole_occupied = int(whole_counts.get("n_tiles") or 0)
        whole_served = int(whole_counts.get("served") or 0)
        self.log(
            f"  100% principal sees {total_rows:,} over the whole extent; the budget request at "
            f"depth {whole_zoom} with k={self.k} serves {whole_served:,} points over "
            f"{whole_occupied:,} occupied tiles "
            f"({whole_served / max(whole_occupied, 1):.1f} a tile) in "
            f"{(whole['bytes'] or 0) / 1e6:.1f} MB, first flush {whole['server_ms']:.0f} ms, "
            f"stream {(whole['stream_ms'] or 0):.0f} ms"
            + (f" — SHED after {whole['wall_ms']:.0f} ms" if whole["shed"] else "")
        )

        # Density deciles, ranked once under the 100% principal and reused by every rung.
        pools: dict[int, list[list[tuple[list[float], int]]]] = {}
        for zoom in zooms:
            boxes = candidate_boxes(quant, zoom, args.candidates, self.rng)
            ranked = rank_by_density(
                args.viewer,
                broad_token,
                view_id,
                zoom,
                boxes,
                quant,
                args.budget_depth,
                self.max_tiles,
                self.log,
            )
            pools[zoom] = decile_pools(ranked)
            self.log(
                f"  zoom {zoom}: visible over {args.candidates} candidates "
                f"min={ranked[0][1]:,} median={ranked[len(ranked)//2][1]:,} max={ranked[-1][1]:,}"
            )

        ladder_spec = compose_ladder(ranks, total_rows, targets)
        ladder_out = []
        for rung in ladder_spec:
            terms = rung["terms"]
            # **A dead server ends the run and does not lose it.** Under a cap the process can be
            # OOM-killed part-way up the ladder — which is the result the capped run exists to
            # find — and every principal measured before that is still a measurement. The rung
            # that was in flight is recorded in `died` with the principal it was serving.
            if self.died is not None:
                break
            self.log(f"  principal target {rung['target']:.0%}: {len(terms)} term(s) {terms}")
            try:
                token, authorise_s = authorise(args.session, args.session_cred, terms)
            except requests.exceptions.RequestException as e:
                self._record_death("session/authorise", e, terms)
                break
            # The first viewport of a fresh session is its own figure: it carries the
            # `(view, principal)` fragment build, which is not a per-request cost and must not be
            # averaged into one.
            t0 = time.perf_counter()
            first = self._sample(token, view_id, 0, full_extent, cold=False)
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

            cells = []
            for zoom in zooms:
                for decile in deciles:
                    pool = pools[zoom][decile]
                    if not pool:
                        continue
                    for which in range(args.cells_per_decile):
                        # Two cells in one decile are two different orderings of the same pool,
                        # so their samples are distinct locations of the same known density.
                        offset = (which * len(pool)) // max(args.cells_per_decile, 1)
                        rotated = pool[offset:] + pool[:offset]
                        box, density = rotated[0]
                        cell = {
                            "zoom": zoom,
                            "decile": decile,
                            "which": which,
                            "box": box,
                            "request_zoom": budget_zoom(
                                quant, box, zoom, args.budget_depth, self.max_tiles
                            ),
                            "density_visible_100pc": density,
                            "distinct_locations": min(
                                args.cold_samples or args.samples, len(rotated)
                            ),
                            "conditions": {},
                        }
                        # **Cold and hot get different sample counts, and that is deliberate.** A
                        # hot sample is a millisecond; a cold one at 3.6×10⁷ rows under the widest
                        # principal is a fresh fragment build plus a re-fault of the bundle, tens
                        # of seconds — so forty of each is not one budget but two, and the cold
                        # half decides whether a rung is measurable in an evening at all. The
                        # count that was used is recorded per run; a reduced run must never look
                        # like a full one.
                        cold_n = args.cold_samples or args.samples
                        for condition in args.conditions.split(","):
                            if condition == "hot":
                                samples = self._hot_samples(
                                    token, view_id, zoom, box, args.samples
                                )
                            elif condition == "cold":
                                samples = self._cold_samples(
                                    terms, view_id, zoom, rotated, cold_n, True
                                )
                            elif condition == "cold_pages_warm_engine":
                                samples = self._cold_samples(
                                    terms, view_id, zoom, rotated, cold_n, False
                                )
                            else:
                                raise SystemExit(f"unknown condition {condition!r}")
                            cell["conditions"][condition] = condition_figures(
                                samples, cold=condition != "hot"
                            )
                        cells.append(cell)
                        # `all` rather than the headline block: the first condition may be a
                        # cold one that proved nothing, and the points a request served are the
                        # same whether the eviction worked.
                        served_figures = cell["conditions"][args.conditions.split(",")[0]][
                            "all"
                        ]["served"]
                        shed = sum(
                            cell["conditions"][c].get("shed", 0) for c in cell["conditions"]
                        )
                        self.log(
                            f"    zoom {zoom} decile {decile}.{which} "
                            f"depth={cell['request_zoom']} density={density:,} "
                            f"served_p50={served_figures['p50']} "
                            + " ".join(
                                f"{c}:p50={cell['conditions'][c]['wall_ms']['p50']}"
                                for c in cell["conditions"]
                            )
                            + (f" shed={shed}" if shed else "")
                        )

            # Appended even when the server died mid-rung: the cells measured before it are
            # measurements, and `died` says where it stopped.
            rung_out = {
                "target": rung["target"],
                "terms": terms,
                "terms_n": len(terms),
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
            }
            rung_out["battery"] = battery_figures(cells)
            ladder_out.append(rung_out)

        text = (
            {"skipped": "the server died earlier in the run"}
            if self.died is not None
            else self._text_and_drilldown(token, view_id, view_id, quant, pools, zooms)
        )

        return {
            "cap": args.cap_bytes,
            "cgroup": str(args.cgroup) if args.cgroup else None,
            "view": view_id,
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
                "whole_extent_zoom": whole_zoom,
                "whole_extent_served": whole_served,
                "whole_extent_occupied_tiles": whole_occupied,
                "whole_extent_bytes": whole["bytes"],
                "whole_extent_shed": bool(whole["shed"]),
            },
            "all_terms_authorise_s": round(broad_authorise_s, 4),
            "candidates_per_zoom": args.candidates,
            "samples_per_cell": args.samples,
            "cold_samples_per_cell": args.cold_samples or args.samples,
            "conditions": args.conditions.split(","),
            "zooms": zooms,
            "deciles": deciles,
            "cells_per_decile": args.cells_per_decile,
            "evictions": self.evictor.evictions,
            "cgroup_at_end": self.evictor.cgroup_events(),
            "oom_kill_seen": self.oom_seen,
            "request_failures": self.failures,
            "died": self.died,
            "ladder": ladder_out,
            "text_and_drilldown": text,
        }

    def _text_and_drilldown(self, token, view_id, _unused, quant, pools, zooms) -> dict:
        """A `match` on the rung's text column, common and rare, and one drill-down.

        Under the same three conditions as the battery, at one location, so the text index's cold
        cost is on the same footing as the geometry's rather than being a warm figure beside cold
        ones.
        """
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
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
                            view_id,
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

        # One drill-down. Handles are `tessera_id`s and most low ones are not live, so a 404 is a
        # result rather than a failure; what is measured is the route, not the row.
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
    """Battery-level figures **over cells, not over pooled samples**.

    Pooling every sample would weight the battery by how many samples each cell happened to
    contribute and let one cell's tail become the battery's p99. What a battery-level p99 should
    mean is "the worst cell", so each cell contributes its own p50 and its own p99, and these are
    percentiles over those.
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
    # Booting one here rather than being handed one: a battery has to know the server's pid and
    # its cgroup, and both are properties of how it was started. Given `--boot-rung`, everything
    # above is filled in from the deployment this creates.
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
        # The **open**, which is `tessera serve` from launch to `/readyz` — on this rung it is the
        # artifact projections being built at open rather than on the first request. Measured
        # around `start()` alone; taking it around the whole run would report the battery.
        result["open_s"] = open_s
    result["ran_s"] = round(time.time() - started, 1)
    Path(args.out).write_text(json.dumps(result, indent=2))
    print(f"wrote {args.out} in {result['ran_s']} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
