"""Tables of one group of bench runs against another, as Markdown on stdout.

    python3 probes/2026-10-05-serving-layers-bench/summarise.py <run dir> \
        --old old-1 old-2 --new new-1 new-2 [--labels old,new] [--beyond] [--busy-from EPOCH_S]

Each name is a run file `<run dir>/<name>.json` that `interactive_bench.py` wrote. Each cell is the
lower middle value and the largest over every repeat (runs times regions): with two values, the
smaller and the larger. With `--beyond`, the first group is runs with prefetch off and the second
the same rounds with it on, paired in order, and a table lists what each prefetch-on run sent
beyond its pair, which is the prefetch's work. With `--busy-from`, a step that started at or after
that time is listed.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from test_corpora.common.interactive_bench import sent

VIEWERS = ("100%", "25%", "1%")

FIELDS = (
    ("first points", "first_points_ms"),
    ("last byte of points", "last_byte_ms"),
    ("last byte of layers", "layers_ms"),
    ("points beside layer work", "points_beside_layers_ms"),
    ("settled", "settled_ms"),
)


def middle(values: list[float]) -> float | None:
    v = sorted(x for x in values if x is not None)
    return v[math.ceil(len(v) / 2) - 1] if v else None


def cell(values: list[float]) -> str:
    v = [x for x in values if x is not None]
    return f"{middle(v):,.0f} / {max(v):,.0f}" if v else "—"


def mb(values: list[float]) -> str:
    m = middle(values)
    return "—" if m is None else f"{m / 1e6:,.2f}"


def opens(runs: list[dict], viewer: str, which: str) -> list[dict]:
    return [r["detail"][viewer][which] for r in runs if which in r["detail"][viewer]]


def steps(runs: list[dict], viewer: str, zoom: int, kind: str) -> list[dict]:
    return [
        s
        for r in runs
        for s in r["detail"][viewer].get("map") or []
        if s["zoom"] == zoom and s["kind"] == kind and s["requests"]
    ]


def zoom_of_steps(run: dict) -> dict[str, float]:
    """Each step's map zoom; an open is at zoom 0."""
    return {s["id"]: s["zoom"] for e in run["detail"].values() for s in e.get("map") or []}


def whole_level(r: dict, zooms: dict[str, float]) -> bool:
    """As the Node side's `wholeLevel`: a promotion, or every tile of a depth more than two below
    the map zoom."""
    if r["kind"] == "promotion":
        return True
    if r["kind"] != "artifacts" or r["path"] != "/v1/artifacts/viewport":
        return False
    b = json.loads(r["body"])
    return b["zoom"] > math.floor(zooms.get(r["step"], 0)) + 2 and len(b.get("tiles") or []) >= 4 ** b["zoom"]


def beyond(on: dict, off: dict) -> list[dict]:
    """The requests `on` sent beyond what `off` sent at the same step."""
    left = Counter(sent(r) for r in off["requests"])
    out = []
    for r in on["requests"]:
        key = sent(r)
        if left[key]:
            left[key] -= 1
        else:
            out.append(r)
    return out


def table(title: str, rows: list[tuple[str, str, str, str]], labels: tuple[str, str]) -> None:
    print(f"\n#### {title}\n")
    print(f"| viewer | measure | {labels[0]} | {labels[1]} |")
    print("|---|---|---:|---:|")
    for viewer, measure, old, new in rows:
        print(f"| {viewer} | {measure} | {old} | {new} |")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("runs", type=Path, help="the directory holding the run files")
    ap.add_argument("--old", nargs="+", required=True)
    ap.add_argument("--new", nargs="+", required=True)
    ap.add_argument("--labels", default="old,new")
    ap.add_argument("--beyond", action="store_true")
    ap.add_argument("--busy-from", type=float)
    args = ap.parse_args()
    old, new = ([json.loads((args.runs / f"{n}.json").read_text()) for n in group] for group in (args.old, args.new))
    names = args.old + args.new
    labels = tuple(f"{n} middle / max ms" for n in args.labels.split(","))

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

    table("First open at zoom 0, a viewer new to the server", open_rows("first"), labels)
    table("The same viewer opening again", open_rows("again"), labels)
    table("Reopen: the server restarted over its cache", open_rows("reopen"), labels)
    table("Zoom 9, genus and species, two regions per run", step_rows(9, "zoom-in"), labels)
    table("Pan at zoom 9", step_rows(9, "pan"), labels)
    table("Pan at zoom 6", step_rows(6, "pan"), labels)
    table("Zoom out to the world from zoom 14", step_rows(0, "zoom-out"), labels)

    print("\n#### Status figures (`masked_count_cache`) after each phase\n")
    print("| run | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | hits | misses | disk bytes |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for name, run in zip(names, old + new):
        for phase, s in run.get("status", {}).items():
            print(
                f"| {name} | {phase} | {s.get('fills')} | {s.get('loads')} | {s.get('not_admitted')} | "
                f"{s.get('reserve_spent')} | {s.get('labels_rows_read')} | {s.get('exact')} | "
                f"{s.get('hits')} | {s.get('misses')} | {s.get('disk_bytes'):,} |"
            )

    print("\n#### Requests and bytes over the whole run, by kind\n")
    print("| run | kind | requests | MB | whole-level |")
    print("|---|---|---:|---:|---:|")
    for name, run in zip(names, old + new):
        zooms = zoom_of_steps(run)
        reqs = run["requests"]
        for kind in sorted({r["kind"] for r in reqs}):
            chosen = [r for r in reqs if r["kind"] == kind]
            print(
                f"| {name} | {kind} | {len(chosen)} | {sum(r.get('bytes', 0) for r in chosen) / 1e6:,.1f} | "
                f"{sum(1 for r in chosen if whole_level(r, zooms))} |"
            )

    print("\n#### Whole-level artifact requests\n")
    for name, run in zip(names, old + new):
        zooms = zoom_of_steps(run)
        for r in run["requests"]:
            if whole_level(r, zooms):
                b = json.loads(r["body"])
                tiles = len(b["tiles"]) if b.get("tiles") else "bbox"
                print(
                    f"- {name} {r['step']} {r['kind']}: depth {b.get('zoom')}, tiles {tiles}, "
                    f"levels {b.get('levels')}, {r['bytes'] / 1e6:,.1f} MB, {r['last_byte_ms']:,.0f} ms"
                )

    if args.beyond:
        print("\n#### What each prefetch-on run sent beyond its prefetch-off pair\n")
        print("| run | kind | requests | MB | slowest ms |")
        print("|---|---|---:|---:|---:|")
        for name, on, off in zip(args.new, new, old):
            extra = beyond(on, off)
            for kind in sorted({r["kind"] for r in extra}):
                chosen = [r for r in extra if r["kind"] == kind]
                print(
                    f"| {name} | {kind} | {len(chosen)} | {sum(r.get('bytes', 0) for r in chosen) / 1e6:,.1f} | "
                    f"{max(r['last_byte_ms'] or 0 for r in chosen):,.0f} |"
                )

    if args.busy_from is not None:
        busy = args.busy_from * 1000
        print("\n#### Steps that started after the machine became busy\n")
        for name, run in zip(names, old + new):
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
