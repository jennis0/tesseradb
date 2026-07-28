"""Stage-2 measurement: mask spatial autocorrelation under Morton order.

For each grant scenario (same construction and seed as mask_probe.py),
permute the entity-space mask into row space via geometry.parquet and
measure:

  run ratio   mean run length of the mask in row order, divided by the
              1/(1-p) expectation for a random mask of the same density.
              ~1.0 = scattered (candidate lists dead, direct evaluation
              is the path); >>1 = authorisation correlates with position.
  ser E/R     serialised Roaring size in entity vs row space — compression
              is autocorrelation made visible.
  tile coverage at depths 4/6/8 (256 / 4,096 / 65,536 tiles): share of
              mask-holding tiles below the ~5% direct-eval crossover, and
              the median coverage of occupied tiles.

Usage: stage2_probe.py <geometry.parquet> <pairs.parquet> [--seed 0]
"""

import argparse
from pathlib import Path

import duckdb
import numpy as np
from pyroaring import BitMap

N_CORPUS = 2_422_486
COVERAGE_TARGETS = [0.25, 0.05, 0.01, 0.0001]
WIDTH_TARGETS = [100, 1_000, 10_000]
DEPTHS = [4, 6, 8]

ap = argparse.ArgumentParser()
ap.add_argument("geometry")
ap.add_argument("pairs")
ap.add_argument("--seed", type=int, default=0)
args = ap.parse_args()

con = duckdb.connect()
geo = con.execute(f"SELECT entity_id, morton FROM read_parquet('{args.geometry}') ORDER BY row_id").fetchnumpy()
entity_to_row = np.empty(N_CORPUS, dtype=np.uint32)
entity_to_row[geo["entity_id"]] = np.arange(N_CORPUS, dtype=np.uint32)
tile_pop = {d: np.bincount(geo["morton"] >> (32 - 2 * d), minlength=4**d) for d in DEPTHS}

tid_col, eid_col = (con.execute(
    f"SELECT term_id, entity_id FROM read_parquet('{args.pairs}') ORDER BY term_id, entity_id")
    .fetchnumpy().values())
bounds = np.searchsorted(tid_col, np.unique(tid_col))
uniq = tid_col[bounds]
posting_arr = {}
for i, t in enumerate(uniq):
    lo, hi = bounds[i], bounds[i + 1] if i + 1 < len(bounds) else len(eid_col)
    posting_arr[t] = eid_col[lo:hi]
sizes = sorted(posting_arr, key=lambda t: (-len(posting_arr[t]), t))

rng = np.random.default_rng(args.seed)
scenarios = []
for c in COVERAGE_TARGETS:
    acc, grant = BitMap(), []
    for t in sizes:
        grant.append(t)
        acc |= BitMap(posting_arr[t])
        if len(acc) >= c * N_CORPUS:
            break
    scenarios.append((f"head c~{c:g}", grant))
for w in WIDTH_TARGETS:
    if w <= len(sizes):
        scenarios.append((f"random w={w}", list(rng.choice(np.array(sizes), w, replace=False))))

print(f"== {Path(args.pairs).stem} over {Path(args.geometry).stem} ==")
hdr = f"{'scenario':<16} {'cover%':>7} {'runlen':>7} {'ratio':>6} {'serE MB':>8} {'serR MB':>8}"
for d in DEPTHS:
    hdr += f"  d{d}:<5%/med%"
print(hdr)
seen = set()
for label, grant in scenarios:
    ents = np.unique(np.concatenate([posting_arr[t] for t in grant]))
    key = (len(ents), int(ents[:50].sum()))
    if key in seen:  # degenerate duplicates (head targets below smallest head term)
        continue
    seen.add(key)
    p = len(ents) / N_CORPUS
    if p > 0.999:  # full-coverage degenerate (e.g. w = whole vocabulary): no information
        continue
    rows = np.sort(entity_to_row[ents])
    runs = 1 + int((np.diff(rows) > 1).sum())
    runlen = len(rows) / runs
    ratio = runlen / (1.0 / (1.0 - p))
    ser_e = len(BitMap(ents).serialize()) / 1e6
    ser_r = len(BitMap(rows).serialize()) / 1e6
    line = f"{label:<16} {100 * p:>7.3f} {runlen:>7.2f} {ratio:>6.2f} {ser_e:>8.2f} {ser_r:>8.2f}"
    for d in DEPTHS:
        cnt = np.bincount(geo["morton"][rows] >> (32 - 2 * d), minlength=4**d)
        occ = cnt > 0
        cov = cnt[occ] / tile_pop[d][occ]
        below = 100 * (cov < 0.05).sum() / occ.sum()
        line += f"  {below:5.1f}/{100 * np.median(cov):5.2f}"
    print(line)
