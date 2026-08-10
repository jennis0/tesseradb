"""Fold the raw CSVs into the tables `results.md` quotes. Reads `raw/`, prints markdown."""

import csv
import sys
from collections import defaultdict
from pathlib import Path

RAW = Path(sys.argv[1] if len(sys.argv) > 1 else "raw")
STAGES = ["s0-build", "s1-flushed", "s2-coalesced", "s3-folded"]


def query_table(scale):
    rows = defaultdict(dict)
    card = defaultdict(dict)
    with open(RAW / f"query-{scale}.csv") as f:
        for r in csv.DictReader(f):
            if r["candidate"] != "masked":
                continue
            rows[r["query"]][r["stage"]] = float(r["ms"])
            card[r["query"]][r["stage"]] = int(r["cardinality"])
    print(f"\n### {scale} — masked filter latency, ms (median of 3)\n")
    print("| query | s0 build | s1 +64 flushes | s2 coalesced | s3 folded | s1/s0 | s3/s0 |")
    print("|---|---|---|---|---|---|---|")
    for q in sorted(rows):
        v = rows[q]
        base = v.get("s0-build")
        cells = " | ".join(f"{v.get(s, float('nan')):.3f}" for s in STAGES)
        r1 = v["s1-flushed"] / base if base else float("nan")
        r3 = v["s3-folded"] / base if base else float("nan")
        print(f"| `{q}` | {cells} | {r1:.2f}× | {r3:.2f}× |")


def stage_table(scale):
    print(f"\n### {scale} — open, residency and layer count\n")
    print("| stage | open ms | extents in manifest | layers/column | attr files on disc | attr bytes |")
    print("|---|---|---|---|---|---|")
    with open(RAW / f"stage-{scale}.csv") as f:
        for r in csv.DictReader(f):
            print(
                f"| {r['stage']} | {float(r['open_ms']):.2f} | {r['extent_files']} | "
                f"{r['layers_archive']} | {r['attrs_files']} | {int(r['attrs_bytes']):,} |"
            )


def write_table(scale):
    print(f"\n### {scale} — write side\n")
    flushes = []
    with open(RAW / f"write-{scale}.csv") as f:
        for r in csv.DictReader(f):
            if r["event"] == "flush":
                flushes.append(float(r["wall_ms"]))
            elif r["event"] in ("build", "fold"):
                print(f"- **{r['event']}**: {float(r['wall_ms']) / 1000:.1f} s, "
                      f"attrs {int(r['attrs_bytes']):,} bytes in {r['attrs_files']} files")
    if flushes:
        flushes_sorted = sorted(flushes)
        print(f"- **flush**: {len(flushes)} of them, median {flushes_sorted[len(flushes)//2]:.1f} ms, "
              f"min {flushes_sorted[0]:.1f}, max {flushes_sorted[-1]:.1f}")
    coal = []
    with open(RAW / f"write-{scale}.csv") as f:
        for r in csv.DictReader(f):
            if r["event"] == "coalesce":
                coal.append((int(r["index"]), float(r["wall_ms"]), int(r["extents"]),
                             int(r["attrs_files"]), int(r["attrs_bytes"])))
    if coal:
        print(f"\n| coalesce pass | ms | extents left | attr files on disc | attr bytes on disc |")
        print("|---|---|---|---|---|")
        for i, ms, ex, af, ab in coal:
            print(f"| {i} | {ms:.0f} | {ex} | {af} | {ab:,} |")


def fold_table(scale):
    p = RAW / f"fold-{scale}.csv"
    if not p.exists():
        return
    print(f"\n### {scale} — the fold's own staircase\n")
    print("| pass | ms | rss at end | anon at end |")
    print("|---|---|---|---|")
    with open(p) as f:
        for r in csv.DictReader(f):
            print(f"| {r['pass']} | {float(r['elapsed_ms']):.0f} | {int(r['rss_bytes']):,} | "
                  f"{int(r['anon_bytes']):,} |")


for scale in sys.argv[2:] or ["2422486", "25000000"]:
    if not (RAW / f"query-{scale}.csv").exists():
        continue
    query_table(scale)
    stage_table(scale)
    write_table(scale)
    fold_table(scale)
