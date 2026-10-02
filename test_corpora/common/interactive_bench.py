"""How interactive a served bundle is: opening a session, moving the map, and looking items up.

A performance benchmark, not a correctness test. It sends a fixed list of requests, takes a few
minutes, and writes figures that compare between runs:

* **Session.** For principals seeing about 1%, 7%, 85% and 100% of the corpus: the
  `/session/authorise` latency, then the time from the token to the first map answer at zoom 0
  over the whole extent, once for a principal the server has not seen and once more for the same
  principal.
* **Map.** For the 1%, 7% and 100% principals, a scripted session from the whole extent the session opened on:
  a zoom into Europe and then the eastern United States down to zoom 14, with pans on the way,
  and back out after each. For the narrowest principal the script is played again from the same
  opened state, sending each request a second time to a server that has answered it once; the
  broad principals' second whole-extent opening is their repeat.
* **Lookups.** For the narrowest and the broadest principal: an item card by `tessera_id`, and a
  filter `eq` on a unique field answering one item, for 200 items the map served.

Requests are the TypeScript viewer's, as its store and driver send them, at the demo's defaults:
a screen of 1600 by 900, a mark budget of 500,000, `k` = `selection.k_max_marks`, the depth chosen
from the counts the server returned, only the tiles the client does not already hold, split into
pieces of at most 25,000 tiles, the bundle's first layer named for colour, and one artifact
request per view. The client's background prefetch ring and idle-time artifact promotion are not
sent.

"First time" is made real on every run by giving each principal a term set no earlier run used:
the composed set plus a subset, unique to the run, of the corpus's smallest terms. The server
caches a principal's visible set in memory and under the deployment's cache directory, keyed on
the sorted set of granted terms, so a set it has never seen is built from the postings. The salt
moves a principal's coverage by well under one per cent of itself.

Attach to a server that is up (the default reads its addresses from the deployment):

    python3 -m test_corpora.common.interactive_bench \\
        --deployment /home/joe/code/tessera/data/ladder/gbif --out run.json [--compare old.json]

Or start one under a memory cap, measure its open, and leave it up for later attached runs:

    python3 -m test_corpora.common.interactive_bench \\
        --deployment /home/joe/code/tessera/data/ladder/gbif --start \\
        --binary /path/to/tessera-serve --keep-serving --out start.json
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import math
import os
import platform
import random
import socket
import subprocess
import sys
import time
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path

import requests
from pyarrow import ipc

from .deployment import Deployment, read_env_file, tomllib
from .serve_battery import compose_ladder, drain, frames

# ---------------------------------------------------------------------------------------------
# The TypeScript client's constants (clients/ts/core/src)
# ---------------------------------------------------------------------------------------------

SCREEN = (1600, 900)
#: The demo viewer's mark budget, `DEFAULT_BUDGET` in viewer/src/main.ts.
BUDGET = 500_000
WORLD = 512.0
CELL_GRID = 65536
MIN_DEPTH = 3
MAX_DEPTH = 16
MIN_GAIN_PER_STEP = 0.15
#: Tiles per piece of one region's fetch, `MAX_TILES_PER_REQUEST` in replica.ts.
PIECE_TILES = 25_000
BASE_ARTIFACT_BUDGET = 48
MAX_ARTIFACT_BUDGET = 2048
M_TARGET_MIN_FACTOR, M_TARGET_MAX_FACTOR, DAMPING = 0.25, 4.0, 0.5

FRAME_TILES, FRAME_POINTS, FRAME_RECORDS = 1, 3, 7

TARGETS = (0.01, 0.07, 0.85, 1.0)
#: The principals the map script is played for. The 85% principal is left out to shorten a run;
#: at the low zooms its views cost what the 100% principal's do.
MAP_TARGETS = (0.01, 0.07, 1.0)
#: The smallest terms, from which each run draws its principals' salt.
SALT_POOL = 16
LOOKUPS = 200
SEED = 20261002

#: Where each scripted region zooms in, as longitude and latitude, on a geographic view.
REGIONS_LONLAT = {
    "europe": (4.5, 51.0),
    "eastern-us": (-75.5, 40.0),
}
ZOOMS_IN = (3, 6, 9, 12, 14)
#: The zooms at which the script pans, half a screen east, then (at the deepest) south too.
PAN_AT = {6: ("east",), 9: ("east",), 14: ("east", "south")}

BANDS = (
    (0, 0, "z0"),
    (1, 4, "z1-4"),
    (5, 8, "z5-8"),
    (9, 11, "z9-11"),
    (12, 16, "z12-14"),
)


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------------------------
# Geometry, as coords.ts, budget.ts and prefetch.ts compute it
# ---------------------------------------------------------------------------------------------


def fit_zoom() -> float:
    """The deck zoom at which the whole world fits the screen: map zoom 0."""
    return math.log2(min(SCREEN) / WORLD)


def world_bbox(
    target: tuple[float, float], deck_zoom: float
) -> tuple[float, float, float, float]:
    scale = 2**deck_zoom
    hw, hh = SCREEN[0] / 2 / scale, SCREEN[1] / 2 / scale

    def clamp(v: float) -> float:
        return min(WORLD, max(0.0, v))

    return (
        clamp(target[0] - hw),
        clamp(target[1] - hh),
        clamp(target[0] + hw),
        clamp(target[1] + hh),
    )


def tile_rect(bbox, depth: int) -> tuple[int, int, int, int]:
    span = WORLD / 2**depth
    top = 2**depth - 1

    def index(v: float) -> int:
        return min(top, max(0, math.floor(v / span)))

    return (index(bbox[0]), index(bbox[1]), index(bbox[2]), index(bbox[3]))


def rect_area(r) -> int:
    return (r[2] - r[0] + 1) * (r[3] - r[1] + 1)


def rect_bbox(r, depth: int, q: dict) -> list[float]:
    """`rectToRequestBbox`: a closed bbox naming exactly the tiles of `r`."""
    span = CELL_GRID / 2**depth
    sx, sy = q["x_max"] - q["x_min"], q["y_max"] - q["y_min"]
    return [
        q["x_min"] + (r[0] * span + 0.5) / CELL_GRID * sx,
        q["y_min"] + (r[1] * span + 0.5) / CELL_GRID * sy,
        q["x_min"] + ((r[2] + 1) * span - 0.5) / CELL_GRID * sx,
        q["y_min"] + ((r[3] + 1) * span - 0.5) / CELL_GRID * sy,
    ]


def tile_xy(prefix: int, depth: int) -> tuple[int, int]:
    x = y = 0
    for bit in range(depth):
        x |= ((prefix >> (2 * bit)) & 1) << bit
        y |= ((prefix >> (2 * bit + 1)) & 1) << bit
    return x, y


def split_rect(r, max_tiles: int) -> list[tuple[int, int, int, int]]:
    width = r[2] - r[0] + 1
    rows = max(1, max_tiles // width)
    if r[3] - r[1] + 1 <= rows:
        return [r]
    return [
        (r[0], y, r[2], min(r[3], y + rows - 1)) for y in range(r[1], r[3] + 1, rows)
    ]


def runs_to_rects(missing: set[tuple[int, int]]) -> list[tuple[int, int, int, int]]:
    """Cover a set of tiles with rectangles: runs along each row, merged down while identical."""
    rows: dict[int, list[tuple[int, int]]] = {}
    for x, y in missing:
        rows.setdefault(y, []).append(x)
    open_: dict[tuple[int, int], list[int]] = {}
    out = []
    for y in sorted(rows):
        xs = sorted(rows[y])
        runs, start = [], xs[0]
        for a, b in zip(xs, xs[1:] + [None]):
            if b != a + 1:
                runs.append((start, a))
                start = b
        still = {}
        for run in runs:
            if run in open_ and open_[run][1] == y - 1:
                still[run] = [open_[run][0], y]
            else:
                still[run] = [y, y]
        for run, (y0, y1) in open_.items():
            if run not in still or still[run][0] != y0:
                out.append((run[0], y0, run[1], y1))
        open_ = still
    out.extend((run[0], y0, run[1], y1) for run, (y0, y1) in open_.items())
    return out


def artifact_budget_for(zoom: float) -> int:
    return min(MAX_ARTIFACT_BUDGET, round(BASE_ARTIFACT_BUDGET * 2 ** max(0.0, zoom)))


def declared_levels_at(layer: dict, zoom: float) -> list[int]:
    declared = layer.get("levels") or []
    z = math.floor(zoom)
    if not declared:
        return []
    if not any(d.get("zoom") is not None for d in declared):
        return [d["level"] for d in declared]
    return [
        d["level"]
        for d in declared
        if d.get("zoom") is None or d["zoom"][0] <= z <= d["zoom"][1]
    ]


# ---------------------------------------------------------------------------------------------
# One viewer: its held tiles, its counts, and the requests it sends for a view
# ---------------------------------------------------------------------------------------------


class Viewer:
    """The TypeScript store's request path for one session, without drawing: which depth a view
    asks for, which tiles it already holds, and the pieces it sends for the rest."""

    def __init__(self, base: str, token: str, meta: dict, view: dict, keep_ids: bool):
        self.base, self.token, self.meta, self.view = base, token, meta, view
        self.q = view["quantisation"]
        sel = meta["selection"]
        self.k = int(sel["k_max_marks"])
        self.theta = float(sel["theta_target_marks"])
        self.m_target = self.theta
        self.max_tiles = int(sel["max_tiles_per_request"])
        layers = meta.get("layers") or []
        self.layer = layers[0] if layers else None
        #: Per depth, every tile whose masked count a response gave; one it omitted holds 0.
        self.counts: dict[int, dict[tuple[int, int], int]] = {}
        #: Per depth, the tiles whose marks the client holds.
        self.held: dict[int, set[tuple[int, int]]] = {}
        self.last_visible: int | None = None
        self.seeded = False
        #: Per (depth, tile), the marks the last response served there.
        self.served: dict[tuple[int, tuple[int, int]], int] = {}
        self.keep_ids = keep_ids
        self.ids: set[int] = set()
        self.http = requests.Session()
        self.http.headers["Authorization"] = f"Bearer {token}"

    def clone(self, keep_ids: bool) -> Viewer:
        """A viewer holding what this one holds, as if it had shown the same views."""
        other = Viewer(self.base, self.token, self.meta, self.view, keep_ids)
        other.counts = {d: dict(c) for d, c in self.counts.items()}
        other.held = {d: set(h) for d, h in self.held.items()}
        other.served = dict(self.served)
        other.m_target, other.last_visible, other.seeded = (
            self.m_target,
            self.last_visible,
            self.seeded,
        )
        return other

    # -- the wire --------------------------------------------------------------------------

    def post_viewport(self, body: dict) -> dict:
        """One `/v1/viewport`. A body the server cuts at its stream deadline is a result, timed to
        the cut and marked `shed`, with whatever whole frames arrived before it."""
        t0 = time.perf_counter()
        r = self.http.post(
            f"{self.base}/v1/viewport", json=body, timeout=600, stream=True
        )
        if r.status_code != 200:
            content, shed = r.content, None
        else:
            content, shed = drain(r)
        wall = (time.perf_counter() - t0) * 1000.0
        out = {
            "ms": wall,
            "status": r.status_code,
            "bytes": len(content),
            "tiles": {},
            "served": {},
        }
        out["server_ms"] = int(r.headers.get("x-tessera-server-us", "0")) / 1000.0
        if shed is not None:
            out["shed"] = True
        if r.status_code != 200:
            out["error"] = content[:300].decode(errors="replace")
            return out
        for kind, payload in frames(content):
            if kind == FRAME_TILES:
                t = ipc.open_stream(io.BytesIO(payload)).read_all()
                tiles = t.column("tile").to_pylist()
                visible = t.column("visible").to_pylist()
                served = (
                    t.column("served").to_pylist()
                    if "served" in t.column_names
                    else [0] * len(tiles)
                )
                depth = body["zoom"]
                for prefix, v, s in zip(tiles, visible, served):
                    xy = tile_xy(int(prefix), depth)
                    out["tiles"][xy] = int(v)
                    out["served"][xy] = int(s or 0)
            elif kind == FRAME_POINTS and self.keep_ids:
                t = ipc.open_stream(io.BytesIO(payload)).read_all()
                name = "tessera_id" if "tessera_id" in t.column_names else "id"
                if name in t.column_names:
                    self.ids.update(int(i) for i in t.column(name).to_pylist())
        return out

    # -- the depth, as budget.ts chooses it ------------------------------------------------

    def counts_for(self, visible_bbox) -> int | None:
        """The deepest depth whose known counts cover the view."""
        best = None
        for depth, cells in self.counts.items():
            r = tile_rect(visible_bbox, depth)
            if rect_area(r) > len(cells):
                continue
            covered = all(
                (x, y) in cells
                for x in range(r[0], r[2] + 1)
                for y in range(r[1], r[3] + 1)
            )
            if covered and (best is None or depth > best):
                best = depth
        return best

    def cells_in(self, depth: int, r) -> list[tuple[tuple[int, int], int]]:
        """The non-empty counted cells of `depth` inside the tile rectangle `r`."""
        cells = self.counts[depth]
        if rect_area(r) <= 4 * len(cells):
            found = (
                ((x, y), cells.get((x, y), 0))
                for x in range(r[0], r[2] + 1)
                for y in range(r[1], r[3] + 1)
            )
        else:
            found = (
                (xy, c)
                for xy, c in cells.items()
                if r[0] <= xy[0] <= r[2] and r[1] <= xy[1] <= r[3]
            )
        return [(xy, c) for xy, c in found if c > 0]

    def predict(self, field_depth: int, inside, depth: int) -> tuple[int, bool]:
        """`sumCells`: the marks a request at `depth` costs, from counts held at `field_depth`,
        and whether any cell reached the cap."""
        delta = depth - field_depth
        if delta >= 0:
            cap = self.k * 4**delta
            return sum(min(cap, c) for _, c in inside), any(c >= cap for _, c in inside)
        ancestors: dict[tuple[int, int], int] = {}
        for (x, y), c in inside:
            key = (x >> -delta, y >> -delta)
            ancestors[key] = ancestors.get(key, 0) + c
        return sum(min(self.k, c) for c in ancestors.values()), any(
            c >= self.k for c in ancestors.values()
        )

    def choose_depth(self, bbox) -> tuple[int, str, float]:
        """`chooseDepth`: the depth, where its prediction came from, and the average model's
        figure for `calibrate`."""
        wanted = max(1.0, BUDGET / max(1.0, self.m_target))
        field = self.counts_for(bbox)
        inside = (
            self.cells_in(field, tile_rect(bbox, field)) if field is not None else None
        )

        def average(depth: int) -> float:
            a = rect_area(tile_rect(bbox, depth)) * self.m_target
            return a if self.last_visible is None else min(a, self.last_visible)

        depth, fitting = MIN_DEPTH, []
        for d in range(MIN_DEPTH, MAX_DEPTH + 1):
            tiles = rect_area(tile_rect(bbox, d))
            if tiles > self.max_tiles:
                break
            if field is not None:
                marks, capped = self.predict(field, inside, d)
                if d > MIN_DEPTH and marks > BUDGET:
                    break
                depth = d
                fitting.append((d, marks))
                if not capped:
                    break
                continue
            depth = d
            if tiles >= wanted:
                break
            if (
                self.last_visible is not None
                and tiles * self.m_target >= self.last_visible
            ):
                break
        if len(fitting) > 1:
            best = fitting[-1][1]
            enough = next(
                (
                    d
                    for d, m in fitting
                    if d >= field and m >= best * (1 - MIN_GAIN_PER_STEP)
                ),
                None,
            )
            if enough is not None and enough < depth:
                depth = enough
        return depth, ("counts" if field is not None else "average"), average(depth)

    # -- one view ----------------------------------------------------------------------------

    def show(self, target: tuple[float, float], map_zoom: float) -> dict:
        """Everything the viewer sends for one settled view, timed request by request."""
        deck_zoom = map_zoom + fit_zoom()
        bbox = world_bbox(target, deck_zoom)
        step: dict = {"map_zoom": map_zoom, "requests": []}
        t_view = time.perf_counter()

        depth, source, avg = self.choose_depth(bbox)
        if not self.seeded and source == "average":
            # The cold view fetches counts before marks, over the visible box at the average
            # model's depth.
            r = tile_rect(bbox, depth)
            res = self.post_viewport(
                {
                    "view": self.view["id"],
                    "zoom": depth,
                    "bbox": rect_bbox(r, depth, self.q),
                    "k": 0,
                    "layers": [],
                }
            )
            self._adopt(depth, r, res, marks=False)
            self.last_visible = sum(res["tiles"].values())
            self.seeded = True
            step["requests"].append(self._sample("seed", depth, res))
            step["counts_ms"] = (time.perf_counter() - t_view) * 1000.0
            depth, source, avg = self.choose_depth(bbox)
        self.seeded = True

        visible = tile_rect(bbox, depth)
        held = self.held.get(depth, set())
        missing = {
            (x, y)
            for x in range(visible[0], visible[2] + 1)
            for y in range(visible[1], visible[3] + 1)
            if (x, y) not in held
        }
        pieces = (
            [p for r in runs_to_rects(missing) for p in split_rect(r, PIECE_TILES)]
            if missing
            else []
        )
        cx, cy = (visible[0] + visible[2]) / 2, (visible[1] + visible[3]) / 2
        pieces.sort(
            key=lambda p: ((p[0] + p[2]) / 2 - cx) ** 2 + ((p[1] + p[3]) / 2 - cy) ** 2
        )
        point_layers = [self.layer["name"]] if self.layer else []
        marks_ms = 0.0
        for piece in pieces:
            body = {
                "view": self.view["id"],
                "zoom": depth,
                "bbox": rect_bbox(piece, depth, self.q),
                "k": self.k,
                "layers": point_layers,
            }
            if point_layers:
                body["artifact_budget"] = artifact_budget_for(deck_zoom)
                levels = declared_levels_at(self.layer, deck_zoom)
                if levels:
                    body["levels"] = levels
            res = self.post_viewport(body)
            self._adopt(depth, piece, res, marks=True)
            marks_ms += res["ms"]
            step["requests"].append(self._sample("marks", depth, res))

        # The visible count and the served marks over the visible box, for the average model.
        cells = self.counts.get(depth, {})
        in_view = [
            (xy, c)
            for xy, c in cells.items()
            if visible[0] <= xy[0] <= visible[2] and visible[1] <= xy[1] <= visible[3]
        ]
        visible_n = sum(c for _, c in in_view)
        actual = sum(self.served.get((depth, xy), 0) for xy, _ in in_view)
        if pieces:
            if actual < visible_n and actual > 0 and avg > 0:
                damped = self.m_target * (1 + (actual / avg - 1) * DAMPING)
                self.m_target = min(
                    self.theta * M_TARGET_MAX_FACTOR,
                    max(self.theta * M_TARGET_MIN_FACTOR, damped),
                )
            self.last_visible = visible_n
        step["marks_ms"] = marks_ms
        step["view_ms"] = (time.perf_counter() - t_view) * 1000.0

        if point_layers:
            levels = declared_levels_at(self.layer, deck_zoom)
            body = {
                "view": self.view["id"],
                "zoom": depth,
                "bbox": rect_bbox(visible, depth, self.q),
                "k": 0,
                "layers": point_layers,
                "computed": ["centroid", "box"],
                "artifact_budget": artifact_budget_for(deck_zoom),
            }
            if levels:
                body["levels"] = levels
            res = self.post_viewport(body)
            step["requests"].append(self._sample("artifacts", depth, res))
            step["artifacts_ms"] = res["ms"]

        step.update(
            depth=depth,
            source=source,
            pieces=len(pieces),
            visible=visible_n,
            served=actual,
        )
        step["shed"] = sum(1 for r in step["requests"] if r.get("shed"))
        step["errors"] = sum(1 for r in step["requests"] if r["status"] != 200)
        return step

    def _adopt(self, depth: int, r, res: dict, marks: bool) -> None:
        cells = self.counts.setdefault(depth, {})
        held = self.held.setdefault(depth, set())
        for x in range(r[0], r[2] + 1):
            for y in range(r[1], r[3] + 1):
                cells[(x, y)] = res["tiles"].get((x, y), 0)
                if marks:
                    held.add((x, y))
        for xy, s in res["served"].items():
            self.served[(depth, xy)] = s

    @staticmethod
    def _sample(kind: str, depth: int, res: dict) -> dict:
        return {
            "kind": kind,
            "depth": depth,
            "ms": round(res["ms"], 2),
            "server_ms": round(res["server_ms"], 2),
            "bytes": res["bytes"],
            "status": res["status"],
            "served": sum(res["served"].values()),
            **({"error": res["error"]} if "error" in res else {}),
            **({"shed": True} if res.get("shed") else {}),
        }


# ---------------------------------------------------------------------------------------------
# The script
# ---------------------------------------------------------------------------------------------


def region_targets(
    view: dict, fallback_counts: dict[tuple[int, int], int], depth: int
) -> dict:
    """World-space centres of the three regions: named places on a web-Mercator view framed as
    the unit square, or elsewhere the three densest cells in distinct depth-3 ancestors."""
    q = view["quantisation"]
    unit = (q["x_min"], q["y_min"], q["x_max"], q["y_max"]) == (0.0, 0.0, 1.0, 1.0)
    if view.get("projection") == "web_mercator" and unit:
        out = {}
        for name, (lon, lat) in REGIONS_LONLAT.items():
            merc = math.log(math.tan(math.pi / 4 + math.radians(lat) / 2)) / math.pi
            y = (1 - merc) / 2 if view.get("tile_scheme") == "xyz" else (1 + merc) / 2
            out[name] = ((lon + 180.0) / 360.0 * WORLD, y * WORLD)
        return out
    chosen, seen = {}, set()
    for (x, y), _ in sorted(fallback_counts.items(), key=lambda kv: (-kv[1], kv[0])):
        ancestor = (x >> (depth - 3), y >> (depth - 3))
        if ancestor in seen:
            continue
        seen.add(ancestor)
        span = WORLD / 2**depth
        chosen[f"dense-{len(chosen) + 1}"] = ((x + 0.5) * span, (y + 0.5) * span)
        if len(chosen) == 3:
            break
    return chosen


def script(regions: dict) -> list[dict]:
    """The scripted session after the whole extent is shown: each region zoomed into with pans,
    then the whole extent again."""
    centre = (WORLD / 2, WORLD / 2)
    steps = []
    for name, target in regions.items():
        prev = 0
        for z in ZOOMS_IN:
            steps.append(
                {
                    "kind": "zoom-in",
                    "region": name,
                    "zoom": z,
                    "target": target,
                    "from": prev,
                }
            )
            prev = z
            at = target
            for direction in PAN_AT.get(z, ()):
                scale = 2 ** (z + fit_zoom())
                dx = SCREEN[0] / 2 / scale if direction == "east" else 0.0
                dy = -SCREEN[1] / 2 / scale if direction == "south" else 0.0
                at = (at[0] + dx, at[1] + dy)
                steps.append({"kind": "pan", "region": name, "zoom": z, "target": at})
        steps.append(
            {"kind": "zoom-out", "region": "world", "zoom": 0, "target": centre}
        )
    return steps


# ---------------------------------------------------------------------------------------------
# Principals
# ---------------------------------------------------------------------------------------------


def principals(ranks: list[dict], run_id: str) -> list[dict]:
    """The four principals, each salted with a subset of the smallest terms unique to this run."""
    by_size = sorted(ranks, key=lambda r: (r["pairs"], r["term"]))
    pool = [r["term"] for r in by_size[:SALT_POOL]]
    rest = [r for r in ranks if r["term"] not in pool]
    total = sum(r["pairs"] for r in ranks)
    out = []
    for entry in compose_ladder(rest, total, TARGETS):
        label = f"{round(entry['target'] * 100)}%"
        digest = hashlib.sha256(f"{run_id}/{label}".encode()).digest()
        # Never empty and never the whole pool, so the 100% principal also differs per run.
        mask = int.from_bytes(digest[:8], "little") % (2**SALT_POOL - 2) + 1
        salt = [t for i, t in enumerate(pool) if mask >> i & 1]
        out.append(
            {
                "label": label,
                "target": entry["target"],
                "terms": sorted(set(entry["terms"]) | set(salt)),
                "base_terms": len(entry["terms"]),
                "salt": salt,
            }
        )
    return out


def authorise(session_base: str, cred: str, terms: Sequence[str]) -> tuple[str, float]:
    auth_data = base64.b64encode(json.dumps({"terms": list(terms)}).encode()).decode()
    t0 = time.perf_counter()
    r = requests.post(
        f"{session_base}/session/authorise",
        headers={"Authorization": f"Bearer {cred}"},
        json={"auth_data": auth_data},
        timeout=300,
    )
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    return r.json()["token"], ms


def read_meta(viewer_base: str, token: str) -> tuple[dict, float]:
    t0 = time.perf_counter()
    r = requests.get(
        f"{viewer_base}/v1/meta",
        headers={"Authorization": f"Bearer {token}"},
        timeout=120,
    )
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    return r.json(), ms


# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------


def stats(values: Sequence[float]) -> dict:
    v = sorted(values)
    if not v:
        return {"n": 0}

    def rank(p: float) -> float:
        return v[min(len(v) - 1, max(0, math.ceil(p / 100 * len(v)) - 1))]

    return {
        "n": len(v),
        "median": round(rank(50), 2),
        "p95": round(rank(95), 2),
        "max": round(v[-1], 2),
    }


class Bench:
    def __init__(self, args, cred: str):
        self.args, self.cred = args, cred
        self.measurements: dict[str, dict] = {}
        self.detail: dict = {}

    def put(self, key: str, values: Sequence[float], **extra) -> None:
        self.measurements[key] = {**stats(values), **extra}
        m = self.measurements[key]
        if m["n"]:
            log(
                f"  {key:<44} n={m['n']:<4} median {m['median']:>9.1f} ms  p95 {m['p95']:>9.1f}  max {m['max']:>9.1f}"
            )

    def open_view(self, token: str, keep_ids: bool = False) -> tuple[Viewer, dict]:
        """What the viewer does with a new token: read `/v1/meta`, then show the whole extent."""
        t0 = time.perf_counter()
        meta, meta_ms = read_meta(self.args.viewer, token)
        view = next(
            v
            for v in meta["views"]
            if v["id"] == (self.args.view or meta["views"][0]["id"])
        )
        viewer = Viewer(self.args.viewer, token, meta, view, keep_ids)
        step = viewer.show((WORLD / 2, WORLD / 2), 0)
        opened = {
            "meta_ms": round(meta_ms, 2),
            "first_counts_ms": round(
                meta_ms + step.get("counts_ms", step["marks_ms"]), 2
            ),
            "first_marks_ms": round(
                (time.perf_counter() - t0) * 1000.0 - step.get("artifacts_ms", 0.0), 2
            ),
            "first_view_ms": round((time.perf_counter() - t0) * 1000.0, 2),
            "step": step,
        }
        return viewer, opened

    def run(self) -> dict:
        ranks = json.loads(Path(self.args.ranks).read_text())
        run_id = str(time.time_ns())
        people = principals(ranks, run_id)
        self.detail["run_id"] = run_id
        self.detail["principals"] = []
        tokens: dict[str, str] = {}
        openers: dict[str, Viewer] = {}
        full_counts: dict = {}
        full_depth = 0
        meta_view = None

        log("session initiation")
        for p in people:
            token1, auth1 = authorise(self.args.session, self.cred, p["terms"])
            viewer1, open1 = self.open_view(token1)
            token2, auth2 = authorise(self.args.session, self.cred, p["terms"])
            opened_again, open2 = self.open_view(token2)
            tokens[p["label"]] = token2
            openers[p["label"]] = opened_again
            seed = open1["step"]["requests"][0]
            visible = (
                viewer1.last_visible
                if seed["kind"] != "seed"
                else sum(viewer1.counts[seed["depth"]].values())
            )
            p["visible"] = visible
            if p["target"] == 1.0:
                full_depth = seed["depth"]
                full_counts = viewer1.counts.get(full_depth, {})
                meta_view = viewer1.view
            self.detail["principals"].append(
                {**p, "open_first": open1, "open_again": open2}
            )
            label = p["label"]
            for which, auth, opened in (
                ("first", auth1, open1),
                ("again", auth2, open2),
            ):
                self.put(f"session.{label}.authorise.{which}", [auth])
                self.put(
                    f"session.{label}.first_counts.{which}", [opened["first_counts_ms"]]
                )
                self.put(
                    f"session.{label}.first_marks.{which}",
                    [opened["first_marks_ms"]],
                    shed=opened["step"]["shed"],
                    errors=opened["step"]["errors"],
                )
        corpus = max(p["visible"] for p in people) or 1
        for p in people:
            p["share"] = round(p["visible"] / corpus, 4)
            log(
                f"  principal {p['label']}: {len(p['terms'])} terms, {p['visible']:,} visible ({p['share']:.2%})"
            )

        regions = region_targets(meta_view, full_counts, full_depth)
        steps = script(regions)
        self.detail["regions"] = {
            k: [round(c, 3) for c in v] for k, v in regions.items()
        }
        self.detail["map"] = {}
        ids: dict[str, list[int]] = {}
        log(f"map ({len(steps)} views a pass after the whole extent)")
        for p in people:
            if p["target"] not in MAP_TARGETS:
                continue
            label = p["label"]
            keep = label in (people[0]["label"], people[-1]["label"])
            passes = {}
            for pass_name in (
                ("fresh", "repeat") if label == people[0]["label"] else ("fresh",)
            ):
                # Each pass starts where the session's second opening left the client: holding
                # the whole extent.
                viewer = openers[label].clone(keep and pass_name == "fresh")
                done = []
                t0 = time.perf_counter()
                for s in steps:
                    step = viewer.show(s["target"], s["zoom"])
                    done.append(
                        {**{k: v for k, v in s.items() if k != "target"}, **step}
                    )
                passes[pass_name] = {
                    "steps": done,
                    "wall_s": round(time.perf_counter() - t0, 2),
                }
                if keep and pass_name == "fresh":
                    ids[label] = sorted(viewer.ids)
            self.detail["map"][label] = passes
            self._map_figures(label, passes)

        self.detail["lookups"] = {}
        log("lookups")
        for label, served in ids.items():
            self._lookups(label, tokens[label], served, meta_view["id"])
        return {
            "principals": [
                {k: p[k] for k in ("label", "terms", "salt", "visible", "share")}
                for p in people
            ]
        }

    def _map_figures(self, label: str, passes: dict) -> None:
        fresh = passes["fresh"]["steps"]
        repeat = passes["repeat"]["steps"] if "repeat" in passes else []
        for lo, hi, band in BANDS:

            def in_band(s: dict, lo: int = lo, hi: int = hi) -> bool:
                return lo <= s["zoom"] <= hi

            groups = {
                "zoom_in": [
                    s for s in fresh if s["kind"] == "zoom-in" and s["pieces"] > 0
                ],
                "pan": [s for s in fresh if s["kind"] == "pan" and s["pieces"] > 0],
                "repeat": [
                    s
                    for s in repeat
                    if s["kind"] in ("zoom-in", "pan") and s["pieces"] > 0
                ],
                "artifacts": [s for s in fresh if "artifacts_ms" in s],
            }
            for name, steps in groups.items():
                chosen = [s for s in steps if in_band(s)]
                if not chosen:
                    continue
                field = "artifacts_ms" if name == "artifacts" else "view_ms"
                self.put(
                    f"map.{label}.{band}.{name}",
                    [s[field] for s in chosen],
                    shed=sum(s["shed"] for s in chosen),
                    errors=sum(s["errors"] for s in chosen),
                )
        self.put(f"map.{label}.pass.fresh_total", [passes["fresh"]["wall_s"] * 1000.0])
        if "repeat" in passes:
            self.put(
                f"map.{label}.pass.repeat_total", [passes["repeat"]["wall_s"] * 1000.0]
            )

    def _lookups(self, label: str, token: str, ids: list[int], view_id: str) -> None:
        rng = random.Random(SEED)
        chosen = rng.sample(ids, min(LOOKUPS, len(ids)))
        http = requests.Session()
        http.headers["Authorization"] = f"Bearer {token}"
        card_ms, filter_ms, unique_values, failures = [], [], [], 0
        for tid in chosen:
            t0 = time.perf_counter()
            r = http.post(f"{self.args.viewer}/v1/items/{tid}", json={}, timeout=120)
            ms = (time.perf_counter() - t0) * 1000.0
            if r.status_code != 200:
                failures += 1
                continue
            card_ms.append(ms)
            value = r.json()["fields"].get(self.args.unique_field)
            if value is not None:
                unique_values.append(value)
        card_failures = failures
        rows = []
        for value in unique_values:
            body = {
                "view": view_id,
                "fields": [self.args.unique_field],
                "filters": {self.args.unique_field: {"eq": str(value)}},
                "page_rows": 1,
                "pages": 1,
            }
            t0 = time.perf_counter()
            r = http.post(f"{self.args.viewer}/v1/items", json=body, timeout=120)
            content = r.content
            ms = (time.perf_counter() - t0) * 1000.0
            if r.status_code != 200:
                failures += 1
                continue
            filter_ms.append(ms)
            rows.append(
                sum(
                    ipc.open_stream(io.BytesIO(payload)).read_all().num_rows
                    for kind, payload in frames(content)
                    if kind == FRAME_RECORDS
                )
            )
        self.detail["lookups"][label] = {
            "ids_served": len(ids),
            "asked": len(chosen),
            "failures": failures,
            "filter_rows": {str(n): rows.count(n) for n in sorted(set(rows))},
        }
        self.put(f"lookup.{label}.item_card", card_ms, errors=card_failures)
        self.put(
            f"lookup.{label}.filter_eq_{self.args.unique_field}",
            filter_ms,
            errors=failures - card_failures,
        )


# ---------------------------------------------------------------------------------------------
# The server, the box, and the comparison
# ---------------------------------------------------------------------------------------------


def deployment_settings(directory: Path) -> dict:
    return tomllib.loads((directory / "tessera.toml").read_text())


def port_of(address: str) -> int:
    return int(address.rsplit(":", 1)[1])


def port_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        return s.connect_ex(("127.0.0.1", port)) != 0


def listening_pid(port: int) -> int | None:
    out = subprocess.run(
        ["ss", "-ltnpH", f"sport = :{port}"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout
    marker = "pid="
    if marker not in out:
        return None
    return int(out.split(marker, 1)[1].split(",", 1)[0])


def process_memory(pid: int | None) -> dict:
    if not pid:
        return {}
    out = {}
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(("VmRSS:", "VmHWM:", "RssAnon:", "RssFile:")):
                key, value = line.split(":", 1)
                out[key.lower() + "_gib"] = round(int(value.split()[0]) / 2**20, 2)
    except OSError:
        return {}
    return out


def memory_max(pid: int | None) -> str | None:
    """The `memory.max` of the server's cgroup: its memory cap, or `max` where it has none."""
    if not pid:
        return None
    try:
        relative = Path(f"/proc/{pid}/cgroup").read_text().strip().split("::", 1)[-1]
        return (
            (Path("/sys/fs/cgroup") / relative.lstrip("/") / "memory.max")
            .read_text()
            .strip()
        )
    except OSError:
        return None


def box() -> dict:
    cpu = next(
        (
            line.split(":", 1)[1].strip()
            for line in Path("/proc/cpuinfo").read_text().splitlines()
            if line.startswith("model name")
        ),
        platform.processor(),
    )
    mem_kb = int(
        next(
            line
            for line in Path("/proc/meminfo").read_text().splitlines()
            if line.startswith("MemTotal")
        ).split()[1]
    )
    return {
        "host": socket.gethostname(),
        "cpu": cpu,
        "cpus": os.cpu_count(),
        "memory_gib": round(mem_kb / 2**20, 1),
        "kernel": platform.release(),
    }


def table(measurements: dict) -> str:
    lines = [
        f"{'measurement':<46} {'n':>4} {'median ms':>11} {'p95 ms':>11} {'max ms':>11}  cut/failed"
    ]
    for key, m in measurements.items():
        if not m.get("n"):
            lines.append(f"{key:<46} {0:>4}")
            continue
        bad = (
            f"  {m.get('shed', 0)}/{m.get('errors', 0)}"
            if m.get("shed") or m.get("errors")
            else ""
        )
        lines.append(
            f"{key:<46} {m['n']:>4} {m['median']:>11.1f} {m['p95']:>11.1f} {m['max']:>11.1f}{bad}"
        )
    return "\n".join(lines)


def compare(old: dict, new: dict) -> str:
    lines = [
        f"{'measurement':<46} {'old median':>11} {'new median':>11} {'change':>8} {'old p95':>10} {'new p95':>10}"
    ]
    for key, m in new["measurements"].items():
        o = old.get("measurements", {}).get(key)
        if not o or not o.get("n") or not m.get("n"):
            lines.append(f"{key:<46} {'—':>11} {m.get('median', '—')!s:>11}")
            continue
        change = (
            (m["median"] - o["median"]) / o["median"] * 100
            if o["median"]
            else float("nan")
        )
        lines.append(
            f"{key:<46} {o['median']:>11.1f} {m['median']:>11.1f} {change:>+7.0f}% {o['p95']:>10.1f} {m['p95']:>10.1f}"
        )
    return "\n".join(lines)


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--deployment",
        required=True,
        help="the rung directory holding tessera.toml and .env",
    )
    ap.add_argument("--viewer", help="the viewer URL; defaults to the deployment's")
    ap.add_argument("--session", help="the session URL; defaults to the deployment's")
    ap.add_argument(
        "--ranks",
        help="term ranks, [{term, pairs}]; defaults to the deployment's *-ranks.json",
    )
    ap.add_argument("--view", help="the view to measure; defaults to the first")
    ap.add_argument(
        "--unique-field",
        default="gbifid",
        help="the unique field the filter lookup uses",
    )
    ap.add_argument(
        "--start", action="store_true", help="start a server under a memory cap first"
    )
    ap.add_argument("--binary", help="with --start, the tessera-serve binary")
    ap.add_argument("--cap", default="24G", help="with --start, the scope's MemoryMax")
    ap.add_argument(
        "--swap", default="2G", help="with --start, the scope's MemorySwapMax"
    )
    ap.add_argument(
        "--scratch", help="with --start, where its cache, WAL and deployment copy go"
    )
    ap.add_argument(
        "--keep-serving", action="store_true", help="with --start, leave the server up"
    )
    ap.add_argument("--out", required=True)
    ap.add_argument(
        "--compare", help="an earlier run's JSON to print the change against"
    )
    args = ap.parse_args(argv)

    started = time.time()
    directory = Path(args.deployment).resolve()
    settings = deployment_settings(directory)
    serve = settings["serve"]
    env = dict(os.environ) | read_env_file(directory / ".env")
    cred = env[serve["session_credential_env"]]
    args.viewer = args.viewer or f"http://{serve['viewer']}"
    args.session = args.session or f"http://{serve['session']}"
    if not args.ranks:
        found = sorted(directory.glob("*-ranks.json"))
        if not found:
            ap.error("no *-ranks.json in the deployment; pass --ranks")
        args.ranks = str(found[0])
    bundle = (directory / settings["bundle"]["path"]).resolve()

    served = None
    result: dict = {
        "started_at": datetime.now(timezone.utc).isoformat(timespec="seconds")
    }
    if args.start:
        if not args.binary:
            ap.error("--start needs --binary")
        ports = tuple(port_of(serve[k]) for k in ("viewer", "session", "control"))
        busy = [p for p in ports if not port_free(p)]
        if busy:
            ap.error(
                f"ports {busy} are in use; stop what holds them or attach without --start"
            )
        cap = {"G": 2**30, "M": 2**20}
        served = Deployment(
            directory,
            bundle,
            Path(args.scratch) if args.scratch else directory / "interactive-bench",
            ports,
            Path(args.binary),
            cap_bytes=int(args.cap[:-1]) * cap[args.cap[-1]],
            swap_bytes=int(args.swap[:-1]) * cap[args.swap[-1]],
        )
        served.clear_scratch()
        t0 = time.time()
        served.start()
        result["server"] = {
            "started": True,
            "open_s": round(time.time() - t0, 1),
            "pid": served.pid,
            "cap": args.cap,
            "swap": args.swap,
            "memory_at_ready": process_memory(served.pid),
            "scratch": str(served.scratch),
        }
        log(
            f"served pid={served.pid} open {result['server']['open_s']} s, {result['server']['memory_at_ready']}"
        )
        pid = served.pid
    else:
        pid = listening_pid(port_of(args.viewer.rsplit("/", 1)[-1]))
        result["server"] = {
            "started": False,
            "pid": pid,
            "memory_max_bytes": memory_max(pid),
            "memory_before": process_memory(pid),
        }

    binary = args.binary
    if not binary and pid:
        try:
            binary = os.readlink(f"/proc/{pid}/exe")
        except OSError:
            binary = None
    current = json.loads((bundle / "CURRENT").read_text())
    result.update(
        binary=binary,
        bundle=str(bundle),
        manifest_digest=current.get("manifest_digest"),
        box=box(),
        viewer=args.viewer,
        settings={
            "screen": SCREEN,
            "budget": BUDGET,
            "piece_tiles": PIECE_TILES,
            "lookups": LOOKUPS,
            "seed": SEED,
        },
    )
    bench = Bench(args, cred)
    try:
        result.update(bench.run())
    finally:
        result["server"]["memory_after"] = process_memory(pid)
        if served is not None and not args.keep_serving:
            served.stop()
            result["server"]["stopped"] = True
    result["ran_s"] = round(time.time() - started, 1)
    result["measurements"] = bench.measurements
    result["detail"] = bench.detail
    Path(args.out).write_text(json.dumps(result, indent=1, default=str))
    print(table(bench.measurements))
    print(f"ran {result['ran_s']} s; wrote {args.out}")
    if served is not None and args.keep_serving:
        print(
            f"left serving: pid {served.pid} (systemd-run pid {served.proc.pid}); stop it by pid"
        )
    if args.compare:
        print()
        print(compare(json.loads(Path(args.compare).read_text()), result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
