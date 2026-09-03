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
from pyarrow import ipc

#: The percentiles every cell reports. p25 and p75 are here because the interesting shape at a
#: cold cell is the *spread*, and a p50/p99 pair cannot show it.
PERCENTILES = (25, 50, 75, 95, 99)


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


def tile_counts(content: bytes):
    """Sum `visible`/`matched` over the response's tiles frame.

    The frame is Arrow IPC *stream* format behind a one-byte kind and a u32 length
    (`clients/ts/core/frame.ts`' convention). **This sum, not a byte digest, is the correctness
    check**: `k` point sampling is not claimed deterministic across processes and is not observed
    to be, while the masked counts are exact.
    """
    if len(content) < 5 or content[0] != 1:
        return None
    length = int.from_bytes(content[1:5], "little")
    table = ipc.open_stream(io.BytesIO(content[5 : 5 + length])).read_all()
    names = table.column_names
    return {
        "visible": sum(int(v) for v in table.column("visible").to_pylist())
        if "visible" in names
        else None,
        "matched": sum(int(v) for v in table.column("matched").to_pylist())
        if "matched" in names
        else None,
        "n_tiles": table.num_rows,
    }


def viewport(
    viewer_base: str,
    token: str,
    view_id: str,
    zoom: int,
    bbox: Sequence[float],
    k: int = 30,
    filters: dict | None = None,
    layers: str | None = "all",
    timeout: float = 120.0,
) -> dict:
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
    )
    wall = time.perf_counter() - t0
    r.raise_for_status()
    return {
        "wall_ms": wall * 1000.0,
        "server_ms": int(r.headers.get("x-tessera-server-us", "0")) / 1000.0,
        "bytes": len(r.content),
        "counts": tile_counts(r.content),
    }


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
    log: Callable[[str], None],
) -> list[tuple[list[float], int]]:
    """`(box, visible)` sorted ascending by `visible`, under the **100% principal**.

    Under the widest principal on purpose: a decile is a property of the corpus's geometry, so
    that every principal's cells at decile *d* are the same places, and "the narrow principal is
    faster at decile 9" is a statement about masking rather than about two different maps.
    """
    ranked = []
    for i, box in enumerate(boxes):
        s = viewport(viewer_base, token, view_id, zoom, box, k=1, layers=None)
        ranked.append((list(box), int(s["counts"]["visible"] or 0)))
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


def cell_figures(samples: Sequence[dict]) -> dict:
    """Server-side and end-to-end percentiles, **separately** — the two answer different questions.

    A cold first request on this corpus is 22.7 s end to end against 0.12 ms server-side
    (`probes/2026-09-02-serve-under-memory-cap`), so one pooled figure is two measurements
    averaged into neither.
    """
    return {
        "server_ms": percentiles([s["server_ms"] for s in samples]),
        "wall_ms": percentiles([s["wall_ms"] for s in samples]),
    }


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
    proven = [s for s in samples if s.get("majflt_delta") not in (None, 0)] if cold else list(samples)
    out = cell_figures(proven)
    out["all"] = cell_figures(samples)
    out["n_samples"] = len(samples)
    out["proven_cold"] = len(proven) if cold else None
    out["eviction_failed"] = (
        sum(1 for s in samples if s.get("eviction_failed")) if cold else 0
    )
    out["visible"] = samples[0]["counts"]["visible"] if samples else None
    out["matched"] = samples[0]["counts"]["matched"] if samples else None
    return out


# ---------------------------------------------------------------------------------------------
# The battery
# ---------------------------------------------------------------------------------------------


class Battery:
    def __init__(self, args, log: Callable[[str], None] = print):
        self.args = args
        self.log = log
        self.evictor = Evictor(
            [Path(args.bundle)] + ([Path(args.cache)] if args.cache else []),
            args.server_pid,
            Path(args.cgroup) if args.cgroup else None,
        )
        self.rng = random.Random(args.seed)
        self.oom_seen = False

    def _fresh_token(self, terms: Sequence[str]) -> str:
        token, _ = authorise(self.args.session, self.args.session_cred, terms)
        return token

    def _watch_oom(self) -> None:
        events = self.evictor.cgroup_events()
        if events and events.get("memory.events", {}).get("oom_kill", 0):
            self.oom_seen = True

    def _sample(self, token, view_id, zoom, box, cold: bool, **kw) -> dict:
        """One request, with the cold proof around it when the condition asks for one."""
        before = self.evictor.majflt() if cold else None
        s = viewport(self.args.viewer, token, view_id, zoom, box, k=self.args.k, **kw)
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
            # Build this session's fragments once — that is what "warm engine" means — on a
            # location that is not one of the samples, so no sample's own extent is faulted here.
            self._sample(token, view_id, zoom, pool[0][0], cold=False)
        out = []
        for i in range(n):
            box = pool[i % len(pool)][0]
            self.evictor.evict()
            if fresh_session:
                token = self._fresh_token(terms)
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
        full_extent = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
        whole = viewport(args.viewer, broad_token, view_id, 0, full_extent, k=1, layers=None)
        total_rows = int(whole["counts"]["visible"] or 0)
        self.log(f"  100% principal sees {total_rows:,} at zoom 0 over the whole extent")

        # Density deciles, ranked once under the 100% principal and reused by every rung.
        pools: dict[int, list[list[tuple[list[float], int]]]] = {}
        for zoom in zooms:
            boxes = candidate_boxes(quant, zoom, args.candidates, self.rng)
            ranked = rank_by_density(
                args.viewer, broad_token, view_id, zoom, boxes, self.log
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
            self.log(f"  principal target {rung['target']:.0%}: {len(terms)} term(s) {terms}")
            token, authorise_s = authorise(args.session, args.session_cred, terms)
            # The first viewport of a fresh session is its own figure: it carries the
            # `(view, principal)` fragment build, which is not a per-request cost and must not be
            # averaged into one.
            t0 = time.perf_counter()
            first = viewport(args.viewer, token, view_id, 0, full_extent, k=args.k)
            first_s = time.perf_counter() - t0
            measured = int(first["counts"]["visible"] or 0)
            self.log(
                f"    authorise {authorise_s*1000:.0f} ms, first viewport {first_s:.2f} s, "
                f"measured coverage {measured/max(total_rows,1):.4%}"
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
                            "density_visible_100pc": density,
                            "distinct_locations": min(args.samples, len(rotated)),
                            "conditions": {},
                        }
                        for condition in args.conditions.split(","):
                            if condition == "hot":
                                samples = self._hot_samples(
                                    token, view_id, zoom, box, args.samples
                                )
                            elif condition == "cold":
                                samples = self._cold_samples(
                                    terms, view_id, zoom, rotated, args.samples, True
                                )
                            elif condition == "cold_pages_warm_engine":
                                samples = self._cold_samples(
                                    terms, view_id, zoom, rotated, args.samples, False
                                )
                            else:
                                raise SystemExit(f"unknown condition {condition!r}")
                            cell["conditions"][condition] = condition_figures(
                                samples, cold=condition != "hot"
                            )
                        cells.append(cell)
                        self.log(
                            f"    zoom {zoom} decile {decile}.{which} "
                            f"density={density:,} "
                            + " ".join(
                                f"{c}:p50={cell['conditions'][c]['wall_ms']['p50']}"
                                for c in cell["conditions"]
                            )
                        )

            rung_out = {
                "target": rung["target"],
                "terms": terms,
                "terms_n": len(terms),
                "rule": rung.get("rule"),
                "measured": round(measured / max(total_rows, 1), 6),
                "measured_visible": measured,
                "authorise_s": round(authorise_s, 4),
                "first_viewport_s": round(first_s, 4),
                "cells": cells,
            }
            rung_out["battery"] = battery_figures(cells)
            ladder_out.append(rung_out)

        text = self._text_and_drilldown(token, view_id, view_id, quant, pools, zooms)

        return {
            "cap": args.cap_bytes,
            "cgroup": str(args.cgroup) if args.cgroup else None,
            "view": view_id,
            "total_rows": total_rows,
            "all_terms_authorise_s": round(broad_authorise_s, 4),
            "candidates_per_zoom": args.candidates,
            "samples_per_cell": args.samples,
            "conditions": args.conditions.split(","),
            "zooms": zooms,
            "deciles": deciles,
            "cells_per_decile": args.cells_per_decile,
            "evictions": self.evictor.evictions,
            "cgroup_at_end": self.evictor.cgroup_events(),
            "oom_kill_seen": self.oom_seen,
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
        for side in ("server_ms", "wall_ms"):
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
    ap.add_argument("--candidates", type=int, default=500, help="candidate boxes per zoom")
    ap.add_argument("--conditions", default="cold,cold_pages_warm_engine,hot")
    ap.add_argument("--k", type=int, default=30)
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
        )
        served.clear_scratch()
        open_at = time.time()
        served.start()
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
            result_open = round(time.time() - open_at, 2)
            served.stop()
    if served is not None:
        result["open_s"] = result_open
    result["ran_s"] = round(time.time() - started, 1)
    Path(args.out).write_text(json.dumps(result, indent=2))
    print(f"wrote {args.out} in {result['ran_s']} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
