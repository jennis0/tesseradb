"""Stage-1 measurement 3: grant grid + mask build over a pair relation.

Grant sets are (seed, config)-deterministic and regenerated, never stored.
Two families per pairs file:

  head    coverage-targeted: union largest postings until coverage target
          -> width emerges (the head-heavy principal)
  random  width-targeted: w terms uniform over the vocabulary
          -> coverage emerges (the tail-heavy principal)

Two build paths per grant set, timed separately:

  semi-join   DuckDB hash semi-join over the exploded pairs -> sorted
              entity array -> BitMap        (the plan 4.2 measurement)
  postings    union of precomputed per-term BitMaps          (the 6.3
              serving design; postings built once, untimed)

--tile R replicates the relation R times with entity_id offset k*N —
term space shared, postings *R. Valid ONLY for build timing and mask
sizes; signature or autocorrelation numbers over tiled data are artefacts.

Usage: mask_probe.py <pairs.parquet> [--seed 0] [--reps 3] [--tile R]
"""

import argparse
import time
from pathlib import Path

import duckdb
import numpy as np
from pyroaring import BitMap

N_BASE = 2_422_486
COVERAGE_TARGETS = [0.25, 0.05, 0.01, 0.0001]
WIDTH_TARGETS = [100, 1_000, 10_000]

ap = argparse.ArgumentParser()
ap.add_argument("pairs")
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--reps", type=int, default=3)
ap.add_argument("--tile", type=int, default=1)
args = ap.parse_args()

N_CORPUS = N_BASE * args.tile
assert N_CORPUS < 2**32

con = duckdb.connect()
if args.tile == 1:
    con.execute(f"CREATE VIEW pairs AS SELECT * FROM read_parquet('{args.pairs}')")
else:
    con.execute(f"""
    CREATE TABLE pairs AS
    SELECT CAST(entity_id + r * {N_BASE} AS UINTEGER) AS entity_id, term_id
    FROM read_parquet('{args.pairs}') CROSS JOIN range({args.tile}) t(r)""")

sizes = con.execute(
    "SELECT term_id, count(*) n FROM pairs GROUP BY 1 ORDER BY n DESC, term_id").fetchnumpy()
term_by_size, vocab = sizes["term_id"], len(sizes["term_id"])

t0 = time.perf_counter()
postings = {}
tid_col, eid_col = (con.execute(
    f"SELECT term_id, entity_id FROM read_parquet('{args.pairs}') ORDER BY term_id, entity_id")
    .fetchnumpy().values())
offsets = (np.arange(args.tile, dtype=np.int64) * N_BASE)[:, None]
bounds = np.searchsorted(tid_col, np.unique(tid_col))
uniq = tid_col[bounds]
for i, t in enumerate(uniq):
    lo, hi = bounds[i], bounds[i + 1] if i + 1 < len(bounds) else len(eid_col)
    base = eid_col[lo:hi].astype(np.int64)
    postings[t] = BitMap(base if args.tile == 1 else (base[None, :] + offsets).ravel())
print(f"{Path(args.pairs).stem} x{args.tile} = {N_CORPUS:,} items: {vocab:,} terms, "
      f"postings built in {time.perf_counter() - t0:.1f}s (setup, untimed below)")

rng = np.random.default_rng(args.seed)
scenarios = []
for c in COVERAGE_TARGETS:  # head: greedy largest-first until coverage
    acc, grant = BitMap(), []
    for t in term_by_size:
        grant.append(t)
        acc |= postings[t]
        if len(acc) >= c * N_CORPUS:
            break
    scenarios.append((f"head c~{c:g}", np.array(grant)))
for w in WIDTH_TARGETS:  # random: fixed width, coverage emerges
    if w <= vocab:
        scenarios.append((f"random w={w}", rng.choice(term_by_size, w, replace=False)))

print(f"{'scenario':<16} {'w':>7} {'cover%':>8} {'|mask|':>10} "
      f"{'join ms':>8} {'bmp ms':>7} {'union ms':>9} {'ser MB':>7} {'w/V':>7}")
for label, grant in scenarios:
    con.execute("CREATE OR REPLACE TABLE grants AS SELECT unnest(?::UINTEGER[]) term_id",
                [grant.tolist()])
    join_ms = bmp_ms = union_ms = None
    for _ in range(args.reps):
        t0 = time.perf_counter()
        arr = con.execute(
            "SELECT DISTINCT entity_id FROM pairs SEMI JOIN grants USING (term_id)"
        ).fetchnumpy()["entity_id"]
        t1 = time.perf_counter()
        mask = BitMap(arr)
        t2 = time.perf_counter()
        join_ms = min(join_ms or 9e9, (t1 - t0) * 1000)
        bmp_ms = min(bmp_ms or 9e9, (t2 - t1) * 1000)
    for _ in range(args.reps):
        t0 = time.perf_counter()
        mask_u = BitMap.union(*(postings[t] for t in grant))
        union_ms = min(union_ms or 9e9, (time.perf_counter() - t0) * 1000)
    assert mask == mask_u, f"{label}: build paths disagree"
    ser = len(mask.serialize()) / 1e6
    print(f"{label:<16} {len(grant):>7,} {100 * len(mask) / N_CORPUS:>8.3f} {len(mask):>10,} "
          f"{join_ms:>8.1f} {bmp_ms:>7.1f} {union_ms:>9.2f} {ser:>7.2f} {len(grant) / vocab:>7.4f}")
