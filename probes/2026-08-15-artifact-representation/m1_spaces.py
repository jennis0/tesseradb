"""M1 — does cluster membership run-encode in ROW space but not ENTITY space?

Uses the real 2.42M corpus: real UMAP geometry, real HDBSCAN membership, and the
real entity<->row permutation built under the shipped (signature, source_id)
allocation rule. pyroaring is croaring, the same library the engine uses, and
`serialize()` is the portable format.
"""
import numpy as np, json
from pyroaring import BitMap

FIX = '/tmp/tessera-bench/fixtures/2422486/attrs-both/v00000/partitions/default'
OUT = 'probes/2026-08-15-artifact-representation'

d = np.load(f'{OUT}/tier_a_assign.npz')
row_entity = np.fromfile(f'{FIX}/views/s0/row-entity.u32', dtype=np.uint32)
N = len(row_entity)
print(f'rows={N}')

def measure(labels, ids, tag):
    """labels indexed by row; ids maps row -> the id space being measured."""
    k = labels.max() + 1
    tot = 0; runopt = 0; members = 0
    per = []
    for c in range(k):
        sel = ids[labels == c]
        bm = BitMap(sel)
        raw = len(bm.serialize())
        bm.run_optimize()
        opt = len(bm.serialize())
        tot += raw; runopt += opt; members += len(sel)
        per.append((len(sel), raw, opt))
    return dict(tag=tag, clusters=int(k), members=int(members),
                bytes=int(tot), bytes_runopt=int(runopt),
                bytes_per_member=round(runopt/max(members,1), 3), per=per)

rows = np.arange(N, dtype=np.uint32)
res = {}
for layer in ['L0', 'L1', 'L2']:
    lab = d[layer]
    r = measure(lab, rows, 'row')
    e = measure(lab, row_entity, 'entity')
    ratio = e['bytes_runopt'] / max(r['bytes_runopt'], 1)
    res[layer] = dict(row=r, entity=e, ratio=round(ratio, 2))
    print(f"{layer}: k={r['clusters']:>4} members={r['members']:>9}  "
          f"row={r['bytes_runopt']/1e6:8.3f} MB ({r['bytes_per_member']:.3f} B/mem)   "
          f"entity={e['bytes_runopt']/1e6:8.3f} MB ({e['bytes_per_member']:.3f} B/mem)   "
          f"entity/row = {ratio:.1f}x")

# also: what does run_optimize buy in each space (i.e. is it runs or arrays?)
for layer in ['L0','L1','L2']:
    r, e = res[layer]['row'], res[layer]['entity']
    print(f"  {layer} run_optimize gain: row {r['bytes']/max(r['bytes_runopt'],1):.1f}x, "
          f"entity {e['bytes']/max(e['bytes_runopt'],1):.2f}x")

for v in res.values():
    v['row'].pop('per'); v['entity'].pop('per')
json.dump(res, open(f'{OUT}/m1_spaces.json','w'), indent=1)
