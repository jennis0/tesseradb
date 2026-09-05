#!/usr/bin/env python3
"""Which step of the hold-out read retains memory: five loops over the same row groups.

    retain_bisect.py --rung <rung dir> --held <held .npy> --variant A|B|C|D|E [--row-groups 30]

    A  `ParquetFile.iter_batches(batch_size=2^17)`, every batch dropped at once
    B  A, plus `Table.from_batches` and `filter` by the hold-out, the result dropped
    C  `ParquetFile.read_row_group(i)` and the same filter, both dropped
    D  B, plus `encode_batch` on the kept rows, the body dropped
    E  B, with every filtered table deliberately retained, as a scale for what kept rows cost

Prints Arrow's live pool bytes and the process's `VmRSS` every ten row groups. A variant whose
pool figure climbs is holding decoded data; one whose RSS climbs while the pool does not is
allocator retention. On rung 4's points file A climbs by ~150 MB a row group and C is flat, which
places the retention in pyarrow's batch reader and nowhere in the driver.
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from test_corpora.common import ingest_cycle as ic  # noqa: E402

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("--rung", required=True)
ap.add_argument("--held", required=True, help="the hold-out's entity ids, a .npy (holdout_memory.py --held-cache writes one)")
ap.add_argument("--variant", choices=list("ABCDE"), required=True)
ap.add_argument("--row-groups", type=int, default=30)
args = ap.parse_args()

rung = Path(args.rung)
held = np.load(args.held)
access, attributes = ic.wire_columns(rung)
pool = pa.default_memory_pool()
reader = pq.ParquetFile(rung / "points.parquet")
N = args.row_groups
t0 = time.monotonic()


def rss_gib() -> float:
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 2**20
    return 0.0


def report(i: int) -> None:
    if (i + 1) % 10 == 0:
        print(f"  rg {i + 1}: pool {pool.bytes_allocated() / 2**30:.2f} GiB rss {rss_gib():.2f} GiB t={time.monotonic() - t0:.0f}s", flush=True)


def mask_of(table) -> pa.Array:
    return pa.array(ic.in_sorted(table.column("entity_id").to_numpy(), held))


print(f"variant {args.variant}, pool {pool.backend_name}, {N} row groups of {rung / 'points.parquet'}")
if args.variant == "A":
    i = 0
    for batch in reader.iter_batches(batch_size=1 << 17):
        if i % 2 == 1:
            report(i // 2)
        i += 1
        if i >= 2 * N:
            break
elif args.variant in "BDE":
    kept = []
    i = 0
    for batch in reader.iter_batches(batch_size=1 << 17):
        table = pa.Table.from_batches([batch]).filter(mask_of(batch))
        if args.variant == "D":
            body = ic.encode_batch(table, access, attributes)
            del body
        if args.variant == "E":
            kept.append(table)
        del table
        if i % 2 == 1:
            report(i // 2)
        i += 1
        if i >= 2 * N:
            break
    if kept:
        print(f"  retained {sum(k.num_rows for k in kept):,} rows, {sum(k.nbytes for k in kept) / 2**30:.2f} GiB by nbytes")
elif args.variant == "C":
    for i in range(N):
        group = reader.read_row_group(i)
        table = group.filter(mask_of(group))
        del group, table
        report(i)
print(f"end: pool {pool.bytes_allocated() / 2**30:.2f} GiB, rss {rss_gib():.2f} GiB")
