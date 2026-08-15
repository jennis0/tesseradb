"""M8 - is the ~80 B/artifact sizing rule shape-sensitive?

M1/M2 measured HDBSCAN clusters, which are compact blobs. Geographic regions are
not: a coastline is fractal, a river corridor is elongated, an archipelago is
disconnected. A region's Morton run count tracks its PERIMETER, not its area, so
the sizing rule may not transfer to a geo deployment. Same member count
throughout, so only shape varies.
"""
import numpy as np, json
from pyroaring import BitMap

G = 4096                      # grid side; row id = morton rank over occupied cells
TARGET = 40_000               # members per region, held constant

def morton(x, y):
    x = x.astype(np.uint64); y = y.astype(np.uint64)
    c = np.zeros_like(x)
    for i in range(12):
        c |= ((x >> np.uint64(i)) & np.uint64(1)) << np.uint64(2*i)
        c |= ((y >> np.uint64(i)) & np.uint64(1)) << np.uint64(2*i+1)
    return c

rng = np.random.default_rng(20260815)
# a uniformly populated plane; row rank = morton order over all points
px = rng.integers(0, G, 4_000_000); py = rng.integers(0, G, 4_000_000)
pm = morton(px, py)
order = np.argsort(pm, kind='stable')
px, py = px[order], py[order]
row_of = np.arange(len(px), dtype=np.uint32)

def rows_in(mask_fn, name):
    """Members at NATIVE density — never decimated.

    An earlier revision held member count constant by `np.linspace` decimation.
    That was a bug that produced this probe's headline result: a stride >= 2
    never keeps two adjacent row ids, so runs == members is *forced* for any
    oversized region, degenerate or not. A compact disc put through it reports
    the same 40 000 runs as a corridor. Compare B/member instead, which is the
    scale-free quantity and needs no equal-size trick.
    """
    sel = mask_fn(px, py)
    idx = row_of[sel]
    b = BitMap(idx); b.run_optimize()
    n = len(idx); sz = len(b.serialize())
    # count maximal runs directly
    runs = 1 + int((np.diff(np.sort(idx)) > 1).sum()) if n else 0
    print(f'  {name:<22} members={n:>6}  runs={runs:>7}  bytes={sz:>8}  '
          f'B/member={sz/max(n,1):6.3f}')
    return dict(members=int(n), runs=runs, bytes=sz, b_per_member=round(sz/max(n,1),4))

cx = cy = G/2
res = {}
r = np.sqrt(TARGET/len(px)*G*G/np.pi)
res['disc'] = rows_in(lambda x,y: (x-cx)**2+(y-cy)**2 < r*r, 'disc (compact)')

side = np.sqrt(TARGET/len(px)*G*G)
res['square'] = rows_in(lambda x,y: (np.abs(x-cx)<side/2)&(np.abs(y-cy)<side/2), 'square')

res['corridor'] = rows_in(lambda x,y: (np.abs(x-cx)<side*4)&(np.abs(y-cy)<side/8),
                          'corridor (river/road)')

# fractal boundary: radius modulated by summed harmonics -> crenellated coastline
def coast(x, y, k=24, amp=0.45):
    th = np.arctan2(y-cy, x-cx); rr = np.sqrt((x-cx)**2+(y-cy)**2)
    w = np.zeros_like(th)
    for h in range(2, k):
        w += np.sin(h*th + h*1.7) / h
    return rr < r*(1 + amp*w)
res['coastline'] = rows_in(coast, 'coastline (fractal)')

# archipelago: many small discs
def arch(x, y, n=60):
    cs = rng.integers(int(G*0.2), int(G*0.8), (n,2))
    rr = r/np.sqrt(n)*1.6
    m = np.zeros(len(x), bool)
    for c in cs: m |= ((x-c[0])**2 + (y-c[1])**2) < rr*rr
    return m
res['archipelago'] = rows_in(arch, 'archipelago (60 parts)')

base = res['disc']['b_per_member']
print('\nrelative to the compact disc:')
for k, v in res.items():
    print(f'  {k:<14} {v["b_per_member"]/base:6.1f}x   ({v["runs"]:>7} runs)')
json.dump(res, open('probes/2026-08-15-artifact-representation/m8_geo_shapes.json','w'), indent=1)
