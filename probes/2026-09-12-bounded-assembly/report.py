"""Each stage's memory, write amplification and faults, from a sampler run and its stage records.

Reads one directory per binary, each holding `proc.tsv` (from `sample.py`) and `stages.json`
(from `--stage-timings-json`), and prints Markdown: one table per binary over the stages, then
one row per binary over the whole build.

A stage's figures are the sampler's counters differenced across the stage's `started_at` and
`ended_at`, at the last sample at or before each boundary. The acceptance figure of the
bounded-assembly design (`docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §8) is the
write ratio: process write bytes over bundle growth, 1.5 or less in every stage.

  report.py <label>=<dir> [<label>=<dir> ...]
"""

import json
import sys
from pathlib import Path

GIB = float(1 << 30)

# Reported from inside an enclosing column loop, so the interval is an interval of the right
# length placed at the report, not when the stage ran (`observer.rs`, `StageRecord`).
CHARGED = {"text_index", "record_blob", "column_release", "filter_postings"}


def read_tsv(path):
    rows = []
    with open(path) as f:
        head = f.readline().rstrip("\n").split("\t")
        for line in f:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != len(head):
                continue  # a truncated final line: the sampler was still writing it
            rows.append({k: float(v) for k, v in zip(head, parts)})
    return rows


def at(rows, t):
    """The last sample at or before t, or the first sample if t precedes them all."""
    chosen = None
    for r in rows:
        if r["t"] <= t:
            chosen = r
        else:
            break
    return chosen if chosen is not None else (rows[0] if rows else None)


def window(rows, start, end):
    inside = [r for r in rows if start <= r["t"] <= end]
    if not inside:
        nearest = at(rows, end)
        inside = [nearest] if nearest else []
    return inside


def gib(b):
    return f"{b / GIB:.3f}"


def stage_table(label, rows, stages):
    print(f"### {label}")
    print()
    print(
        "| stage | wall s | peak RssAnon GiB | write GiB | bundle growth GiB | "
        "ratio | major faults | read GiB |"
    )
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for s in stages:
        start, end = s["started_at"], s["ended_at"]
        a, b = at(rows, start), at(rows, end)
        inside = window(rows, start, end)
        if a is None or b is None or not inside:
            print(f"| {s['stage']} | {s['wall_s']:.1f} | no samples | | | | | |")
            continue
        peak_anon = max(r["rss_anon_kb"] for r in inside) * 1024
        wrote = b["write_bytes"] - a["write_bytes"]
        grew = b["bundle_alloc_b"] - a["bundle_alloc_b"]
        read = b["read_bytes"] - a["read_bytes"]
        faults = int(b["majflt"] - a["majflt"])
        ratio = f"{wrote / grew:.2f}" if grew > 0 else "n/a"
        name = s["stage"] + (" ‡" if s["stage"] in CHARGED else "")
        print(
            f"| {name} | {s['wall_s']:.1f} | {gib(peak_anon)} | {gib(wrote)} | "
            f"{gib(grew)} | {ratio} | {faults} | {gib(read)} |"
        )
    print()
    if any(s["stage"] in CHARGED for s in stages):
        print(
            "‡ Reported from inside the column loop: the duration is measured, the interval it "
            "is placed at is not when the stage ran, so this row's counters are those of "
            "whatever ran over that interval."
        )
        print()


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    runs = []
    for arg in sys.argv[1:]:
        label, _, where = arg.partition("=")
        d = Path(where)
        runs.append((label, read_tsv(d / "proc.tsv"), json.load(open(d / "stages.json"))))

    print("## Per stage")
    print()
    for label, rows, stages in runs:
        stage_table(label, rows, stages)

    print("## Whole build")
    print()
    print(
        "| binary | wall s | peak RssAnon GiB | peak VmSwap GiB | peak allocated disk GiB | "
        "write GiB | read GiB | major faults |"
    )
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for label, rows, stages in runs:
        if not rows:
            print(f"| {label} | no samples | | | | | | |")
            continue
        wall = rows[-1]["t"] - rows[0]["t"]
        print(
            f"| {label} | {wall:.1f} | "
            f"{gib(max(r['rss_anon_kb'] for r in rows) * 1024)} | "
            f"{gib(max(r['vm_swap_kb'] for r in rows) * 1024)} | "
            f"{gib(max(r['bundle_alloc_b'] for r in rows))} | "
            f"{gib(rows[-1]['write_bytes'] - rows[0]['write_bytes'])} | "
            f"{gib(rows[-1]['read_bytes'] - rows[0]['read_bytes'])} | "
            f"{int(rows[-1]['majflt'] - rows[0]['majflt'])} |"
        )
    print()
    print(
        "Whole-build wall time is the sampler's span, which starts after the build and ends "
        "within one sampling interval of it."
    )


if __name__ == "__main__":
    main()
