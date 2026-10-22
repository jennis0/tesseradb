"""The before-and-after table for how a run reads its key filter, from the
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
STEPS = [
    ("before", "before"),
    ("step1", "1: sequential open"),
    ("step2a", "2a: and advise every page"),
    ("step2", "2: and advise pages not held"),
]
COLUMNS = [
    ("open cold ms", lambda r: r["open_cold_ns"] / 1e6),
    ("open major faults", lambda r: r["open_cold_major_faults"]),
    ("cold batch major faults", lambda r: r["batch_cold_major_faults"] / 3),
    ("batch warm ms", lambda r: r["batch_warm_ns"] / 1e6),
    ("batch, pages cold, ms", lambda r: r["batch_pages_cold"]["p50_ns"] / 1e6),
    ("batch cold ms", lambda r: r["batch_cold"]["p50_ns"] / 1e6),
    ("present warm µs", lambda r: r["present_warm"]["p50_ns"] / 1e3),
    ("refused warm µs", lambda r: r["refused_warm"]["p50_ns"] / 1e3 if r["refused_warm"] else None),
    ("refused cold µs", lambda r: r["refused_cold"]["p50_ns"] / 1e3 if r["refused_cold"] else None),
    ("present cold µs", lambda r: r["present_cold"]["p50_ns"] / 1e3),
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


def main():
    print("| run | step | filter | " + " | ".join(c for c, _ in COLUMNS) + " | rounds | load |")
    print("|" + "---|" * (5 + len(COLUMNS)))
    for size, label in SIZES:
        for step, name in STEPS:
            done = rounds(step, size)
            for spec in ["0", "10:7"]:
                rows = [r[spec] for r in done if spec in r]
                if not rows:
                    continue
                cells = []
                for _, f in COLUMNS:
                    vals = [v for v in (f(r) for r in rows) if v is not None]
                    cells.append(fmt(statistics.median(vals)) if vals else "–")
                loads = [float(r["load_before"].split()[0]) for r in rows]
                print(f"| {label} | {name} | {spec} | " + " | ".join(cells)
                      + f" | {len(rows)} | {min(loads):.1f}–{max(loads):.1f} |")


if __name__ == "__main__":
    main()
