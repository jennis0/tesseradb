"""Measurement 3 (mask build) at the scaled corpus's larger scales.

Supersedes `mask_probe.py --tile N` for the 250M/1B cases: that hack
replicated identical postings, so every term had N identical blocks.
Here the vocabulary is real — 60 corpus-spanning *global* terms plus
413x116 *replica-local* ones — so head grants produce huge diffuse
masks and random grants small clustered ones, and the (width, coverage)
grid means something again.

Postings are built by streaming parquet row groups: the 1.72B-pair
relation never lands in memory at once. Only the Roaring postings stay
resident (a few GB at 10^9).

Grant families match mask_probe.py: *head* unions the largest postings
until a coverage target (width emerges); *random* takes w uniform terms
(coverage emerges). The union path is timed; the DuckDB semi-join —
plan §4.2's formulation, which measurement reassigned to build cadence —
is timed for a couple of scenarios with --semijoin, since at this size
it is minutes, not milliseconds.

Usage: mask_probe_scaled.py <scaled_dir> --scale N [--semijoin] [--reps 3]
"""

import argparse
import json
import time

import numpy as np
import pyarrow.parquet as pq
from pyroaring import BitMap

COVERAGE_TARGETS = [0.25, 0.05, 0.01, 0.0001]
WIDTH_TARGETS = [100, 1_000, 10_000]

ap = argparse.ArgumentParser()
ap.add_argument("scaled")
ap.add_argument("--scale", type=int, default=1_000_000_000)
ap.add_argument("--scales", default=None,
                help="comma-separated entity limits; postings are built once "
                     "at the largest and restricted per scale, so the curve "
                     "isolates scale with everything else held constant")
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--reps", type=int, default=3)
ap.add_argument("--semijoin", action="store_true")
ap.add_argument("--pairs", default="categories-subclass")
args = ap.parse_args()

meta = json.load(open(f"{args.scaled}/scales.json"))
SCALES = ([int(x) for x in args.scales.split(",")] if args.scales
          else [args.scale])
N = max(SCALES)
t0 = time.perf_counter()


def log(m):
    print(f"[{time.perf_counter() - t0:7.1f}s] {m}", flush=True)


# --------------------------------------------- postings, streamed
log(f"building postings for entity_id < {N:,}")
pf = pq.ParquetFile(f"{args.scaled}/pairs/{args.pairs}.pairs.parquet")
postings = {}
seen_pairs = 0
for batch in pf.iter_batches(batch_size=8_000_000, columns=["entity_id", "term_id"]):
    ent = batch.column("entity_id").to_numpy()
    tid = batch.column("term_id").to_numpy()
    keep = ent < N
    if not keep.any():
        continue
    ent, tid = ent[keep], tid[keep]
    seen_pairs += len(ent)
    o = np.argsort(tid, kind="stable")
    ent, tid = ent[o], tid[o]
    edges = np.flatnonzero(np.diff(tid)) + 1
    for lo, hi in zip(np.concatenate(([0], edges)),
                      np.concatenate((edges, [len(tid)]))):
        t = int(tid[lo])
        b = postings.get(t)
        if b is None:
            postings[t] = BitMap(ent[lo:hi])
        else:
            b |= BitMap(ent[lo:hi])
log(f"  {seen_pairs:,} pairs -> {len(postings):,} terms with postings")

sizes = sorted(postings, key=lambda t: (-len(postings[t]), t))
big = len(postings[sizes[0]])
log(f"  largest posting {big:,} ({100 * big / N:.2f}% of corpus); "
    f"smallest {len(postings[sizes[-1]]):,}")

# ------------------------------------------------------- grant grid
def containers(bm):
    """Approximate container count: distinct high-16-bit keys."""
    a = np.asarray(bm, dtype=np.uint32)
    return len(np.unique(a >> 16)) if len(a) else 0


rng = np.random.default_rng(args.seed)
print(f"\n=== {args.pairs} ===")
print(f"{'scale':>14} {'scenario':<16} {'w':>7} {'cover%':>8} {'|mask|':>14} "
      f"{'containers':>11} {'union ms':>10} {'ser MB':>8}")
for S in sorted(SCALES):
    if S < N:
        pos = {}
        for t, b in postings.items():
            c = b.copy()
            c.remove_range(S, 2**32)
            if len(c):
                pos[t] = c
    else:
        pos = postings
    sizes = sorted(pos, key=lambda t: (-len(pos[t]), t))
    scenarios = []
    for cov in COVERAGE_TARGETS:
        acc, grant = BitMap(), []
        for t in sizes:
            grant.append(t)
            acc |= pos[t]
            if len(acc) >= cov * S:
                break
        scenarios.append((f"head c~{cov:g}", grant))
    for w in WIDTH_TARGETS:
        if w <= len(sizes):
            scenarios.append((f"random w={w}",
                              list(rng.choice(np.array(sizes), w, replace=False))))
    seen = set()
    for label, grant in scenarios:
        ms = None
        for _ in range(args.reps):
            t0_ = time.perf_counter()
            mask = BitMap.union(*(pos[int(g)] for g in grant))
            ms = min(ms or 9e9, (time.perf_counter() - t0_) * 1000)
        key = (len(mask), int(grant[0]))
        if key in seen:
            continue
        seen.add(key)
        cont = sum(containers(pos[int(g)]) for g in grant)
        print(f"{S:>14,} {label:<16} {len(grant):>7,} {100 * len(mask) / S:>8.3f} "
              f"{len(mask):>14,} {cont:>11,} {ms:>10.1f} "
              f"{len(mask.serialize()) / 1e6:>8.2f}")
    if S < N:
        del pos

# ------------------------------------------------- semi-join comparison
if args.semijoin:
    import duckdb
    con = duckdb.connect()
    print("\n--- plan §4.2 formulation (DuckDB hash semi-join over the pair table) ---")
    for label, grant in scenarios[:1] + scenarios[-1:]:
        con.execute("CREATE OR REPLACE TABLE g AS SELECT unnest(?::UINTEGER[]) term_id",
                    [[int(x) for x in grant]])
        t = time.perf_counter()
        n = con.execute(f"""
            SELECT count(DISTINCT entity_id) FROM
              read_parquet('{args.scaled}/pairs.parquet')
              SEMI JOIN g USING (term_id)
            WHERE entity_id < {N}""").fetchone()[0]
        dt = (time.perf_counter() - t) * 1000
        print(f"  {label:<16} w={len(grant):>6,}  {n:>14,} items  {dt:>10,.0f} ms")
log("done")
