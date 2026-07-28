"""Dictionary-scale probe: mask build against a 10^7-10^8-term vocabulary.

`mask_probe_scaled.py` holds a BitMap per term, which is fine at 10^4-10^5
terms and impossible at 10^8 (Python object plus container overhead is
~250 B/term, so 117M terms is 40-55 GB before a single posting is
stored). That is a probe limitation, not an architectural one: the design
stores terms below a few hundred members as plain sorted int32 arrays
(§6.2), which at 4.37B pairs is ~17.5 GB of postings plus ~1.3 GB of
offsets — inside Appendix A's ~20 GB term-index budget at 10^9.

This probe follows the serving shape instead of the probe-convenient one:
**a grant set is at most 10^4 terms, so only granted postings are ever
materialised.** Pass 1 streams the pair relation to get term sizes (a
bincount, not a dictionary); pass 2 extracts postings for the union of
all scenarios' granted terms. Nothing else is held.

The **fold** knob varies dictionary scale while holding pairs, entity
coverage and spatial footprint exactly constant — F consecutive replicas
share one local vocabulary:

    F=12 -> 10.0M terms    F=3 -> 39.2M    F=2 -> 58.7M    F=1 -> 116.9M

Usage: mask_probe_surnames.py <scaled_dir> [--scale N] [--fold F] [--reps 3]
"""

import argparse
import json
import time

import numpy as np
import pyarrow.parquet as pq
from pyroaring import BitMap

COVERAGE_TARGETS = [0.25, 0.05, 0.01]
WIDTH_TARGETS = [100, 1_000, 10_000]

ap = argparse.ArgumentParser()
ap.add_argument("scaled")
ap.add_argument("--config", default="surnames")
ap.add_argument("--scale", type=int, default=1_000_000_000)
ap.add_argument("--fold", type=int, default=1)
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--reps", type=int, default=3)
args = ap.parse_args()

meta = json.load(open(f"{args.scaled}/pairs/{args.config}.json"))
n_g = meta["global_terms"]
n_l = meta["local_terms_per_replica"]
N = args.scale
F = args.fold
PAIRS = f"{args.scaled}/pairs/{args.config}.pairs.parquet"
t0 = time.perf_counter()


def log(m):
    print(f"[{time.perf_counter() - t0:7.1f}s] {m}", flush=True)


def fold(tid):
    """Merge the local vocabularies of F consecutive replicas."""
    if F == 1:
        return tid
    loc = tid >= n_g
    out = tid.copy()
    idx = tid[loc] - n_g
    out[loc] = n_g + (idx // n_l // F) * n_l + (idx % n_l)
    return out


# ------------------------------------------------- pass 1: term sizes
log(f"pass 1: term sizes, {args.config} @ {N:,}, fold {F}")
pf = pq.ParquetFile(PAIRS)
sizes = None
n_pairs = 0
for b in pf.iter_batches(batch_size=16_000_000, columns=["entity_id", "term_id"]):
    ent = b.column("entity_id").to_numpy()
    keep = ent < N
    if not keep.any():
        continue
    tid = fold(b.column("term_id").to_numpy()[keep])
    n_pairs += len(tid)
    c = np.bincount(tid)
    if sizes is None:
        sizes = c
    elif len(c) > len(sizes):
        c[:len(sizes)] += sizes
        sizes = c
    else:
        sizes[:len(c)] += c
present = np.flatnonzero(sizes)
log(f"  {n_pairs:,} pairs, {len(present):,} distinct terms present")
nz = sizes[present]
print(f"  posting sizes: median {int(np.median(nz))}, p99 {int(np.percentile(nz, 99)):,}, "
      f"max {int(nz.max()):,}; singletons {int((nz == 1).sum()):,} "
      f"({100 * (nz == 1).sum() / len(nz):.1f}%)")
print(f"  as CSR: {4 * n_pairs / 1e9:.1f} GB postings + "
      f"{8 * len(present) / 1e9:.2f} GB offsets "
      f"(as a BitMap per term it would be ~{250 * len(present) / 1e9:.0f} GB of "
      f"Python/container overhead — hence this probe's shape)")

# --------------------------------------------------------- scenarios
rng = np.random.default_rng(args.seed)
order = present[np.argsort(-nz)]
scenarios, needed = [], set()
acc = 0
for c in COVERAGE_TARGETS:                 # head: largest postings first
    grant, acc = [], 0
    for t in order:
        grant.append(int(t)); acc += int(sizes[t])
        if acc >= c * N:                   # upper bound on coverage (overlap ignored)
            break
    scenarios.append((f"head c~{c:g}", grant)); needed.update(grant)
for w in WIDTH_TARGETS:
    if w <= len(present):
        g = [int(x) for x in rng.choice(present, w, replace=False)]
        scenarios.append((f"random w={w}", g)); needed.update(g)
log(f"scenarios: {len(scenarios)}, {len(needed):,} distinct terms needed")

# ------------------------------- pass 2: postings for granted terms only
log("pass 2: extracting granted postings")
want = np.zeros(int(present.max()) + 1, dtype=bool)
want[list(needed)] = True
buf = {t: [] for t in needed}
for b in pf.iter_batches(batch_size=16_000_000, columns=["entity_id", "term_id"]):
    ent = b.column("entity_id").to_numpy()
    keep = ent < N
    if not keep.any():
        continue
    ent = ent[keep]
    tid = fold(b.column("term_id").to_numpy()[keep])
    sel = want[tid]
    if not sel.any():
        continue
    e2, t2 = ent[sel], tid[sel]
    o = np.argsort(t2, kind="stable")
    e2, t2 = e2[o], t2[o]
    edges = np.flatnonzero(np.diff(t2)) + 1
    for lo, hi in zip(np.concatenate(([0], edges)), np.concatenate((edges, [len(t2)]))):
        buf[int(t2[lo])].append(e2[lo:hi])
postings = {t: BitMap(np.concatenate(v)) if v else BitMap() for t, v in buf.items()}
del buf
log(f"  built {len(postings):,} postings")

print(f"\n=== {args.config} @ {N:,}, fold {F} "
      f"({len(present):,} distinct terms) ===")
print(f"{'scenario':<16} {'w':>7} {'cover%':>8} {'|mask|':>14} {'union ms':>10} {'ser MB':>8}")
for label, grant in scenarios:
    ms = None
    for _ in range(args.reps):
        t = time.perf_counter()
        mask = BitMap.union(*(postings[g] for g in grant))
        ms = min(ms or 9e9, (time.perf_counter() - t) * 1000)
    print(f"{label:<16} {len(grant):>7,} {100 * len(mask) / N:>8.3f} {len(mask):>14,} "
          f"{ms:>10.1f} {len(mask.serialize()) / 1e6:>8.2f}")
log("done")
