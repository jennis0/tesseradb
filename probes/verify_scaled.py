"""Verify the scaled corpus and exercise the core primitive at 10^9.

Three things:
  1. the 2,422,486 prefix is the real data, unchanged (Morton codes
     identical to the hashed geometry artifact, same rank order);
  2. a scale's row_id really is the running position under the prefix
     filter (checked against an independent re-sort at 250k and 2.4M);
  3. an end-to-end masked viewport at the full scale — mask build in
     entity space, permutation into row space, then per-tile
     `range_cardinality` over real Morton tiles. This is §2.6 steps
     4-6 at the design's target size.

Usage: verify_scaled.py <scaled_dir> <base_geometry.parquet>
"""

import json
import sys
import time

import numpy as np
import pyarrow.compute as pc
import pyarrow.parquet as pq
from pyroaring import BitMap

SCALED, BASE = sys.argv[1], sys.argv[2]
meta = json.load(open(f"{SCALED}/scales.json"))
N = meta["points"]
t0 = time.perf_counter()


def log(m):
    print(f"[{time.perf_counter() - t0:6.1f}s] {m}", flush=True)


# ---------------------------------------------- 1 & 2: prefix fidelity
log("reading scaled geometry (entity_id, morton)")
g = pq.read_table(f"{SCALED}/geometry.parquet", columns=["entity_id", "morton"])
s_eid = g.column("entity_id").to_numpy()
s_mort = g.column("morton").to_numpy()
del g
log(f"  {len(s_eid):,} rows in memory")

b = pq.read_table(BASE, columns=["entity_id", "morton"])
b_eid, b_mort = b.column("entity_id").to_numpy(), b.column("morton").to_numpy()
n_base = len(b_eid)
del b

keep = s_eid < n_base
sub_eid, sub_mort = s_eid[keep], s_mort[keep]
o_s, o_b = np.argsort(sub_eid), np.argsort(b_eid)
print("\n--- 1. the real-data prefix ---")
print(f"  rows: {keep.sum():,} (expected {n_base:,})")
print(f"  Morton codes identical to the hashed artifact: "
      f"{np.array_equal(sub_mort[o_s], b_mort[o_b])}")
print(f"  rank order identical: "
      f"{np.array_equal(np.argsort(np.argsort(sub_eid)), np.argsort(np.argsort(b_eid)))}")

print("\n--- 2. derived row_id == independent re-sort ---")
for limit in (250_000, n_base):
    k = s_eid < limit
    e, m = s_eid[k], s_mort[k]
    # independent: re-sort this subset from scratch by (morton, entity)
    fresh = np.lexsort((e, m))
    print(f"  entity_id < {limit:>10,}: {k.sum():>12,} rows, "
          f"derived order == re-sorted: {np.array_equal(fresh, np.arange(len(e)))}")

# ------------------------------------- 3. masked viewport at full scale
print("\n--- 3. masked viewport at the full scale ---")
log("building entity->row permutation")
ent_to_row = np.empty(N, dtype=np.uint32)
ent_to_row[s_eid] = np.arange(N, dtype=np.uint32)   # rows are Morton-ranked
del s_eid

log("reading global-term postings (term_id < 60)")
pt = pq.read_table(f"{SCALED}/pairs.parquet",
                   filters=[("term_id", "<", 12)], columns=["entity_id", "term_id"])
p_ent = pt.column("entity_id").to_numpy()
p_tid = pt.column("term_id").to_numpy()
del pt
log(f"  {len(p_ent):,} pairs over {len(np.unique(p_tid))} global terms")

order = np.argsort(p_tid, kind="stable")
p_ent, p_tid = p_ent[order], p_tid[order]
bounds = np.searchsorted(p_tid, np.unique(p_tid))
uniq = np.unique(p_tid)
postings = {}
for i, t in enumerate(uniq):
    lo = bounds[i]
    hi = bounds[i + 1] if i + 1 < len(bounds) else len(p_ent)
    postings[int(t)] = BitMap(p_ent[lo:hi])
del p_ent, p_tid, order

grant = list(postings)[:6]
t = time.perf_counter()
mask_e = BitMap.union(*(postings[g] for g in grant))
t_union = (time.perf_counter() - t) * 1000
print(f"  mask (entity space): {len(mask_e):,} items "
      f"({100 * len(mask_e) / N:.2f}% coverage), union {t_union:.0f} ms, "
      f"{len(mask_e.serialize()) / 1e6:.1f} MB serialised")

t = time.perf_counter()
rows = np.sort(ent_to_row[np.array(mask_e, dtype=np.uint32)])
mask_r = BitMap(rows)
t_perm = (time.perf_counter() - t) * 1000
print(f"  permuted into row space: {t_perm:.0f} ms, "
      f"{len(mask_r.serialize()) / 1e6:.1f} MB serialised")

# a viewport = a few hundred tiles at some depth; tiles are Morton
# prefixes, hence contiguous row ranges. Find each tile's [lo, hi).
print(f"  {'depth':>5} {'tiles':>7} {'rows/tile':>12} {'visible':>13} "
      f"{'total ms':>9} {'µs/tile':>9}")
for depth in (0, 2, 4, 6, 8, 10, 12):
    if depth == 0:
        first, last, occupied = np.array([0]), np.array([N]), np.array([0])
    else:
        shift = 32 - 2 * depth
        tiles = s_mort >> np.uint32(shift)        # sorted ascending
        first = np.searchsorted(tiles, np.arange(4**depth), side="left")
        last = np.searchsorted(tiles, np.arange(4**depth), side="right")
        occupied = np.flatnonzero(last > first)
        del tiles
    # a viewport is a few hundred tiles; at coarse depths the whole map
    # is fewer than that, so take every occupied tile there
    sample = occupied[:: max(1, len(occupied) // 300)][:300]
    spans = (last[sample] - first[sample])
    ranges = [(int(first[ti]), int(last[ti])) for ti in sample]
    dt = None
    for _ in range(5):                            # min of 5, not one shot
        t = time.perf_counter()
        total = 0
        for lo_, hi_ in ranges:
            total += mask_r.range_cardinality(lo_, hi_)
        dt = min(dt or 9e9, (time.perf_counter() - t) * 1000)
    print(f"  {depth:>5} {len(sample):>7,} {int(spans.mean()):>12,} {total:>13,} "
          f"{dt:>9.2f} {1000 * dt / len(sample):>9.1f}")
    del first, last
log("done")
