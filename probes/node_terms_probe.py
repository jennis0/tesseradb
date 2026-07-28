"""§7.6's unmeasured assumption: how many terms does a node draw on?

Design §7.6: "The assumption this section rests on is that a typical node
draws on a modest number of terms; it is unmeasured (§16)." It governs
how many candidate generating sets the caller's labeller must consider
and how long the fallback ladder runs — label *availability* and
labelling cost, not safety (containment stays exact either way).

High terms-per-item is the case that stresses it, which is why this runs
against the `hiterms` configs. A Morton tile stands in for a cluster
node: both are spatially coherent groups of items, and tiles are what we
actually have.

Reports, per depth: items per tile, distinct terms per tile, and the
ratio of distinct terms to items — the quantity that decides whether a
labeller sees hundreds of candidates or hundreds of thousands.

Usage: node_terms_probe.py <scaled_dir> <config> [--scale 10000000]
                           [--depths 8,10,12] [--tiles 200]
"""

import argparse
import time

import numpy as np
import pyarrow.parquet as pq

ap = argparse.ArgumentParser()
ap.add_argument("scaled")
ap.add_argument("config")
ap.add_argument("--scale", type=int, default=10_000_000)
ap.add_argument("--depths", default="8,10,12")
ap.add_argument("--tiles", type=int, default=200)
ap.add_argument("--seed", type=int, default=0)
args = ap.parse_args()

DEPTHS = [int(d) for d in args.depths.split(",")]
N = args.scale
t0 = time.perf_counter()


def log(m):
    print(f"[{time.perf_counter() - t0:6.1f}s] {m}", flush=True)


log(f"geometry for entity_id < {N:,}")
g = pq.read_table(f"{args.scaled}/geometry.parquet", columns=["entity_id", "morton"])
eid = g.column("entity_id").to_numpy()
keep = eid < N
eid, mort = eid[keep], g.column("morton").to_numpy()[keep]
del g
log(f"  {len(eid):,} rows")

# tile id per entity, for each depth
rng = np.random.default_rng(args.seed)
tile_of = {}
sampled = {}
for d in DEPTHS:
    t = (mort >> np.uint32(32 - 2 * d)).astype(np.int64)
    order = np.argsort(t, kind="stable")
    ts, es = t[order], eid[order]
    edges = np.flatnonzero(np.diff(ts)) + 1
    starts = np.concatenate(([0], edges))
    ends = np.concatenate((edges, [len(ts)]))
    pick = rng.choice(len(starts), min(args.tiles, len(starts)), replace=False)
    # entity -> sampled tile slot, else -1
    slot = np.full(N, -1, dtype=np.int32)
    for j, i in enumerate(pick):
        slot[es[starts[i]:ends[i]]] = j
    tile_of[d] = slot
    sampled[d] = [int(ends[i] - starts[i]) for i in pick]
    del t, order, ts, es
log("tiles sampled; streaming pairs")

# distinct terms per sampled tile, via per-tile term sets
seen = {d: [set() for _ in range(len(sampled[d]))] for d in DEPTHS}
pf = pq.ParquetFile(f"{args.scaled}/pairs/{args.config}.pairs.parquet")
for b in pf.iter_batches(batch_size=16_000_000, columns=["entity_id", "term_id"]):
    e = b.column("entity_id").to_numpy()
    m = e < N
    if not m.any():
        continue
    e = e[m]
    tid = b.column("term_id").to_numpy()[m]
    for d in DEPTHS:
        s = tile_of[d][e]
        hit = s >= 0
        if not hit.any():
            continue
        sh, th = s[hit], tid[hit]
        o = np.argsort(sh, kind="stable")
        sh, th = sh[o], th[o]
        edges = np.flatnonzero(np.diff(sh)) + 1
        for lo, hi in zip(np.concatenate(([0], edges)),
                          np.concatenate((edges, [len(sh)]))):
            seen[d][int(sh[lo])].update(th[lo:hi].tolist())

print(f"\n=== {args.config} @ {N:,} — terms per tile (node proxy) ===")
print(f"{'depth':>6} {'tiles':>6} {'items/tile med':>15} {'terms/tile med':>15} "
      f"{'p95':>10} {'terms/item':>11}")
for d in DEPTHS:
    items = np.array(sampled[d])
    terms = np.array([len(x) for x in seen[d]])
    ok = items > 0
    print(f"{d:>6} {ok.sum():>6} {int(np.median(items[ok])):>15,} "
          f"{int(np.median(terms[ok])):>15,} {int(np.percentile(terms[ok], 95)):>10,} "
          f"{np.median(terms[ok] / items[ok]):>11.2f}")
log("done")
