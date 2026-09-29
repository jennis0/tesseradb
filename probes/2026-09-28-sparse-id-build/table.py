"""Print runs of `probe.py` side by side, one row per stage.

    python3 table.py new-s1e8.json main-s1e8.json ...

Each cell is `seconds / MiB read / MiB written / major faults` for the stage in that run, summed
over the stage's records where it ran once a batch.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    runs = [(Path(p).stem, json.loads(Path(p).read_text())) for p in sys.argv[1:]]
    stages: list[str] = []
    for _, run in runs:
        for stage in run["stages"]:
            if stage["stage"] not in stages:
                stages.append(stage["stage"])
    print("| stage | " + " | ".join(name for name, _ in runs) + " |")
    print("|---" * (len(runs) + 1) + "|")
    for name in stages:
        cells = []
        for _, run in runs:
            found = [s for s in run["stages"] if s["stage"] == name]
            if not found:
                cells.append("")
                continue
            total = {
                key: sum(s[key] or 0 for s in found)
                for key in ("wall_s", "read_mib", "written_mib", "major_faults")
            }
            cells.append(
                f"{total['wall_s']:.1f} s / {total['read_mib']:,} / {total['written_mib']:,} / "
                f"{total['major_faults']:,}"
            )
        print(f"| {name} | " + " | ".join(cells) + " |")
    print(
        "| **whole build** | "
        + " | ".join(
            f"**{r['wall_s']:.1f} s** / {r['read_mib']:,} / {r['written_mib']:,} / "
            f"{r['major_faults'] or 0:,}, exit {r['exit']}, peak anon {r['peak_anon_mib']:,} MiB"
            for _, r in runs
        )
        + " |"
    )


if __name__ == "__main__":
    main()
