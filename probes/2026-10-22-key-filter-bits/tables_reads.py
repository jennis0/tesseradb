"""The before-and-after tables for how a run reads its key filter, from the
raw-reads-<step>-<entries>-<round>.txt files run-reads.sh writes beside this file. Each cell is the
median over the rounds of that run's figure; the load column gives the range of one-minute load
averages the rounds started at.

    python3 tables_reads.py
"""

import json
import statistics
from pathlib import Path

HERE = Path(__file__).resolve().parent
SIZES = [("67108864", "2²⁶"), ("233000000", "233 M")]
STEPS = [("before", "ed2e5a742"), ("prev", "147eb1b0f"), ("now", "now")]
OPEN_AND_BATCH = [
    ("open cold ms", lambda r: r["open_cold_ns"] / 1e6),
    ("open major faults", lambda r: r["open_cold_major_faults"]),
    ("10,000 warm ms", lambda r: r["batch_warm_ns"] / 1e6),
    ("10,000 pages cold ms", lambda r: r["batch_pages_cold"]["p50_ns"] / 1e6),
    ("10,000 cold ms", lambda r: r["batch_cold"]["p50_ns"] / 1e6),
    ("cold batch major faults", lambda r: r["batch_cold_major_faults"] / 3),
    ("present warm µs", lambda r: r["present_warm"]["p50_ns"] / 1e3),
    ("refused warm µs", lambda r: r["refused_warm"]["p50_ns"] / 1e3 if r["refused_warm"] else None),
    ("refused cold µs", lambda r: r["refused_cold"]["p50_ns"] / 1e3 if r["refused_cold"] else None),
    ("present cold µs", lambda r: r["present_cold"]["p50_ns"] / 1e3),
]
SIZED = [
    (f"{size} {case} µs", lambda r, s=size, c=case: r["batches"][s][c]["p50_ns"] / 1e3)
    for size in ["2", "10", "100", "10000"]
    for case in ["just_opened", "warm", "after_eviction"]
]


def rounds(step, size):
    out = []
    for path in sorted(HERE.glob(f"raw-reads-{step}-{size}-*.txt")):
        rows = {}
        for line in path.read_text().splitlines():
            if line.startswith("filter-shape "):
                row = json.loads(line.split(" ", 1)[1])
                rows[row["spec"]] = row
        out.append(rows)
    return out


def fmt(v):
    if v is None:
        return "–"
    return f"{v:,.0f}" if v >= 100 else f"{v:.2f}" if v < 10 else f"{v:.1f}"


def table(columns, specs):
    print("| run | reader | filter | " + " | ".join(c for c, _ in columns) + " | rounds | load |")
    print("|" + "---|" * (5 + len(columns)))
    for size, label in SIZES:
        for step, name in STEPS:
            done = rounds(step, size)
            for spec in specs:
                rows = [r[spec] for r in done if spec in r]
                if not rows:
                    continue
                cells = []
                for _, f in columns:
                    vals = [v for v in (f(r) for r in rows) if v is not None]
                    cells.append(fmt(statistics.median(vals)) if vals else "–")
                loads = [float(r["load_before"].split()[0]) for r in rows]
                print(f"| {label} | {name} | {spec} | " + " | ".join(cells)
                      + f" | {len(rows)} | {min(loads):.1f}–{max(loads):.1f} |")
    print()


def main():
    table(OPEN_AND_BATCH, ["0", "10:7"])
    table(SIZED, ["0", "10:7"])


if __name__ == "__main__":
    main()
