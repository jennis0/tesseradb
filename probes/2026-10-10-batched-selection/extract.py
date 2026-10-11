"""Every cell's median from run.sh's JSON, one line each: `python3 extract.py DIR > medians.csv`."""
import json, os, sys

out = sys.stdout
out.write("tiles_layout,tiles,coverage_pct,depth,shards,op,visible,us_median\n")
rows = []
for layout in ("abutting", "jittered"):
    for tiles in (3000, 256):
        report = json.load(open(os.path.join(sys.argv[1], f"{layout}-{tiles}.json")))
        for c in report["cells"]:
            rows.append((layout, tiles, c["coverage_pct"], c["depth"], c["shards"], c["op"], c["visible"],
                         round(c["us_median"], 1)))
for r in sorted(rows):
    out.write(",".join(map(str, r)) + "\n")
