"""One CSV row per principal of each zero-round run, fresh and churned bundles served back to back."""

import csv
import json
import sys

out = csv.writer(sys.stdout)
out.writerow(["run", "bundle", "postings_bytes", "target", "visible", "union_ms", "p50_ms", "p95_ms",
              "p99_ms", "wall_p99_ms", "first_viewport_ms"])
for path in sys.argv[1:]:
    run = path.rsplit("ab-", 1)[-1].split(".")[0]
    row = json.loads(open(path).readline())
    for p in sorted(row["principals"], key=lambda p: p["target"]):
        s = p["server_ms"]
        out.writerow([run, run.rstrip("2"), row["postings_bytes"], p["target"], p["visible"],
                      p["union_ms"], s["p50"], s["p95"], s["p99"], p["wall_ms"]["p99"],
                      p["first_viewport_ms"]])
