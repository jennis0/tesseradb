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
* **Lookups.** For the narrowest and the broadest principal: an item card by `tessera_id`, and a
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
        [--prefetch [--reference prefetch-off-run.json]]

Build the TypeScript core first: `npm --prefix clients/ts run build -w @mosaica/client`.
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
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path

import requests
from pyarrow import ipc

from .deployment import Deployment, read_env_file, tomllib
from .serve_battery import Evictor, compose_ladder, frames

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


def write_plan(path: Path, args, people: list[dict], phase: str = "run") -> None:
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
    }
    path.write_text(json.dumps(plan))


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
    figures: Figures, viewer: str, entry: dict, view_id: str, field: str
) -> dict:
    http = requests.Session()
    http.headers["Authorization"] = f"Bearer {entry['token']}"
    card_ms, filter_ms, rows = [], [], []
    card_failed = filter_failed = 0
    values = []
    for tid in entry["lookup_ids"]:
        t0 = time.perf_counter()
        r = http.post(f"{viewer}/v1/items/{tid}", json={}, timeout=120)
        ms = (time.perf_counter() - t0) * 1000.0
        if r.status_code != 200:
            card_failed += 1
            continue
        card_ms.append(ms)
        values.append(r.json()["fields"].get(field))
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
    figures.put(f"lookup.{entry['label']}.item_card", card_ms, errors=card_failed)
    figures.put(
        f"lookup.{entry['label']}.filter_eq_{field}", filter_ms, errors=filter_failed
    )
    return {
        "ids_served": entry["ids_served"],
        "asked": len(entry["lookup_ids"]),
        "filter_rows": {str(n): rows.count(n) for n in sorted(set(rows))},
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
    ap.add_argument("--out", required=True)
    ap.add_argument(
        "--compare", help="an earlier run's JSON to compare figures and requests with"
    )
    args = ap.parse_args(argv)

    if not Path(args.core).is_file():
        ap.error(
            f"{args.core} is missing; build it with `npm --prefix clients/ts run build -w @mosaica/client`"
        )
    if args.reopen and not args.start:
        ap.error("--reopen needs --start, since it restarts the server")
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
    if not args.ranks:
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
    figures = Figures()
    try:
        if served is not None:
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
            pid = served.pid
        else:
            pid = listening_pid(port_of(args.viewer.rsplit("/", 1)[-1]))
            result["server"] = {
                "started": False,
                "fresh_cache": False,
                "pid": pid,
                "memory_before": process_memory(pid),
            }
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
            },
        )

        people = principals(json.loads(Path(args.ranks).read_text()), targets)
        work = Path(args.out).with_suffix(".work")
        work.mkdir(exist_ok=True)
        write_plan(work / "plan.json", args, people)
        log("session and map, in the TypeScript core")
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
        with_ids = [e for e in node["principals"] if "lookup_ids" in e]
        if with_ids:
            view_id = check_unique_field(
                args.viewer, with_ids[0]["token"], args.unique_field
            )
            for entry in with_ids:
                result["lookups"][entry["label"]] = lookups(
                    figures, args.viewer, entry, view_id, args.unique_field
                )
        for entry in node["principals"]:
            entry.pop("token", None)
        result["principals"] = [
            {
                **{k: e[k] for k in ("label", "terms", "visible", "regions")},
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
            write_plan(work / "reopen.json", args, people, phase="reopen")
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
    finally:
        if served is not None and not args.keep_serving:
            served.stop()
            result.setdefault("server", {})["stopped"] = True

    result["ran_s"] = round(time.time() - started, 1)
    result["measurements"] = figures.measurements
    Path(args.out).write_text(json.dumps(result, indent=1, default=str))
    print(table(figures.measurements))
    print(f"ran {result['ran_s']} s; wrote {args.out}")
    if served is not None and args.keep_serving:
        print(f"left serving: pid {served.pid}; stop it by pid")
    if args.compare:
        print()
        print(compare(json.loads(Path(args.compare).read_text()), result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
