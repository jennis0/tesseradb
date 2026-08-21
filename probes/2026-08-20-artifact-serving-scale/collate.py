#!/usr/bin/env python3
"""Fold the re-measurement's per-run CSVs into one record and one median table.

The campaign's first pass reported the **minimum** of three iterations inside a run and then the
design quoted the minimum of three whole runs, which is a minimum of nine and is not a number any
request will see. This keeps every run and reports the median, with the spread beside it so a cell
whose runs disagree says so.

    collate.py <data-dir> <tag>

reads `<data-dir>/<tag>-run*.csv` and writes `<data-dir>/<tag>-all.csv` (every run, with a `run`
column) and `<data-dir>/<tag>-medians.csv` (one row per cell).
"""

import csv
import glob
import re
import statistics
import sys

KEY = ["arm", "rows", "artifacts", "mask_pct", "viewport_pct", "depth", "route"]
# Timing columns: median across runs, with the run-to-run spread reported separately.
TIMES = ["candidacy_us", "count_us", "containment_us", "cut_us", "total_us"]


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    data, tag = sys.argv[1], sys.argv[2]
    paths = sorted(glob.glob(f"{data}/{tag}-run*.csv"))
    if not paths:
        print(f"no runs matching {data}/{tag}-run*.csv", file=sys.stderr)
        return 1

    rows = []
    header = None
    for path in paths:
        run = re.search(r"-run(\d+)\.csv$", path).group(1)
        with open(path, newline="") as handle:
            reader = csv.DictReader(handle)
            header = reader.fieldnames
            for row in reader:
                row["run"] = run
                rows.append(row)

    with open(f"{data}/{tag}-all.csv", "w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=["run"] + header)
        writer.writeheader()
        writer.writerows(rows)

    cells: dict[tuple, list[dict]] = {}
    for row in rows:
        cells.setdefault(tuple(row[k] for k in KEY), []).append(row)

    out_fields = (
        KEY
        + ["runs", "setup_ms"]
        + [f"{t}_median" for t in TIMES]
        + ["total_us_min", "total_us_max", "candidates", "served"]
    )
    with open(f"{data}/{tag}-medians.csv", "w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=out_fields)
        writer.writeheader()
        for key, group in sorted(cells.items()):
            record = dict(zip(KEY, key))
            record["runs"] = len(group)
            record["setup_ms"] = group[0]["setup_ms"]
            for t in TIMES:
                record[f"{t}_median"] = round(
                    statistics.median(float(r[t]) for r in group)
                )
            # The spread is over whole runs *and* over the iterations inside them: `min_total_us`
            # and `max_total_us` are the run's own extremes, so the envelope is the union.
            record["total_us_min"] = round(min(float(r["min_total_us"]) for r in group))
            record["total_us_max"] = round(max(float(r["max_total_us"]) for r in group))
            record["candidates"] = group[0]["candidates"]
            record["served"] = group[0]["served"]
            writer.writerow(record)
    print(f"{tag}: {len(paths)} runs, {len(cells)} cells")
    return 0


if __name__ == "__main__":
    sys.exit(main())
