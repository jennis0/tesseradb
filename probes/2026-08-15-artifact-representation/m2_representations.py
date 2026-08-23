"""M2 - representation sizes for the same real membership, four ways."""
import numpy as np, json
from pyroaring import BitMap

FIX='/tmp/tessera-bench/fixtures/2422486/attrs-both/v00000/partitions/default'
OUT='probes/2026-08-15-artifact-representation'
d=np.load(f'{OUT}/tier_a_assign.npz')
row_entity=np.fromfile(f'{FIX}/views/s0/row-entity.u32',dtype=np.uint32)
N=len(row_entity); rows=np.arange(N,dtype=np.uint32)

def width(k): return 1 if k+1<=255 else (2 if k+1<=65535 else 4)

res={}
for layer in ['L0','L1','L2']:
    lab=d[layer]; k=int(lab.max()+1); cov=float((lab>=0).mean()); w=width(k)
    dense = N*w                                        # ordinal 0 = no artifact
    present = int((lab>=0).sum())
    pres_bm = BitMap(rows[lab>=0]); pres_bm.run_optimize()
    partial = present*w + len(pres_bm.serialize())      # values + presence bitmap
    rowbm = ent = 0
    for c in range(k):
        b=BitMap(rows[lab==c]); b.run_optimize(); rowbm+=len(b.serialize())
        b=BitMap(row_entity[lab==c]); b.run_optimize(); ent+=len(b.serialize())
    res[layer]=dict(clusters=k, coverage=round(cov,3), width=w,
                    dense_col=dense, partial_col=partial, row_bitmaps=rowbm,
                    entity_bitmaps=ent)
    print(f'{layer}: k={k:>4} cov={cov:.2f} w=u{w*8}  dense={dense/1e6:7.3f}  '
          f'partial={partial/1e6:7.3f}  ROW_BM={rowbm/1e6:7.3f}  entity_bm={ent/1e6:7.3f} MB'
          f'   | dense/rowbm = {dense/max(rowbm,1):6.1f}x')

tot={k:sum(res[l][k] for l in res) for k in ['dense_col','partial_col','row_bitmaps','entity_bitmaps']}
print(f"\nall three layers: dense={tot['dense_col']/1e6:.2f} MB  partial={tot['partial_col']/1e6:.2f} MB  "
      f"ROW_BITMAPS={tot['row_bitmaps']/1e6:.3f} MB  entity_bitmaps={tot['entity_bitmaps']/1e6:.2f} MB")
print(f"row bitmaps are {tot['dense_col']/tot['row_bitmaps']:.0f}x smaller than the dense column")
json.dump(dict(per_layer=res, totals=tot), open(f'{OUT}/m2_representations.json','w'), indent=1)
