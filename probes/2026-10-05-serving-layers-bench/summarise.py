"""Tables of old against new from the bench runs in `runs/`, as Markdown on stdout.

    python3 probes/2026-10-05-serving-layers-bench/summarise.py old-1 old-2 -- new-1 new-2 [--busy-from EPOCH_S]

Each cell is the median and the largest value over every repeat (runs times regions). With
`--busy-from`, a step that started at or after that time is counted apart and listed.
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

RUNS = Path(__file__).with_name("runs")
VIEWERS = ("100%", "25%", "1%")


def load(names: list[str]) -> list[dict]:
    return [json.loads((RUNS / f"{n}.json").read_text()) for n in names]


def cell(values: list[float]) -> str:
    v = sorted(x for x in values if x is not None)
    if not v:
        return "—"
    p50 = v[math.ceil(len(v) / 2) - 1]
    return f"{p50:,.0f} / {v[-1]:,.0f}"


def mb(values: list[float]) -> str:
    v = sorted(x for x in values if x is not None)
    if not v:
        return "—"
    return f"{v[math.ceil(len(v) / 2) - 1] / 1e6:,.2f}"


def opens(runs: list[dict], viewer: str, which: str) -> list[dict]:
    return [r["detail"][viewer][which] for r in runs if which in r["detail"][viewer]]


def steps(runs: list[dict], viewer: str, zoom: int, kind: str) -> list[dict]:
    return [
        s
        for r in runs
        for s in r["detail"][viewer].get("map") or []
        if s["zoom"] == zoom and s["kind"] == kind and s["requests"]
    ]


def table(title: str, rows: list[tuple[str, str, str]], old_label: str, new_label: str) -> None:
    print(f"\n#### {title}\n")
    print(f"| viewer | measure | {old_label} | {new_label} |")
    print("|---|---|---:|---:|")
    for viewer, measure, old, new in rows:
        print(f"| {viewer} | {measure} | {old} | {new} |")


FIELDS = (
    ("first points", "first_points_ms"),
    ("last byte of points", "last_byte_ms"),
    ("last byte of layers", "layers_ms"),
    ("points beside layer work", "points_beside_layers_ms"),
    ("settled", "settled_ms"),
)


def main() -> int:
    argv = sys.argv[1:]
    busy = None
    if "--busy-from" in argv:
        i = argv.index("--busy-from")
        busy = float(argv[i + 1]) * 1000
        argv = argv[:i] + argv[i + 2 :]
    cut = argv.index("--")
    old, new = load(argv[:cut]), load(argv[cut + 1 :])
    lo, ln = "old p50 / max ms", "new p50 / max ms"

    def open_rows(which: str) -> list:
        rows = []
        for v in VIEWERS:
            for name, field in (("counts", "counts_ms"),) + FIELDS:
                rows.append(
                    (
                        v,
                        name,
                        cell([o.get(field) for o in opens(old, v, which)]),
                        cell([o.get(field) for o in opens(new, v, which)]),
                    )
                )
            rows.append(
                (
                    v,
                    "MB received",
                    mb([o.get("bytes") for o in opens(old, v, which)]),
                    mb([o.get("bytes") for o in opens(new, v, which)]),
                )
            )
        return rows

    def step_rows(zoom: int, kind: str) -> list:
        rows = []
        for v in VIEWERS:
            o, n = steps(old, v, zoom, kind), steps(new, v, zoom, kind)
            for name, field in FIELDS:
                rows.append((v, name, cell([s.get(field) for s in o]), cell([s.get(field) for s in n])))
            rows.append((v, "MB received", mb([s.get("bytes") for s in o]), mb([s.get("bytes") for s in n])))
            rows.append(
                (
                    v,
                    "artifact requests",
                    str(sum(s["kinds"].get("artifacts", 0) for s in o)),
                    str(sum(s["kinds"].get("artifacts", 0) for s in n)),
                )
            )
        return rows

    table("First open at zoom 0, a viewer new to the server", open_rows("first"), lo, ln)
    table("The same viewer opening again", open_rows("again"), lo, ln)
    table("Reopen: the server restarted over its cache", open_rows("reopen"), lo, ln)
    table("Zoom 9, genus and species, two regions per run", step_rows(9, "zoom-in"), lo, ln)
    table("Pan at zoom 9", step_rows(9, "pan"), lo, ln)
    table("Pan at zoom 6", step_rows(6, "pan"), lo, ln)
    table("Zoom out to the world from zoom 14", step_rows(0, "zoom-out"), lo, ln)

    print("\n#### Status figures (`masked_count_cache`) after each phase\n")
    print("| run | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | hits | misses | disk bytes |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for name, run in zip(argv[:cut] + argv[cut + 1 :], old + new):
        for phase, s in run.get("status", {}).items():
            print(
                f"| {name} | {phase} | {s.get('fills')} | {s.get('loads')} | {s.get('not_admitted')} | "
                f"{s.get('reserve_spent')} | {s.get('labels_rows_read')} | {s.get('exact')} | "
                f"{s.get('hits')} | {s.get('misses')} | {s.get('disk_bytes'):,} |"
            )

    print("\n#### Requests and bytes over the whole run, by kind\n")
    print("| run | kind | requests | MB | idle | whole-level |")
    print("|---|---|---:|---:|---:|---:|")
    for name, run in zip(argv[:cut] + argv[cut + 1 :], old + new):
        reqs = run["requests"]
        for kind in sorted({r["kind"] for r in reqs}):
            chosen = [r for r in reqs if r["kind"] == kind]
            print(
                f"| {name} | {kind} | {len(chosen)} | {sum(r.get('bytes', 0) for r in chosen) / 1e6:,.1f} | "
                f"{sum(1 for r in chosen if r.get('idle'))} | {sum(1 for r in chosen if r.get('whole_level'))} |"
            )

    print("\n#### Whole-level artifact requests\n")
    for name, run in zip(argv[:cut] + argv[cut + 1 :], old + new):
        for r in run["requests"]:
            if r.get("whole_level"):
                b = json.loads(r["body"])
                tiles = len(b["tiles"]) if b.get("tiles") else "bbox"
                print(
                    f"- {name} {r['step']} {r['kind']}{' (idle)' if r.get('idle') else ''}: depth {b.get('zoom')}, "
                    f"tiles {tiles}, levels {b.get('levels')}, {r['bytes'] / 1e6:,.1f} MB, {r['last_byte_ms']:,.0f} ms"
                )

    if busy is not None:
        print("\n#### Steps that started after the machine became busy\n")
        for name, run in zip(argv[:cut] + argv[cut + 1 :], old + new):
            late = [
                f"{v}/{s['kind']}@{s['zoom']}"
                for v in run["detail"]
                for s in (run["detail"][v].get("map") or [])
                if s["t0"] >= busy
            ]
            reopened = [v for v in run["detail"] if "reopen" in run["detail"][v]]
            if late:
                print(f"- {name}: {len(late)} map steps ({', '.join(late)}), and the reopen of {', '.join(reopened)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
