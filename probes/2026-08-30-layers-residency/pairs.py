#!/usr/bin/env python3
"""Count the member pairs a `key`-list Parquet declares, per artifact.

    python3 pairs.py data/ladder/overture/members-taxonomy.parquet

**A null element is not an artifact.** `key` is a list per row — one element per ladder level —
and a null element means the point is in no artifact at that level: counted as unclustered by the
build, never pushed to the member spill. `pc.value_counts` returns null as its own bucket, and on
Overture's taxonomy that bucket is 51.8% of the entries, so a reading that keeps it reports one
artifact holding half the corpus and sends the next person after a term that does not exist. This
happened on 2026-08-30 — see the README. Nulls are reported separately below and never ranked.
"""
import collections
import sys

import pyarrow.compute as pc
import pyarrow.parquet as pq

path = sys.argv[1]
f = pq.ParquetFile(path)
counts = collections.Counter()
rows = nulls = 0
for i in range(f.metadata.num_row_groups):
    col = f.read_row_group(i, columns=["key"]).column("key").combine_chunks()
    rows += len(col)
    flat = pc.list_flatten(col)
    nulls += flat.null_count
    vc = pc.value_counts(flat)
    for s in vc:
        key = s["values"].as_py()
        if key is not None:
            counts[key] += s["counts"].as_py()

pairs = sum(counts.values())
ranked = sorted(counts.values())
print(path)
print(f"  rows {rows:,}   entries {pairs + nulls:,}   pairs {pairs:,}   null {nulls:,} "
      f"({100 * nulls / (pairs + nulls):.1f}% — unclustered, not an artifact)")
print(f"  artifacts {len(counts):,}   median {ranked[len(ranked) // 2]:,}   "
      f"largest {ranked[-1]:,} ({100 * ranked[-1] / pairs:.1f}%)")
print(f"  top5 {[(k, f'{v:,}') for k, v in counts.most_common(5)]}")
