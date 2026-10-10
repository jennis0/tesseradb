"""Prints the README's tables from medians.csv.

Table 1: a request's mask work, the count and the selection summed, in ms. "Today" is the
per-range count and `select_tiles` at N = 1. "Per part" is `count_ranges` and `select_parts`;
"batched" is `count_ranges` and `select_batched`. The ratio in brackets is against today.

Table 2: the selection alone, `select_batched` over `select_parts`, at each N."""
import csv, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
cell = {}
for r in csv.DictReader(open(os.path.join(HERE, "medians.csv"))):
    key = (r["tiles_layout"], int(r["tiles"]), float(r["coverage_pct"]), int(r["depth"]), int(r["shards"]), r["op"])
    cell[key] = float(r["us_median"]) / 1000

def ms(x):
    if x >= 100:
        return f"{x:,.0f}"
    if x >= 10:
        return f"{x:.1f}"
    if x >= 1:
        return f"{x:.2f}"
    return f"{x:.3f}"


layout = sys.argv[1] if len(sys.argv) > 1 else "abutting"
for tiles in (3000, 256):
    print(f"\n### Table 1, {layout} tiles, {tiles:,} tiles\n")
    print("| coverage | depth | today N = 1 | N = 8 per part | N = 8 batched | N = 32 per part | N = 32 batched | N = 100 per part | N = 100 batched |")
    print("|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for cov in (50.0, 10.0, 1.0, 0.1):
        for d in range(6, 15):
            k = lambda n, op: cell.get((layout, tiles, cov, d, n, op))
            if k(1, "count") is None or k(1, "select_tiles") is None:
                continue
            today = k(1, "count") + k(1, "select_tiles")
            row = f"| {cov:g}% | {d} | {ms(today)} |"
            for n in (8, 32, 100):
                for sel in ("select_parts", "select_batched"):
                    c, s = k(n, "count_ranges"), k(n, sel)
                    row += " — |" if c is None or s is None else f" {ms(c + s)} ({(c + s) / today:.1f}×) |"
            print(row)
    print(f"\n### Table 2, {layout} tiles, {tiles:,} tiles: selection batched over selection per part\n")
    print("| coverage | depth | N = 1 | N = 8 | N = 32 | N = 100 |")
    print("|---:|---:|---:|---:|---:|---:|")
    for cov in (50.0, 10.0, 1.0, 0.1):
        for d in range(6, 15):
            vals = []
            for n in (1, 8, 32, 100):
                p, b = cell.get((layout, tiles, cov, d, n, "select_parts")), cell.get((layout, tiles, cov, d, n, "select_batched"))
                vals.append("—" if p is None or b is None else f"{b / p:.2f}")
            if all(v == "—" for v in vals):
                continue
            print(f"| {cov:g}% | {d} | " + " | ".join(vals) + " |")
