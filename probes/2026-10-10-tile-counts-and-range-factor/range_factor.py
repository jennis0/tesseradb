"""The density cells of the whole view, counted by range and by the pass, per viewer and depth:
the request's median over rounds of each round's median, and items per cell that could hold them,
the ratio RANGE_FACTOR is compared with. Checks both routes served the same rows."""
import glob, json, os, statistics, sys

RUNS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs")
from collections import defaultdict

VIEW_ROWS = 20_000_000
ms = defaultdict(list)
rows = defaultdict(set)
items = {}
method = defaultdict(set)
for path in sorted(glob.glob(os.path.join(RUNS, "agg-*-r*.jsonl"))):
    _, viewer, route, _ = os.path.basename(path)[:-6].split("-")
    for line in open(path):
        rec = json.loads(line)
        case = rec.get("case", "")
        if not case.startswith("density d") or rec.get("region"):
            continue
        depth = int(case.split("d")[-1])
        ms[(viewer, depth, route)].append(rec["ms"]["median"])
        rows[(viewer, depth)].add(rec["rows"])
        items[viewer] = rec["items"]
        method[(viewer, depth, route)].add(rec["method"])

order = {"t1": 0, "t3": 1, "t21": 2, "all": 3}
print("| viewer's items | depth | cells | items ÷ cells | by range ms | by the pass ms | range ÷ pass |")
print("|---:|---:|---:|---:|---:|---:|---:|")
for viewer in sorted(items, key=order.get):
    for depth in sorted({d for (v, d, _) in ms if v == viewer}):
        r, p = ms.get((viewer, depth, "ranges")), ms.get((viewer, depth, "pass"))
        if not r or not p:
            continue
        cells = min(4 ** depth, VIEW_ROWS)
        rm, pm = statistics.median(r), statistics.median(p)
        print(f"| {items[viewer]:,} | {depth} | {cells:,} | {items[viewer] / cells:,.1f} | {rm:.2f} | {pm:.2f} | {rm / pm:.2f} |")
bad = [k for k, v in rows.items() if len(v) != 1]
wrong = [k for k, v in method.items() if v != {k[2]}]
print(f"\nrows differing between routes: {bad}; route not the one forced: {wrong}", file=sys.stderr)
