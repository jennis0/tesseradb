#!/usr/bin/env python3
"""The publication's grouping pass alone: bodies assembled and counted, none sent.

Runs `test_corpora.common.ingest_cycle.Publication` over one declared layer of a rung exactly as
the ingest cycle does, with the consumer replaced by a counter, and samples the process's resident
set and the transient bucket files' size once a second. What it measures is the driver's own cost
of reading a member table in artifact order once: the counting pass, the partitioning pass, the
bucket reads and the body assembly, with their peak memory and peak disk.

    grouping_pass.py --rung <rung dir> --layer <name> --work <dir> --out <json> [--rss-csv <csv>]
                     [--bucket-rows N] [--max-bytes N]

`--work` receives the layer's buckets while the pass runs and is emptied when it ends.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from test_corpora.common import ingest_cycle as ic  # noqa: E402

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("--rung", required=True)
ap.add_argument("--layer", required=True)
ap.add_argument("--work", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--rss-csv", default=None)
ap.add_argument("--bucket-rows", type=int, default=16_000_000)
ap.add_argument("--max-bytes", type=int, default=32 * 1024 * 1024)
ap.add_argument("--interval", type=float, default=1.0)
args = ap.parse_args()

rung = Path(args.rung)
layer = next((l for l in ic.declared_layers(rung) if l["name"] == args.layer), None)
if layer is None or layer["roster"] is None:
    raise SystemExit(f"{args.layer}: not a declared layer of {rung} with a roster")
work = Path(args.work) / args.layer.replace("/", "__")
work.mkdir(parents=True, exist_ok=True)


def status() -> tuple[int, int]:
    rss = hwm = 0
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1])
        elif line.startswith("VmHWM:"):
            hwm = int(line.split()[1])
    return rss, hwm


def work_bytes() -> int:
    total = 0
    try:
        with os.scandir(work) as it:
            for entry in it:
                total += entry.stat().st_size
    except FileNotFoundError:
        pass
    return total


progress = {"bodies": 0, "artifacts": 0, "members": 0, "bytes": 0, "done": False}
peak_work = {"bytes": 0}
t0 = time.monotonic()


def sampler(path: Path | None) -> None:
    fh = open(path, "w", newline="") if path else None
    w = csv.writer(fh) if fh else None
    if w:
        w.writerow(["t_s", "bodies", "artifacts", "members", "vm_rss_kib", "vm_hwm_kib", "work_bytes"])
    while True:
        rss, hwm = status()
        disk = work_bytes()
        peak_work["bytes"] = max(peak_work["bytes"], disk)
        if w:
            w.writerow([f"{time.monotonic() - t0:.1f}", progress["bodies"], progress["artifacts"], progress["members"], rss, hwm, disk])
            fh.flush()
        if progress["done"]:
            break
        time.sleep(args.interval)
    if fh:
        fh.close()


thread = threading.Thread(target=sampler, args=(Path(args.rss_csv) if args.rss_csv else None,), daemon=True)
thread.start()

publication = ic.Publication(layer["roster"], layer["members"], work, args.max_bytes, args.bucket_rows)
print(f"[{time.monotonic() - t0:.0f}s] {args.layer}: {len(publication.rows):,} artifacts in the roster, "
      f"member file {layer['members'].name}", flush=True)
edges = 0
wall0 = time.perf_counter()
try:
    for level, body, artifacts, members, body_edges in publication.bodies():
        progress["bodies"] += 1
        progress["artifacts"] += artifacts
        progress["members"] += members
        progress["bytes"] += len(body)
        edges += body_edges
        del body
        if progress["bodies"] % 25 == 0:
            rss, hwm = status()
            print(f"[{time.monotonic() - t0:.0f}s] {progress['bodies']} bodies, {progress['artifacts']:,} artifacts, "
                  f"{progress['members']:,} members, RSS {rss / 2**20:.2f} GiB, HWM {hwm / 2**20:.2f} GiB, "
                  f"work {work_bytes() / 2**30:.2f} GiB", flush=True)
finally:
    wall = time.perf_counter() - wall0
    publication.cleanup()
    progress["done"] = True
    thread.join()

rss, hwm = status()
stats = dict(publication.stats)
declined = stats.pop("declined_artifacts")
result = {
    "rung": rung.name,
    "layer": args.layer,
    "member_file": layer["members"].name,
    "bucket_rows": args.bucket_rows,
    "max_bytes": args.max_bytes,
    "route_max_body_bytes": ic.ROUTE_MAX_BODY_BYTES,
    "wall_s": round(wall, 1),
    "peak_rss_bytes": hwm * 1024,
    "peak_work_bytes": peak_work["bytes"],
    "bodies": progress["bodies"],
    "body_bytes": progress["bytes"],
    "artifacts_assembled": progress["artifacts"],
    "members_assembled": progress["members"],
    "edges_assembled": edges,
    "declined_artifacts_n": len(declined),
    "declined_members": sum(d["members"] for d in declined),
    "declined_artifacts": declined,
    **stats,
}
Path(args.out).parent.mkdir(parents=True, exist_ok=True)
Path(args.out).write_text(json.dumps(result, indent=2))
print(f"[{time.monotonic() - t0:.0f}s] done: {progress['bodies']} bodies, {progress['artifacts']:,} artifacts, "
      f"{progress['members']:,} members, {progress['bytes'] / 2**30:.2f} GiB of bodies; {len(declined)} declined "
      f"({sum(d['members'] for d in declined):,} members); {stats['read_path']}, buckets {stats['buckets']}, "
      f"count {stats['count_s']} s, partition {stats['partition_s']} s, wall {wall:.0f} s, "
      f"peak RSS {hwm / 2**20:.2f} GiB, peak work {peak_work['bytes'] / 2**30:.2f} GiB; wrote {args.out}", flush=True)
