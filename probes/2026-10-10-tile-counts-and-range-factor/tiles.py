"""Before and after, per (viewer, view, depth): the count stage and the request's wall time, each
the median over rounds of each round's figure, and a check that both sides served the same counts.

Viewers: narrow, medium, broad and everything from terms 0..200, and everything from 0..20."""
import csv, glob, os, statistics, sys

RUNS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs")
from collections import defaultdict

fig = defaultdict(lambda: defaultdict(list))
served = defaultdict(set)
meta = {}
for path in sorted(glob.glob(os.path.join(RUNS, "t*-*-r*.csv"))):
    hi, side, _ = os.path.basename(path)[1:-4].split("-")
    for row in csv.DictReader(open(path)):
        if row["refused"]:
            continue
        name = row["principal"]
        if hi == "20":
            if name != "everything":
                continue
            name = "21 terms"
        key = (name, row["fraction"], int(row["depth"]))
        fig[key][side + "_server"].append(int(row["server_us"]))
        fig[key][side + "_count"].append(int(row["count_us"]))
        served[key].add((row["visible_total"], row["tiles_nonempty"], row["sigma_visible"]))
        meta[key] = (int(row["visible_total"]), int(row["tiles_resolved"]), int(row["tiles_nonempty"]))

bad = [k for k, v in served.items() if len(v) != 1]
order = {"narrow": 0, "medium": 1, "broad": 2, "21 terms": 3, "everything": 4}
views = {"1": "whole", "1/4": "a quarter", "1/16": "a sixteenth"}
print("| viewer | visible | view | depth | tiles | non-empty | count before | count after | request before | request after | after ÷ before |")
print("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|")
for key in sorted(fig, key=lambda k: (order[k[0]], list(views).index(k[1]), k[2])):
    f = fig[key]
    if not f["before_server"] or not f["after_server"]:
        continue
    vis, tiles, nonempty = meta[key]
    med = lambda xs: statistics.median(xs) / 1000
    cb, ca, sb, sa = med(f["before_count"]), med(f["after_count"]), med(f["before_server"]), med(f["after_server"])
    print(f"| {key[0]} | {vis:,} | {views[key[1]]} | {key[2]} | {tiles:,} | {nonempty:,} | {cb:.2f} | {ca:.2f} | {sb:.2f} | {sa:.2f} | {sa/sb:.2f} |")
print()
print(f"rows whose counts differ between runs: {len(bad)}", file=sys.stderr)
for k in bad[:10]:
    print(k, served[k], file=sys.stderr)
