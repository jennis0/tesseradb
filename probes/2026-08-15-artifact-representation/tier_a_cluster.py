"""Tier A: real HDBSCAN over the real UMAP geometry of the 2.42M arXiv corpus.

Geometry is recovered from the built bundle's sorted `morton.u32` (row order IS
morton order), so the coordinates are the real cuML UMAP output, quantised.
Emits the distributional parameters Tier B needs, and the row-space/entity-space
membership arrays M1 measures.
"""
import numpy as np, json, sys, time
from sklearn.cluster import HDBSCAN

FIX = '/tmp/tessera-bench/fixtures/2422486/attrs-both/v00000/partitions/default'
OUT = 'probes/2026-08-15-artifact-representation'

def demorton(c):
    c = c.astype(np.uint64); x = np.zeros_like(c); y = np.zeros_like(c)
    for i in range(16):
        x |= ((c >> np.uint64(2*i)) & np.uint64(1)) << np.uint64(i)
        y |= ((c >> np.uint64(2*i+1)) & np.uint64(1)) << np.uint64(i)
    return x.astype(np.float32), y.astype(np.float32)

morton = np.fromfile(f'{FIX}/slices/s0/segments/seg-0/morton.u32', dtype=np.uint32)
x, y = demorton(morton)
xy = np.stack([x, y], 1)
N = len(xy)
print(f'rows={N} (row order == morton order)', flush=True)

# Cluster a subsample, then assign the remainder by nearest centroid.
# HDBSCAN on 2.4M 2D points is not tractable here; the sample preserves the
# distributional properties Tier B needs and the full assignment preserves the
# row-space geometry M1 needs.
SAMPLE = int(sys.argv[1]) if len(sys.argv) > 1 else 300_000
rng = np.random.default_rng(20260815)
idx = rng.choice(N, SAMPLE, replace=False); idx.sort()

layers = {}
for name, mcs in [('L2', 60), ('L1', 600), ('L0', 6000)]:
    t0 = time.time()
    h = HDBSCAN(min_cluster_size=mcs, min_samples=10, n_jobs=-1)
    lab = h.fit_predict(xy[idx])
    k = lab.max() + 1
    noise = float((lab < 0).mean())
    print(f'{name}: min_cluster_size={mcs} clusters={k} noise={noise:.3f} '
          f'({time.time()-t0:.0f}s)', flush=True)
    # centroids over the sample, then assign every row
    cent = np.zeros((k, 2), np.float32)
    for c in range(k):
        cent[c] = xy[idx][lab == c].mean(0)
    # nearest-centroid assignment in chunks; keep noise proportion by distance cutoff
    assign = np.zeros(N, np.int64)
    d_thresh = None
    CH = 200_000
    dists = np.empty(N, np.float32)
    for s in range(0, N, CH):
        e = min(s+CH, N)
        d = ((xy[s:e, None, :] - cent[None, :, :])**2).sum(2)
        nn = d.argmin(1)
        assign[s:e] = nn
        dists[s:e] = np.sqrt(d[np.arange(e-s), nn])
    # reproduce the measured noise fraction by cutting the farthest points
    if noise > 0:
        d_thresh = np.quantile(dists, 1.0 - noise)
        assign[dists > d_thresh] = -1
    layers[name] = dict(min_cluster_size=mcs, k=int(k), noise=noise,
                        assign=assign)
    print(f'  assigned all {N} rows; realised noise={(assign<0).mean():.3f}', flush=True)

np.savez_compressed(f'{OUT}/tier_a_assign.npz',
                    **{n: v['assign'].astype(np.int32) for n, v in layers.items()},
                    morton=morton)
stats = {n: dict(min_cluster_size=v['min_cluster_size'], k=v['k'],
                 noise=round(float((v['assign'] < 0).mean()), 4),
                 sizes=np.bincount(v['assign'][v['assign'] >= 0]).tolist())
         for n, v in layers.items()}
json.dump({'n_rows': N, 'sample': SAMPLE, 'layers': stats},
          open(f'{OUT}/tier_a_stats.json', 'w'))
print('wrote tier_a_assign.npz and tier_a_stats.json')
