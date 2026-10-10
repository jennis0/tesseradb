#!/usr/bin/env python3
"""Print the README's tables from `runs/`: medians over the three runs of each binary."""

import csv
import json
import os
import statistics

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = os.path.join(HERE, "runs")


def blocks(name):
    out = []
    with open(os.path.join(RUNS, name)) as f:
        for block in f.read().split("\n\n"):
            lines = [line for line in block.strip().splitlines() if line]
            if lines:
                out.append(list(csv.DictReader(lines)))
    return out


def underlay(block, title, offsets, first_version=False):
    """One table: the underlay loop's time and the request's, before and after."""
    rows = {}
    for side in ("before", "after"):
        for run in ("r1", "r2", "r3"):
            for r in blocks(f"underlay-{side}-{run}.csv")[block]:
                if r["offset"] not in offsets:
                    continue
                key = (r["principal"], r.get("depth") or r.get("zoom"), r["offset"])
                rows.setdefault(key, {}).setdefault(side, []).append(
                    (int(r["underlay_us"]), int(r["total_us"]))
                )
    first = {}
    if first_version:
        for r in blocks("underlay-first-version.csv")[block]:
            first[(r["principal"], r.get("depth") or r.get("zoom"), r["offset"])] = int(
                r["underlay_us"]
            )
    print(f"\n### {title}\n")
    head = "| viewer | depth | offset | underlay before | underlay after | ratio | request before | request after | ratio |"
    if first_version:
        head += " underlay, first version |"
    print(head)
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|" + ("---:|" if first_version else ""))
    for key, sides in rows.items():
        ub = statistics.median(u for u, _ in sides["before"]) / 1e3
        ua = statistics.median(u for u, _ in sides["after"]) / 1e3
        tb = statistics.median(t for _, t in sides["before"]) / 1e3
        ta = statistics.median(t for _, t in sides["after"]) / 1e3
        line = (
            f"| {key[0]} | {key[1]} | {key[2]} | {ub:,.2f} | {ua:,.2f} | {ub / ua:.2f} |"
            f" {tb:,.1f} | {ta:,.1f} | {tb / ta:.2f} |"
        )
        if first_version:
            line += f" {first[key] / 1e3:,.2f} |"
        print(line)


def aggregate():
    print("\n### Aggregate cell counts, whole view, median ms of five requests\n")
    print("| case | route | before, run 1 | before, run 2 | after, run 1 | after, run 2 |")
    print("|---|---|---:|---:|---:|---:|")
    for case in ("density d6", "density d16", "density d32", "bay top 10 x cells d16"):
        cells, route = [], None
        for side in ("before", "after"):
            for run in ("r1", "r2"):
                with open(os.path.join(RUNS, f"aggregate-{side}-{run}.jsonl")) as f:
                    for line in f:
                        j = json.loads(line)
                        if j.get("case") == case and not j.get("region"):
                            cells.append(j["ms"]["median"])
                            route = j["method"]
        print(f"| {case} | {route} | " + " | ".join(f"{c:.2f}" for c in cells) + " |")


underlay(1, "Density underlay over the whole view", {"2", "3", "4", "5"}, first_version=True)
underlay(0, "Density underlay under one tile", {"4", "5"})
aggregate()
