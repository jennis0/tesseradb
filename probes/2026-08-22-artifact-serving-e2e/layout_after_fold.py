"""Which layout a level is served by, before its first fold and after it.

**Why this is a step of its own.** The 10⁷ grid measures the partition relation twice — once as a
stored member list and once as a predicate over the column it partitions — and the two are 243 ms
and 135 ms at the broad principal, 1 054 ms and 85 ms at the narrow one. The server's own log says
why: the predicate spelling is served `RowMajorLabel` and the enumerated one `ArtifactMajor`, at
**73.3 blocks per artifact**, which is above `layout::ROW_MAJOR_BLOCKS_PER_ARTIFACT` (10.0) with
99 997 artifacts, above `ROW_MAJOR_MIN_ARTIFACTS` (1 000), and disjoint. By `layout::choose`'s
rule that level should be row-major.

[Decision 0094](../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
says the layout is chosen at the build and **re-evaluated at every fold**, so the candidate
explanation is that a freshly built level has no observed locality to choose from and lands
artifact-major until the first fold looks at it. This asks the question directly: read the layout
off the log, fold, read it again.

It measures nothing. The answer is a line in the server's log either way, and what it decides is how
the campaign's grid should be read — whether the enumerated twin's figures are the layout the
heuristic wants for it, or the one it has before a fold has run.

Run: `python3 layout_after_fold.py --work DIR`
"""

from __future__ import annotations

import argparse
import json
import re
import time
from pathlib import Path

import requests

import campaign as C

WHOLE_MAP = C.Viewport("100%", [0.0, 0.0, C.GRID, C.GRID], 0, 1.0)
LINE = re.compile(
    r"layer=(?P<layer>\S+) level=(?P<level>\d+) view=\S+ ordinals=(?P<ordinals>\d+) "
    r"everywhere=(?P<everywhere>\d+) adopted=(?P<adopted>\w+) layout=(?P<layout>\w+) "
    r"blocks_per_artifact=(?P<blocks>[\d.]+)"
)
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def layouts(log: Path, since: int) -> list[dict]:
    out = []
    for raw in log.read_text(errors="replace").splitlines()[since:]:
        match = LINE.search(ANSI.sub("", raw))
        if match:
            out.append(match.groupdict())
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--layers", nargs="*", default=[
        C.LAYER_FLAT, C.LAYER_PARTITION_ENUM, C.LAYER_PARTITION_ATTR,
        C.LAYER_BOUNDARY, C.LAYER_TREED,
    ])
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}
    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)
    server = C.Server(work, *ports)
    server.spawn()
    report: dict = {}
    try:
        mark = len(server.log.read_text(errors="replace").splitlines())
        token, _ = server.authorise(grants["0.9375"]["grant"].split(","))
        for layer in args.layers:
            C.warm(server, token, WHOLE_MAP, [layer])
        report["before_fold"] = layouts(server.log, mark)

        folds_before = server.status()["compaction"]["folds"]
        mark = len(server.log.read_text(errors="replace").splitlines())
        requests.post(
            f"{server.control_base}/control/compact",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=60,
        )
        deadline = time.monotonic() + 3600
        while time.monotonic() < deadline:
            if server.status()["compaction"]["folds"] > folds_before:
                break
            time.sleep(1.0)
        report["at_fold"] = layouts(server.log, mark)

        mark = len(server.log.read_text(errors="replace").splitlines())
        for layer in args.layers:
            C.warm(server, token, WHOLE_MAP, [layer])
        report["after_fold"] = layouts(server.log, mark)
    finally:
        server.stop()

    out = args.out or (work / "layout-after-fold.json")
    out.write_text(json.dumps(report, indent=2))
    for phase in ("before_fold", "at_fold", "after_fold"):
        print(f"--- {phase}")
        for row in report.get(phase, []):
            print(f"  {row['layer']:38s} layout={row['layout']:16s} "
                  f"blocks/artifact={float(row['blocks']):8.2f} ordinals={row['ordinals']}")
    print(f"-> {out}")


if __name__ == "__main__":
    main()
