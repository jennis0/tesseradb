"""How interactive a served bundle is: opening a session, moving the map, and looking items up.

A performance benchmark, not a correctness test. It sends a fixed list of requests and writes
figures that compare between runs:

* **Session.** For principals seeing about 1%, 7%, 85% and 100% of the corpus (`--targets`):
  authorise, then the time from the token to the whole extent's first counts, first points, last
  byte of points, last byte of the view's artifacts (`layers`), the last byte of a points request
  that ran beside an artifacts request (`points_beside_layers`), and the last byte of everything
  the view asked for (`settled`), twice: once for a principal new to the server and once more for
  the same principal. An older core's idle promotion of a whole level is not in `settled`. Each
  artifacts request asking for a whole level (a promotion, or every tile of a depth more than two
  below the map zoom) is counted in `whole_level_requests`.
* **Reopen** (`--reopen`, with `--start`). The server restarted over the same cache, with the
  bundle's pages evicted again, and each principal opening once more.
* **Map.** For each principal, a fixed camera script from the whole extent the session opened on:
  into its own two densest regions down to zoom 14, with pans, and back out after each.
* **Lookups.** For the narrowest and the broadest principal: an item card by `mosaica_id`, and a
  filter `eq` on a unique field answering one item, for 200 items the map served.

The session and map sections run the TypeScript core's own store, driver and replica in Node
(`interactive_bench.mjs`), configured as the demo viewer configures them, with a camera script in
place of a person. Each request is timed on the wire in a worker thread, so the bench's own
decoding is not in the waits. Each step waits for the store to go quiet before the next move, so
the requests a step sends depend on the data and not on timing. For the same reason the viewer's
background prefetch is off (its `?prefetch=0`, and in a newer core the artifact channel's prefetch
of neighbouring and parent tiles with it) unless `--prefetch` is given, since a move cancels it
part-way, and revalidation never falls due, since it runs on a sixty-second clock.

With `--prefetch`, `--reference` names a run of the same script with prefetch off. A request
beyond what that run sent at the same step is the prefetch's work (`extra`): it is kept out of
the step's points and layers, timed as `extra`, and counted, and `settled` still includes it.

With `--replay`, the session and map sections send an earlier run's requests again in place of
the store: the same bodies and headers, step by step, each session authorised with the same
terms. Two bundles of one corpus, such as one built at several shard counts, serve slightly
different points, so the store asks each for different things next; a replay asks each for the
same. A request is sent as long after its step began as it was in the reference, and later by as
much as any response part that had arrived before it there (headers, first counts, first points,
last byte) arrives later here, so what ran together runs together and what waited still waits.
A step then waits for quiet as a camera step does. The figures are named and computed as in a
normal run; `points_beside_layers` times the points requests that ran beside an artifacts request
in the reference. Ids the server issued cannot be sent verbatim: artifact ids are mapped by
layer, level and key, and lookups by the unique field's value, both found before the measured
run, which then starts again from an empty cache; a request naming an id with no match here is
left out and counted under `replay`. Lookups send the reference's filter values. A run records
what a replay of it needs, so any run can be replayed.

The core is `clients/ts/core` of this checkout unless `--core` names another build of it, so an
older server is measured with the client of its own commit. A newer core draws layers only with a
number of artifacts per tile (`--per-tile`). The run reads `/control/status` after each phase and
keeps its `masked_count_cache` figures, and keeps the server's memory and the bytes its cache
directory holds at each start and after each phase.

Every run sends the same requests: the principals' term sets are fixed, the 100% principal holds
every term, and the camera script is fixed per principal. `--compare` diffs two runs' request logs
as well as their figures. A principal is new to the server only on a server this run started
(`--start`), which begins with an empty cache and the bundle's pages evicted; attached to a running
server, the first opening is whatever the server has already seen, and the result says which.

    python3 -m test_corpora.common.interactive_bench \\
        --deployment data/ladder/gbif --out run.json [--compare old.json] \\
        [--start --binary /path/to/mosaica [--keep-serving] [--reopen]] \\
        [--core /path/to/clients/ts/core/dist/index.js] [--targets 0.01,0.25,1] [--per-tile 50] \\
        [--prefetch [--reference prefetch-off-run.json] | --replay reference-run.json]

Build the TypeScript core first: `npm --prefix clients/ts run build -w @mosaicajs/client`.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
import os
import platform
import socket
import subprocess
import sys
import time
from collections import Counter
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path

import requests
from pyarrow import ipc

from .deployment import Deployment, read_env_file, tomllib
from .serve_battery import FRAME_TRAILER, Evictor, compose_ladder, frames

REPO = Path(__file__).resolve().parents[2]
NODE_SCRIPT = Path(__file__).with_suffix(".mjs")
CORE = REPO / "clients/ts/core/dist/index.js"
ARROW = REPO / "clients/ts/node_modules/apache-arrow/Arrow.node.mjs"

SCREEN = (1600, 900)
#: The demo viewer's mark budget, `DEFAULT_BUDGET` in viewer/src/main.ts.
BUDGET = 500_000
TARGETS = (0.01, 0.07, 0.85, 1.0)
#: Each principal's map regions: its densest cells, in distinct depth-3 ancestors.
REGIONS = 2
ZOOMS = (3, 6, 9, 12, 14)
#: Where the script pans: half a screen east, then (at the deepest) south too.
PANS = {6: ["east"], 9: ["east"], 14: ["east", "south"]}
#: How long nothing may be in flight before a step counts as settled. Longer than every timer
#: the store runs while still (the artifact channel's idle promotion is 1.5 s), so each fires
#: inside the step it belongs to, and two runs send the same requests in the same steps.
QUIET_MS = 2000
STEP_TIMEOUT_MS = 600_000
LOOKUPS = 200
FRAME_RECORDS = 7

BANDS = (
    (0, 0, "z0"),
    (1, 4, "z1-4"),
    (5, 8, "z5-8"),
    (9, 11, "z9-11"),
    (12, 16, "z12-14"),
)


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def stats(values: Sequence[float]) -> dict:
    v = sorted(x for x in values if x is not None)
    if not v:
        return {"n": 0}

    def rank(p: float) -> float:
        return v[min(len(v) - 1, max(0, math.ceil(p / 100 * len(v)) - 1))]

    return {
        "n": len(v),
        "median": round(rank(50), 1),
        "p95": round(rank(95), 1),
        "max": round(v[-1], 1),
    }


# ---------------------------------------------------------------------------------------------
# Principals and the plan the Node side runs
# ---------------------------------------------------------------------------------------------


def principals(ranks: list[dict], targets: Sequence[float] = TARGETS) -> list[dict]:
    """The principals: the 100% principal holds every term, the others a fixed greedy set."""
    total = sum(r["pairs"] for r in ranks)
    out = []
    for entry in compose_ladder(ranks, total, targets):
        label = f"{round(entry['target'] * 100)}%"
        out.append(
            {
                "label": label,
                "terms": sorted(entry["terms"]),
                "map": True,
                "ids": entry["target"] in (targets[0], targets[-1]),
            }
        )
    return out


def box_at(q: dict, centre: Sequence[float], z: float, screen: tuple[int, int] = SCREEN) -> list[float]:
    """The data-space box a screen shows at map zoom `z` around `centre`, as the Node side's
    `boxAt`: zoom 0 fits the extent's height. `q` is a view's `quantisation` from `/v1/meta`."""
    w, h = screen
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


def size_bytes(text: str) -> int:
    """`24G` or `512M` as bytes, as `systemd-run`'s memory properties take them."""
    unit = {"G": 2**30, "M": 2**20}
    if not text or text[-1] not in unit or not text[:-1].isdigit():
        raise ValueError(f"{text!r} is not a size; write a whole number of G or M, such as 24G")
    return int(text[:-1]) * unit[text[-1]]


def evict(*roots: Path) -> None:
    """Drop the pages of every file under `roots`, so the next reads come from disk."""
    Evictor(list(roots), None, None).evict()


def reference_keys(path: str | None) -> list | None:
    """A reference run's requests, counted by what they sent, for the Node side to match."""
    if not path:
        return None
    counts: dict[str, int] = {}
    for r in json.loads(Path(path).read_text()).get("requests", []):
        key = json.dumps(list(sent(r)), separators=(",", ":"), ensure_ascii=False)
        counts[key] = counts.get(key, 0) + 1
    return sorted(counts.items())


def write_plan(
    path: Path, args, people: list[dict], phase: str = "run", replay: dict | None = None
) -> None:
    core = Path(args.core).resolve()
    plan = {
        "core": core.as_uri(),
        # Beside the core's own workspace: `clients/ts/core/dist/index.js` → `clients/ts/node_modules`.
        "arrow": (core.parents[2] / ARROW.relative_to(ARROW.parents[2])).as_uri(),
        "viewer": args.viewer,
        "session": args.session,
        "screen": SCREEN,
        "budget": BUDGET,
        "title_field": args.title_field,
        "principals": people,
        "regions": REGIONS,
        "zooms": ZOOMS,
        "pans": {str(k): v for k, v in PANS.items()},
        "quiet_ms": QUIET_MS,
        "step_timeout_ms": STEP_TIMEOUT_MS,
        "lookups": LOOKUPS,
        "per_tile": args.per_tile,
        "prefetch": args.prefetch,
        "phase": phase,
        "reference": reference_keys(args.reference),
        "replay": replay,
    }
    path.write_text(json.dumps(plan))


# ---------------------------------------------------------------------------------------------
# Replay: a reference run's requests, sent again to another bundle of the same corpus
# ---------------------------------------------------------------------------------------------

#: The parts of a response a replayed request can wait for, as the Node side names them.
PARTS = ("headers", "counts", "points", "last_byte")


def authorise(session: str, credential: str, terms: Sequence[str]) -> str:
    r = requests.post(
        f"{session}/session/authorise",
        json={"terms": list(terms)},
        headers={"Authorization": f"Bearer {credential}"},
        timeout=120,
    )
    r.raise_for_status()
    return r.json()["token"]


def read_records(viewer: str, token: str, path: str, body: dict) -> list[dict]:
    """Every row of a records read (`/v1/items`, `/v1/artifacts`), following `next` to the end,
    waiting out the bulk admission limit."""
    rows: list[dict] = []
    cursor = None
    while True:
        sent = {**body, "cursor": cursor} if cursor else body
        for _ in range(60):
            r = requests.post(
                f"{viewer}{path}",
                json=sent,
                headers={"Authorization": f"Bearer {token}"},
                timeout=600,
            )
            if r.status_code != 429:
                break
            time.sleep(float(r.headers.get("retry-after") or 1))
        r.raise_for_status()
        trailer: dict = {}
        for kind, payload in frames(r.content):
            if kind == FRAME_RECORDS:
                rows += ipc.open_stream(io.BytesIO(payload)).read_all().to_pylist()
            elif kind == FRAME_TRAILER:
                trailer = json.loads(payload)
        cursor = trailer.get("next")
        if not cursor or body.get("pages"):
            return rows


def artifact_keys(viewer: str, token: str, log_: list[dict]) -> dict:
    """The level and key of every artifact a `/v1/artifacts` request named by `mosaica_id`, by
    layer, so a replay can name the same artifacts in another bundle."""
    asked: dict[tuple[str, str], set[str]] = {}
    for r in log_:
        if r["path"] == "/v1/artifacts" and r.get("body"):
            b = json.loads(r["body"])
            if b.get("ids"):
                asked.setdefault((b["view"], b["layer"]), set()).update(str(i) for i in b["ids"])
    out: dict[str, dict] = {}
    for (view, layer), ids in sorted(asked.items()):
        ids_ = sorted(ids)
        for at in range(0, len(ids_), 1000):
            body = {
                "view": view,
                "layer": layer,
                "ids": ids_[at : at + 1000],
                "fields": ["key", "level"],
            }
            for row in read_records(viewer, token, "/v1/artifacts", body):
                out.setdefault(layer, {})[str(row["mosaica_id"])] = [row["level"], row["key"]]
    return out


def artifacts_here(viewer: str, token: str, reference: dict) -> dict:
    """This bundle's `mosaica_id` for each layer, level and key the reference's artifact ids
    name, where exactly one artifact carries it."""
    keys = reference["artifact_keys"]
    view = next(
        json.loads(r["body"])["view"]
        for r in reference["requests"]
        if r["path"] == "/v1/artifacts" and r.get("body")
    ) if keys else None
    found: dict[tuple, list[str]] = {}
    for layer, level in sorted({(layer, lk[0]) for layer, m in keys.items() for lk in m.values()}):
        body = {"view": view, "layer": layer, "level": level, "fields": ["key", "level"]}
        for row in read_records(viewer, token, "/v1/artifacts", body):
            if row["key"] is not None:
                key = (layer, row["level"], row["key"])
                found.setdefault(key, []).append(str(row["mosaica_id"]))
    return {k: v[0] for k, v in found.items() if len(v) == 1}


def lookups_here(
    viewer: str, token: str, view_id: str, field: str, cards: list
) -> list[str | None]:
    """This bundle's `mosaica_id` for each item the reference looked up, by its unique field's
    value: `None` where the reference's card failed or the value answers other than one item."""
    out: list[str | None] = []
    for _, value, ok in cards:
        if not ok or value is None:
            out.append(None)
            continue
        body = {
            "view": view_id,
            "fields": [field],
            "filters": {field: {"eq": str(value)}},
            "page_rows": 2,
            "pages": 1,
        }
        rows = read_records(viewer, token, "/v1/items", body)
        out.append(str(rows[0]["mosaica_id"]) if len(rows) == 1 else None)
    return out


def replayed_body(r: dict, keys: dict, here: dict) -> str | None:
    """The body to send again: the reference's, with each artifact id it names replaced by this
    bundle's id for the same layer, level and key, or `None` where one has no such id."""
    if r["path"] != "/v1/artifacts" or not r.get("body"):
        return r.get("body")
    b = json.loads(r["body"])
    if not b.get("ids"):
        return r["body"]
    ids = []
    for i in b["ids"]:
        level_key = keys.get(b["layer"], {}).get(str(i))
        mapped = here.get((b["layer"], *level_key)) if level_key else None
        if mapped is None:
            return None
        ids.append(mapped if isinstance(i, str) else int(mapped))
    b["ids"] = ids
    return json.dumps(b, separators=(",", ":"), ensure_ascii=False)


def replay_plan(reference: dict, phase: str, here: dict) -> tuple[dict, list[dict]]:
    """The Node side's replay of one phase of `reference`, step by step, and the reference
    requests left out of it."""
    in_phase = [
        r for r in reference["requests"] if (r.get("phase") == "reopen") == (phase == "reopen")
    ]
    by_step: dict[str, list[dict]] = {}
    for r in in_phase:
        by_step.setdefault(r["step"], []).append(r)
    left_out: list[dict] = []
    used: set[str] = set()

    def step(spec: dict, origin: float, **extra) -> dict:
        used.add(spec["id"])
        planned = []
        for r in by_step.get(spec["id"], []):
            body = replayed_body(r, reference["artifact_keys"], here)
            if body is None and r.get("body") is not None:
                left_out.append({**r, "why": "an artifact id with no layer, level and key here"})
                continue
            offset = r["at_ms"] - origin
            planned.append(
                {
                    "ref_id": r["id"],
                    "method": r["method"],
                    "path": r["path"],
                    "body": body,
                    "headers": r["headers"],
                    "offset": offset,
                    "events": {
                        part: None if r.get(f"{part}_ms") is None else offset + r[f"{part}_ms"]
                        for part in PARTS
                    },
                    "extra": bool(r.get("extra")),
                    "beside_layers": r.get("beside_layers"),
                    "abort_after_ms": r["last_byte_ms"] if r.get("error") == "aborted" else None,
                }
            )
        return {"spec": spec, "requests": planned, **extra}

    principals = []
    for p in reference["principals"]:
        label = p["label"]
        detail = reference["detail"][label]
        steps = []
        for which in ("reopen",) if phase == "reopen" else ("first", "again"):
            o = detail[which]
            spec = {"id": f"{label}/{which}", "kind": "open", "region": "world", "zoom": 0}
            steps.append(
                step(
                    spec,
                    o["start_at_ms"],
                    open=which,
                    token_off=o["token_at_ms"] - o["start_at_ms"],
                    show_off=o["show_at_ms"] - o["start_at_ms"],
                )
            )
        if phase != "reopen":
            for m in detail.get("map") or []:
                spec = {k: m[k] for k in ("id", "kind", "region", "zoom", "centre")}
                steps.append(step(spec, m["t0_at_ms"], open=None, session="again"))
        principals.append(
            {
                "label": label,
                "terms": p["terms"],
                "densest": phase != "reopen",
                "lookups": phase != "reopen" and label in reference["lookups"],
                "steps": steps,
            }
        )
    left_out += [
        {**r, "why": "outside every step"} for r in in_phase if r["step"] not in used
    ]
    return {"principals": principals}, left_out


def replayed(reference: dict) -> str | None:
    """Why `reference` cannot be replayed, or `None`."""
    if "artifact_keys" not in reference or any("at_ms" not in r for r in reference["requests"]):
        return (
            "the reference run predates replay: it records no request start times or artifact "
            "keys; run it again with this bench and replay that"
        )
    return None


def replay_summary(reference: dict, result: dict, left_out: list[dict]) -> dict:
    """What a replay sent of its reference: how many requests, which were left out and why, how
    many went with another body (artifact ids of this bundle) or got another status, and the
    principals whose densest regions here are not the reference's."""
    sent = {(r.get("phase"), r["ref_id"]): r for r in result["requests"] if "ref_id" in r}
    pairs = [
        (r, sent[(r.get("phase"), r["id"])])
        for r in reference["requests"]
        if (r.get("phase"), r["id"]) in sent
    ]

    def count(rows) -> dict:
        return dict(sorted(Counter(rows).items()))

    return {
        "reference_requests": len(reference["requests"]),
        "sent": len(sent),
        "left_out": count(r["kind"] for r in left_out),
        "left_out_why": count(r["why"] for r in left_out),
        "bodies_changed": sum(1 for r, s in pairs if r.get("body") != s.get("body")),
        "status_differs": count(
            f"{r['kind']} {r.get('status')}->{s.get('status')}"
            for r, s in pairs
            if r.get("status") != s.get("status")
        ),
        "regions_differ": [
            p["label"]
            for p, q in zip(reference["principals"], result["principals"])
            if p.get("regions") != q.get("regions")
        ],
    }


# ---------------------------------------------------------------------------------------------
# Figures
# ---------------------------------------------------------------------------------------------


class Figures:
    def __init__(self) -> None:
        self.measurements: dict[str, dict] = {}

    def put(
        self, key: str, values: Sequence[float], shed: int = 0, errors: int = 0
    ) -> None:
        m = {**stats(values), "shed": shed, "errors": errors}
        if not m["n"] and not shed and not errors:
            return
        self.measurements[key] = m
        if m["n"]:
            log(
                f"  {key:<46} n={m['n']:<4} median {m['median']:>9.1f} ms  p95 {m['p95']:>9.1f}  max {m['max']:>9.1f}"
            )

    def session(self, entry: dict, opens: Sequence[str] = ("first", "again")) -> None:
        label = entry["label"]
        for which in opens:
            o = entry[which]
            bad = {"shed": o["shed"], "errors": o["errors"] + int(o["timed_out"])}
            self.put(f"session.{label}.authorise.{which}", [o["authorise_ms"]])
            self.put(f"session.{label}.counts.{which}", [o["counts_ms"]], **bad)
            self.put(
                f"session.{label}.first_points.{which}", [o["first_points_ms"]], **bad
            )
            self.put(f"session.{label}.last_byte.{which}", [o["last_byte_ms"]], **bad)
            self.put(f"session.{label}.settled.{which}", [o["settled_ms"]], **bad)
            self.put(f"session.{label}.layers.{which}", [o.get("layers_ms")], **bad)
            self.put(
                f"session.{label}.points_beside_layers.{which}",
                [o.get("points_beside_layers_ms")],
            )
            self.put(f"session.{label}.extra.{which}", [o.get("extra_ms")])

    def map(self, entry: dict) -> None:
        label = entry["label"]
        steps = [s for s in entry.get("map", []) if s["requests"] > 0]
        for lo, hi, band in BANDS:
            for kind in ("zoom-in", "pan", "zoom-out"):
                chosen = [
                    s for s in steps if lo <= s["zoom"] <= hi and s["kind"] == kind
                ]
                if not chosen:
                    continue
                bad = {
                    "shed": sum(s["shed"] for s in chosen),
                    "errors": sum(s["errors"] + int(s["timed_out"]) for s in chosen),
                }
                name = kind.replace("-", "_")
                for field in (
                    "counts_ms",
                    "first_points_ms",
                    "last_byte_ms",
                    "layers_ms",
                    "points_beside_layers_ms",
                ):
                    self.put(
                        f"map.{label}.{band}.{name}.{field.removesuffix('_ms')}",
                        [s.get(field) for s in chosen],
                        **bad,
                    )


#: The routes whose requests are timed by kind: the points viewport and the artifacts viewport.
TIMED_ROUTES = ("/v1/viewport", "/v1/artifacts/viewport")


def request_figures(figures: Figures, log_: list[dict]) -> None:
    """Per request kind over the whole run: the wire time to the last byte."""
    timed = [r for r in log_ if r["path"] in TIMED_ROUTES]
    for kind in sorted({r["kind"] for r in timed}):
        chosen = [r for r in timed if r["kind"] == kind]
        figures.put(
            f"requests.{kind}.last_byte",
            [r["last_byte_ms"] for r in chosen],
            shed=sum(1 for r in chosen if r.get("shed")),
            errors=sum(
                1 for r in chosen if r.get("error") or (r.get("status") or 0) >= 400
            ),
        )


# ---------------------------------------------------------------------------------------------
# Lookups
# ---------------------------------------------------------------------------------------------


def lookups(
    figures: Figures,
    viewer: str,
    label: str,
    token: str,
    ids: Sequence[str],
    view_id: str,
    field: str,
    values: Sequence | None = None,
) -> dict:
    """An item card for each of `ids`, then a filter on `field` for each card's value; a replay
    passes the reference's `values`, so its filters are the reference's whatever its cards say."""
    http = requests.Session()
    http.headers["Authorization"] = f"Bearer {token}"
    card_ms, filter_ms, rows = [], [], []
    card_failed = filter_failed = 0
    cards = []
    for tid in ids:
        t0 = time.perf_counter()
        r = http.post(f"{viewer}/v1/items/{tid}", json={}, timeout=120)
        ms = (time.perf_counter() - t0) * 1000.0
        if r.status_code != 200:
            card_failed += 1
            cards.append([tid, None, False])
            continue
        card_ms.append(ms)
        cards.append([tid, r.json()["fields"].get(field), True])
    if values is None:
        values = [value for _, value, ok in cards if ok]
    for value in values:
        if value is None:
            filter_failed += 1
            continue
        body = {
            "view": view_id,
            "fields": [field],
            "filters": {field: {"eq": str(value)}},
            "page_rows": 1,
            "pages": 1,
        }
        t0 = time.perf_counter()
        r = http.post(f"{viewer}/v1/items", json=body, timeout=120)
        content = r.content
        ms = (time.perf_counter() - t0) * 1000.0
        if r.status_code != 200:
            filter_failed += 1
            continue
        filter_ms.append(ms)
        rows.append(
            sum(
                ipc.open_stream(io.BytesIO(p)).read_all().num_rows
                for kind, p in frames(content)
                if kind == FRAME_RECORDS
            )
        )
    figures.put(f"lookup.{label}.item_card", card_ms, errors=card_failed)
    figures.put(
        f"lookup.{label}.filter_eq_{field}", filter_ms, errors=filter_failed
    )
    return {
        "asked": len(ids),
        "filter_rows": {str(n): rows.count(n) for n in sorted(set(rows))},
        "field": field,
        # Each card asked for: its `mosaica_id`, the field's value, and whether it answered.
        "cards": cards,
        "values": list(values),
    }


def volumes(node: dict, phase: str) -> dict:
    """Bytes received, per open and per map band and move, and the prefetch's and whole-level
    requests by kind."""
    out: dict = {}
    for e in node["principals"]:
        for which in ("first", "again", "reopen"):
            if which in e:
                out[f"session.{e['label']}.{which}"] = e[which].get("bytes", 0)
        for lo, hi, band in BANDS:
            for kind in ("zoom-in", "pan", "zoom-out"):
                chosen = [
                    s
                    for s in e.get("map", [])
                    if lo <= s["zoom"] <= hi and s["kind"] == kind and s["requests"]
                ]
                if chosen:
                    out[f"map.{e['label']}.{band}.{kind.replace('-', '_')}"] = sum(
                        s.get("bytes", 0) for s in chosen
                    )
    requests_ = node["requests"]
    return {
        "phase": phase,
        "bytes": out,
        "bytes_by_kind": {
            k: sum(r.get("bytes", 0) for r in requests_ if r["kind"] == k)
            for k in sorted({r["kind"] for r in requests_})
        },
        "requests_by_kind": {
            k: sum(1 for r in requests_ if r["kind"] == k)
            for k in sorted({r["kind"] for r in requests_})
        },
        "extra_requests_by_kind": {
            k: sum(1 for r in requests_ if r["kind"] == k and r.get("extra"))
            for k in sorted({r["kind"] for r in requests_ if r.get("extra")})
        },
        "whole_level_requests": sum(1 for r in requests_ if r.get("whole_level")),
    }


def control_status(control: str, credential: str) -> dict:
    """`/control/status`'s figures counters (`masked_count_cache`), or the failure."""
    try:
        r = requests.get(
            f"{control}/control/status",
            headers={"Authorization": f"Bearer {credential}"},
            timeout=60,
        )
        r.raise_for_status()
        return r.json().get("masked_count_cache", {})
    except requests.RequestException as e:
        return {"error": str(e)}


def check_unique_field(viewer: str, token: str, field: str) -> str:
    """The view to look items up in, after checking `field` is a declared unique column."""
    r = requests.get(
        f"{viewer}/v1/meta", headers={"Authorization": f"Bearer {token}"}, timeout=120
    )
    r.raise_for_status()
    meta = r.json()
    names = {c["name"] for c in meta.get("declared_scalars", [])}
    if field not in names:
        raise SystemExit(
            f"--unique-field {field!r} is not a declared column of this bundle; name one of its "
            f"unique columns ({', '.join(sorted(names))})"
        )
    return meta["views"][0]["id"]


# ---------------------------------------------------------------------------------------------
# The server, the box, and the comparison
# ---------------------------------------------------------------------------------------------


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
    if "pid=" not in out:
        return None
    return int(out.split("pid=", 1)[1].split(",", 1)[0])


def process_memory(pid: int | None) -> dict:
    out: dict = {}
    if not pid:
        return out
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(("VmRSS:", "VmHWM:", "RssAnon:", "RssFile:")):
                key, value = line.split(":", 1)
                out[key.lower() + "_gib"] = round(int(value.split()[0]) / 2**20, 2)
        relative = Path(f"/proc/{pid}/cgroup").read_text().strip().split("::", 1)[-1]
        out["memory_max"] = (
            (Path("/sys/fs/cgroup") / relative.lstrip("/") / "memory.max")
            .read_text()
            .strip()
        )
    except OSError:
        pass
    return out


def directory_bytes(path: Path | None) -> int | None:
    """The bytes of the files under `path`: what a started server has written to its cache."""
    if path is None or not path.is_dir():
        return None
    return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())


def binary_identity(path: str | None) -> dict:
    if not path:
        return {}
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    version = subprocess.run(
        [path, "--version"], capture_output=True, text=True, check=False
    ).stdout.strip()
    return {"path": path, "sha256": digest.hexdigest(), "version": version}


def box() -> dict:
    lines = Path("/proc/cpuinfo").read_text().splitlines()
    cpu = next(
        (x.split(":", 1)[1].strip() for x in lines if x.startswith("model name")), ""
    )
    mem = Path("/proc/meminfo").read_text().splitlines()
    mem_kb = int(next(x for x in mem if x.startswith("MemTotal")).split()[1])
    return {
        "host": socket.gethostname(),
        "cpu": cpu,
        "cpus": os.cpu_count(),
        "memory_gib": round(mem_kb / 2**20, 1),
        "kernel": platform.release(),
    }


def bad_of(m: dict) -> str:
    return (
        f"{m.get('shed', 0)}/{m.get('errors', 0)}"
        if m.get("shed") or m.get("errors")
        else ""
    )


def table(measurements: dict) -> str:
    head = f"{'measurement':<50} {'n':>4} {'median ms':>10} {'p95 ms':>10} {'max ms':>10}  shed/failed"
    lines = [head]
    for key, m in measurements.items():
        if not m.get("n"):
            lines.append(f"{key:<50} {0:>4}  {bad_of(m)}")
            continue
        lines.append(
            f"{key:<50} {m['n']:>4} {m['median']:>10.1f} {m['p95']:>10.1f} {m['max']:>10.1f}  {bad_of(m)}"
        )
    return "\n".join(lines)


def compare(old: dict, new: dict) -> str:
    """The change in each median and p95, with n and the shed and failed counts on both sides,
    every key either run has, then whether the two runs sent the same requests."""
    lines = [
        f"{'measurement':<50} {'old n/median':>14} {'new n/median':>14} {'change':>8} {'old p95':>9} {'new p95':>9}  old/new shed/failed"
    ]
    o_all, n_all = old.get("measurements", {}), new.get("measurements", {})
    for key in list(n_all) + [k for k in o_all if k not in n_all]:
        o, m = o_all.get(key, {}), n_all.get(key, {})

        def cell(x: dict) -> str:
            return f"{x['n']}/{x['median']:.1f}" if x.get("n") else "—"

        if o.get("n") and m.get("n"):
            change = (
                f"{(m['median'] - o['median']) / o['median'] * 100:+.0f}%"
                if o["median"]
                else ("same" if m["median"] == 0 else "from 0")
            )
            p95 = f"{o['p95']:>9.1f} {m['p95']:>9.1f}"
        else:
            change = "only old" if o and not m else "only new" if m and not o else "—"
            p95 = f"{'':>9} {'':>9}"
        lines.append(
            f"{key:<50} {cell(o):>14} {cell(m):>14} {change:>8} {p95}  {bad_of(o) or '-'} / {bad_of(m) or '-'}"
        )
    lines.append("")
    lines.append(diff_requests(old.get("requests", []), new.get("requests", [])))
    return "\n".join(lines)


def sent(r: dict) -> tuple:
    """What a request sent, without the per-session parts: its step, route and body."""
    body = r.get("body")
    if r["path"] == "/session/authorise":
        body = "<body>" if body else body
    return (r.get("step"), r["method"], r["path"], body)


def diff_requests(old: list[dict], new: list[dict]) -> str:
    if not old:
        return "requests: the old run has no request log"
    a = sorted(sent(r) for r in old)
    b = sorted(sent(r) for r in new)
    if a == b:
        return f"requests: identical ({len(a)} requests)"
    only_old = [x for x in a if x not in set(b)]
    only_new = [x for x in b if x not in set(a)]
    out = [
        f"requests: DIFFER: {len(a)} old, {len(b)} new; {len(only_old)} only in old, {len(only_new)} only in new"
    ]
    for tag, rows in (("old", only_old), ("new", only_new)):
        for row in rows[:5]:
            out.append(f"  only {tag}: {row[0]} {row[2]} {str(row[3])[:160]}")
    return "\n".join(out)


# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--deployment",
        required=True,
        help="the rung directory holding mosaica.toml and .env",
    )
    ap.add_argument("--viewer", help="the viewer URL; defaults to the deployment's")
    ap.add_argument("--session", help="the session URL; defaults to the deployment's")
    ap.add_argument(
        "--ranks",
        help="term ranks, [{term, pairs}]; defaults to the deployment's *-ranks.json",
    )
    ap.add_argument(
        "--unique-field",
        default="gbifid",
        help="the unique column the filter lookup uses",
    )
    ap.add_argument(
        "--title-field",
        help="the demo's `titleField` for this dataset, if it names one",
    )
    ap.add_argument(
        "--start",
        action="store_true",
        help="start a server, with an empty cache, first",
    )
    ap.add_argument("--binary", help="with --start, the mosaica binary")
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
    ap.add_argument(
        "--reopen",
        action="store_true",
        help="with --start, then restart the server over the same cache and open each principal again",
    )
    ap.add_argument(
        "--core",
        default=str(CORE),
        help="the TypeScript core's built index.js, for a server of another commit",
    )
    ap.add_argument(
        "--targets",
        default=",".join(str(t) for t in TARGETS),
        help="the principals' shares of the corpus, comma-separated, ascending, the last 1",
    )
    ap.add_argument(
        "--per-tile",
        type=int,
        default=50,
        help="the store's artifacts per level per tile (createStore's artifacts.perTile)",
    )
    ap.add_argument(
        "--prefetch",
        action="store_true",
        help="leave the store's idle prefetch on, as the demo viewer does without ?prefetch=0",
    )
    ap.add_argument(
        "--reference",
        help="with --prefetch, a run of the same script with prefetch off: what this run sent "
        "beyond it is the prefetch's work, kept out of points and layers",
    )
    ap.add_argument(
        "--replay",
        help="a run's JSON whose requests to send again, step by step, in place of the camera "
        "script; its principals, store settings and lookups are used, and --ranks, --targets, "
        "--per-tile and --unique-field are not",
    )
    ap.add_argument("--out", required=True)
    ap.add_argument(
        "--compare", help="an earlier run's JSON to compare figures and requests with"
    )
    args = ap.parse_args(argv)

    if not Path(args.core).is_file():
        ap.error(
            f"{args.core} is missing; build it with `npm --prefix clients/ts run build -w @mosaicajs/client`"
        )
    if args.reopen and not args.start:
        ap.error("--reopen needs --start, since it restarts the server")
    reference = None
    if args.replay:
        if args.reference or args.prefetch:
            ap.error(
                "--replay sends the reference's requests as they were sent; drop --prefetch and --reference"
            )
        reference = json.loads(Path(args.replay).read_text())
        why = replayed(reference)
        if why:
            ap.error(f"--replay {args.replay}: {why}")
        if args.reopen and not any(r.get("phase") == "reopen" for r in reference["requests"]):
            ap.error(
                f"--replay {args.replay} has no reopen phase; drop --reopen or replay a run made with it"
            )
        # The settings the reference's requests were made under.
        args.per_tile = reference["settings"]["per_tile"]
        args.prefetch = reference["settings"]["prefetch"]
        args.unique_field = reference["settings"]["unique_field"]
    targets = tuple(float(t) for t in args.targets.split(","))
    started = time.time()
    directory = Path(args.deployment).resolve()
    settings = tomllib.loads((directory / "mosaica.toml").read_text())
    serve = settings["serve"]
    env = dict(os.environ) | read_env_file(directory / ".env")
    cred = env[serve["operator_credential_env"]]
    args.viewer = args.viewer or f"http://{serve['viewer']}"
    args.session = args.session or f"http://{serve['session']}"
    control = f"http://{serve['control']}"
    if not args.ranks and reference is None:
        found = sorted(directory.glob("*-ranks.json"))
        if not found:
            ap.error("no *-ranks.json in the deployment; pass --ranks")
        args.ranks = str(found[0])
    bundle = (directory / settings["bundle"]["path"]).resolve()

    result: dict = {
        "started_at": datetime.now(timezone.utc).isoformat(timespec="seconds")
    }
    served = None
    if args.start:
        if not args.binary:
            ap.error("--start needs --binary")
        ports = tuple(port_of(serve[k]) for k in ("viewer", "session", "control"))
        busy = [p for p in ports if not port_free(p)]
        if busy:
            ap.error(
                f"ports {busy} are in use; stop what holds them or attach without --start"
            )
        served = Deployment(
            directory,
            bundle,
            Path(args.scratch) if args.scratch else directory / "interactive-bench",
            ports,
            Path(args.binary),
            cap_bytes=size_bytes(args.cap),
            swap_bytes=size_bytes(args.swap),
        )
        served.clear_scratch()

    def start_fresh() -> None:
        evict(bundle)
        t0 = time.time()
        served.start()
        result["server"] = {
            "started": True,
            "fresh_cache": True,
            "open_s": round(time.time() - t0, 1),
            "pid": served.pid,
            "memory_at_ready": process_memory(served.pid),
            "cache_bytes_at_ready": directory_bytes(served.cache),
        }
        log(f"served pid={served.pid}, open {result['server']['open_s']} s")

    if reference is None:
        people = principals(json.loads(Path(args.ranks).read_text()), targets)
    else:
        people = [
            {
                "label": p["label"],
                "terms": p["term_list"],
                "map": True,
                "ids": p["label"] in reference["lookups"],
            }
            for p in reference["principals"]
        ]
    broadest = max(people, key=lambda p: len(p["terms"]))["terms"]
    figures = Figures()
    replay_out: dict = {}
    left_out: list[dict] = []
    try:
        if served is not None:
            start_fresh()
        if reference is not None:
            # This bundle's ids for what the reference named by id, found before the measured
            # run, which then starts again from an empty cache with the bundle's pages evicted.
            log("replay: finding this bundle's ids for the reference's artifacts and lookups")
            token = authorise(args.session, cred, broadest)
            here = artifacts_here(args.viewer, token, reference)
            lookup_here: dict[str, list] = {}
            for p in people:
                done = reference["lookups"].get(p["label"])
                if done:
                    lookup_here[p["label"]] = lookups_here(
                        args.viewer,
                        authorise(args.session, cred, p["terms"]),
                        check_unique_field(args.viewer, token, done["field"]),
                        done["field"],
                        done["cards"],
                    )
            if served is not None:
                served.stop()
                served.clear_scratch()
                start_fresh()
            plan, left_out = replay_plan(reference, "run", here)
            named = [
                (layer, *level_key)
                for layer, m in reference["artifact_keys"].items()
                for level_key in m.values()
            ]
            replay_out = {
                "reference": str(Path(args.replay).resolve()),
                "reference_started_at": reference.get("started_at"),
                "reference_bundle": reference.get("bundle"),
                "artifact_ids_named": len(named),
                "artifact_ids_found_here": sum(1 for k in named if k in here),
            }
        if served is None:
            pid = listening_pid(port_of(args.viewer.rsplit("/", 1)[-1]))
            result["server"] = {
                "started": False,
                "fresh_cache": False,
                "pid": pid,
                "memory_before": process_memory(pid),
            }
        else:
            pid = served.pid
        binary = args.binary or (os.readlink(f"/proc/{pid}/exe") if pid else None)
        result.update(
            binary=binary_identity(binary),
            bundle=str(bundle),
            manifest_digest_on_disk=json.loads((bundle / "CURRENT").read_text()).get(
                "manifest_digest"
            ),
            box=box(),
            viewer=args.viewer,
            settings={
                "screen": SCREEN,
                "budget": BUDGET,
                "zooms": ZOOMS,
                "pans": PANS,
                "regions": REGIONS,
                "quiet_ms": QUIET_MS,
                "per_tile": args.per_tile,
                "prefetch": args.prefetch,
                "core": str(Path(args.core).resolve()),
                "unique_field": args.unique_field,
                "replay": replay_out.get("reference"),
            },
        )

        work = Path(args.out).with_suffix(".work")
        work.mkdir(exist_ok=True)
        write_plan(work / "plan.json", args, people, replay=plan if reference else None)
        log("session and map, " + ("replayed" if reference else "in the TypeScript core"))
        node_env = dict(os.environ, MOSAICA_BENCH_SESSION_CRED=cred)
        subprocess.run(
            [
                "node",
                str(NODE_SCRIPT),
                str(work / "plan.json"),
                str(work / "node.json"),
            ],
            env=node_env,
            check=True,
        )
        node = json.loads((work / "node.json").read_text())
        for entry in node["principals"]:
            figures.session(entry)
        for entry in node["principals"]:
            figures.map(entry)
        request_figures(figures, node["requests"])
        result["volumes"] = [volumes(node, "run")]
        result["status"] = {"run": control_status(control, cred)}
        result["pins_served"] = sorted(
            {r["pin"] for r in node["requests"] if r.get("pin")}
        )

        log("lookups")
        result["lookups"] = {}
        with_ids = [e for e in node["principals"] if "token" in e]
        if with_ids:
            view_id = check_unique_field(
                args.viewer, with_ids[0]["token"], args.unique_field
            )
            for entry in with_ids:
                label = entry["label"]
                if reference is None:
                    ids, values = entry["lookup_ids"], None
                else:
                    done = reference["lookups"][label]
                    ids = [i for i in lookup_here[label] if i is not None]
                    values = done["values"]
                    replay_out.setdefault("lookups_left_out", {})[label] = len(
                        done["cards"]
                    ) - len(ids)
                result["lookups"][label] = {
                    "ids_served": entry.get("ids_served"),
                    **lookups(
                        figures,
                        args.viewer,
                        label,
                        entry["token"],
                        ids,
                        view_id,
                        args.unique_field,
                        values,
                    ),
                }
        for entry in node["principals"]:
            entry.pop("token", None)
        result["principals"] = [
            {
                **{k: e.get(k) for k in ("label", "terms", "visible", "regions")},
                "term_list": p["terms"],
            }
            for e, p in zip(node["principals"], people)
        ]
        result["detail"] = {
            e["label"]: {k: e.get(k) for k in ("first", "again", "map")}
            for e in node["principals"]
        }
        result["requests"] = node["requests"]
        result["server"]["memory_after"] = process_memory(pid)
        result["server"]["cache_bytes_after"] = directory_bytes(served.cache if served else None)

        if args.reopen:
            served.stop()
            evict(bundle)
            t0 = time.time()
            served.start()
            result["server"]["reopen_s"] = round(time.time() - t0, 1)
            result["server"]["memory_at_reopen"] = process_memory(served.pid)
            result["server"]["cache_bytes_at_reopen"] = directory_bytes(served.cache)
            log(f"restarted pid={served.pid}, open {result['server']['reopen_s']} s")
            replay = None
            if reference is not None:
                replay, more = replay_plan(reference, "reopen", here)
                left_out += more
            write_plan(work / "reopen.json", args, people, phase="reopen", replay=replay)
            subprocess.run(
                ["node", str(NODE_SCRIPT), str(work / "reopen.json"), str(work / "reopen-node.json")],
                env=node_env,
                check=True,
            )
            again = json.loads((work / "reopen-node.json").read_text())
            for entry in again["principals"]:
                figures.session(entry, opens=("reopen",))
                result["detail"][entry["label"]]["reopen"] = entry["reopen"]
            result["volumes"].append(volumes(again, "reopen"))
            result["status"]["reopen"] = control_status(control, cred)
            result["requests"] += [{**r, "phase": "reopen"} for r in again["requests"]]
            result["server"]["memory_after_reopen"] = process_memory(served.pid)
            result["server"]["cache_bytes_after_reopen"] = directory_bytes(served.cache)

        # After every measured phase: what a replay of this run needs to name its artifacts.
        result["artifact_keys"] = artifact_keys(
            args.viewer, authorise(args.session, cred, broadest), result["requests"]
        )
        if reference is not None:
            replay_out.update(replay_summary(reference, result, left_out))
            result["replay"] = replay_out
    finally:
        if served is not None and not args.keep_serving:
            served.stop()
            result.setdefault("server", {})["stopped"] = True

    result["ran_s"] = round(time.time() - started, 1)
    result["measurements"] = figures.measurements
    Path(args.out).write_text(json.dumps(result, indent=1, default=str))
    print(table(figures.measurements))
    print(f"ran {result['ran_s']} s; wrote {args.out}")
    if reference is not None:
        print(json.dumps(result["replay"], indent=1))
    if served is not None and args.keep_serving:
        print(f"left serving: pid {served.pid}; stop it by pid")
    if args.compare:
        print()
        print(compare(json.loads(Path(args.compare).read_text()), result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
