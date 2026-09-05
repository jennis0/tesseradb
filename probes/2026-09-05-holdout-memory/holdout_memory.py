#!/usr/bin/env python3
"""Drive `HoldOut.batches` over a rung's points file and sample the driver's resident set.

The ingest cycle streams a rung's hold-out out of `points.parquet` as ingest batches. On rung 4
(52 GB of ZSTD parquet carrying abstracts, 718 row groups of 262,144 rows) the driver reached
38 GB of RSS by ~900 batches and both attribution cells stalled at the same row count. This probe
runs the driver's own reader, `test_corpora.common.ingest_cycle.HoldOut.batches` as committed,
with a consumer that discards each body, and writes one CSV line a second:

    t_s, batches, rows, vm_rss_kib, vm_hwm_kib, pool_allocated, pool_max, pool_backend

`pool_allocated` is Arrow's own count of live bytes in its default pool. If it stays flat while
`vm_rss_kib` climbs, the growth is memory the allocator holds after Arrow freed it; if it climbs
too, something still references the decoded data. `retain_bisect.py` beside this says which step.

    holdout_memory.py --rung <rung dir> --out <csv> [--pool system|jemalloc|mimalloc]
                      [--limit-batches N] [--held-cache <.npy>]

`--pool` is applied before pyarrow is imported (`ARROW_DEFAULT_MEMORY_POOL`) and again through
`pa.set_memory_pool`, so the Parquet reader, the compute kernels and the IPC writer all allocate
from the chosen backend.
"""

from __future__ import annotations

import argparse
import csv
import os
import sys
import threading
import time
from pathlib import Path

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("--rung", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--fraction", type=float, default=0.10)
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--pool", choices=["mimalloc", "jemalloc", "system"], default=None)
ap.add_argument("--limit-batches", type=int, default=0, help="stop after this many bodies; 0 is the whole file")
ap.add_argument("--held-cache", default=None, help="a .npy of the hold-out's entity ids, written on first use")
ap.add_argument("--interval", type=float, default=1.0)
args = ap.parse_args()

if args.pool:
    os.environ["ARROW_DEFAULT_MEMORY_POOL"] = args.pool

import numpy as np  # noqa: E402
import pyarrow as pa  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from test_corpora.common import ingest_cycle as ic  # noqa: E402

if args.pool:
    pools = {
        "mimalloc": pa.mimalloc_memory_pool,
        "jemalloc": pa.jemalloc_memory_pool,
        "system": pa.system_memory_pool,
    }
    pa.set_memory_pool(pools[args.pool]())

rung = Path(args.rung)
points = rung / "points.parquet"


def status() -> tuple[int, int]:
    rss = hwm = 0
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1])
        elif line.startswith("VmHWM:"):
            hwm = int(line.split()[1])
    return rss, hwm


progress = {"batches": 0, "rows": 0, "done": False}
t0 = time.monotonic()


def sampler(path: Path) -> None:
    pool = pa.default_memory_pool()
    with open(path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["t_s", "batches", "rows", "vm_rss_kib", "vm_hwm_kib", "pool_allocated", "pool_max", "pool_backend"])
        while True:
            rss, hwm = status()
            w.writerow([f"{time.monotonic() - t0:.1f}", progress["batches"], progress["rows"], rss, hwm,
                        pool.bytes_allocated(), pool.max_memory(), pool.backend_name])
            fh.flush()
            if progress["done"]:
                return
            time.sleep(args.interval)


Path(args.out).parent.mkdir(parents=True, exist_ok=True)
thread = threading.Thread(target=sampler, args=(Path(args.out),), daemon=True)
thread.start()

# The hold-out. Cached because the split reads the whole entity_id column and permutes it, which
# is 30 s and 2.5 GB on rung 4, and a run of this probe is about the reader and not the split.
if args.held_cache and Path(args.held_cache).exists():
    held = np.load(args.held_cache)
else:
    _, held = ic.split_entities(points, args.fraction, args.seed)
    if args.held_cache:
        np.save(args.held_cache, held)
pa.default_memory_pool().release_unused()
print(f"[{time.monotonic() - t0:.0f}s] hold-out {len(held):,} rows; pool {pa.default_memory_pool().backend_name}", flush=True)

hold = ic.HoldOut(rung, held, head_rows=0)
body_bytes = 0
try:
    for start, body, n in hold.batches():
        body_bytes += len(body)
        del body
        progress["batches"] += 1
        progress["rows"] += n
        if progress["batches"] % 50 == 0:
            rss, hwm = status()
            print(f"[{time.monotonic() - t0:.0f}s] {progress['batches']} batches, {progress['rows']:,} rows, "
                  f"RSS {rss / 2**20:.2f} GiB, HWM {hwm / 2**20:.2f} GiB, "
                  f"pool {pa.default_memory_pool().bytes_allocated() / 2**30:.2f} GiB", flush=True)
        if args.limit_batches and progress["batches"] >= args.limit_batches:
            break
finally:
    progress["done"] = True
    thread.join()
rss, hwm = status()
print(f"[{time.monotonic() - t0:.0f}s] done: {progress['batches']} batches, {progress['rows']:,} rows, "
      f"{body_bytes / 2**30:.2f} GiB of bodies, RSS {rss / 2**20:.2f} GiB, HWM {hwm / 2**20:.2f} GiB", flush=True)
