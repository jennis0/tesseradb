"""M7 - does the (signature, morton) tiebreak help the CORE structures too?

Artifact membership is the most spatially-correlated set there is, so it shows
the largest effect. This asks what the same reordering does to (a) the
permutation array, (b) postings over attributes that correlate with position,
and (c) the row->entity inverse. Real corpus throughout.
"""
import numpy as np, json
import pyarrow.parquet as pq, pyarrow as pa, pyarrow.ipc as ipc
from pyroaring import BitMap
import zlib

FIX='/tmp/tessera-bench/fixtures/2422486/attrs-both/v00000/partitions/default'
OUT='probes/2026-08-15-artifact-representation'

row_entity=np.fromfile(f'{FIX}/views/s0/row-entity.u32',dtype=np.uint32)
N=len(row_entity)
entity_row=np.empty(N,np.uint32); entity_row[row_entity]=np.arange(N,dtype=np.uint32)

t=pq.read_table(f'{FIX}/terms/pairs.parquet')
ent=t.column('entity_id').to_numpy().astype(np.int64)
trm=t.column('term_id').to_numpy().astype(np.int64)
o=np.lexsort((trm,ent)); ent,trm=ent[o],trm[o]
starts=np.searchsorted(ent,np.arange(N+1))
sig=np.zeros(N,np.uint64)
for e in range(N):
    h=np.uint64(1469598103934665603)
    for tt in trm[starts[e]:starts[e+1]]:
        h=np.uint64((int(h)^int(tt))*1099511628211 % (1<<64))
    sig[e]=h

def assign_ids(minor):
    ordr=np.lexsort((minor,sig))
    nid=np.empty(N,np.uint32); nid[ordr]=np.arange(N,dtype=np.uint32); return nid

id_src=assign_ids(np.arange(N,dtype=np.int64))
id_mor=assign_ids(entity_row.astype(np.int64))

def perm_stats(newid, tag):
    # permutation.bin is entity -> row, indexed by new entity id
    perm=np.empty(N,np.uint32); perm[newid]=entity_row
    d=np.diff(perm.astype(np.int64))
    asc=float((d>0).mean())                     # fraction of adjacent pairs ascending
    # monotone run length
    breaks=np.flatnonzero(d<=0); runs=np.diff(np.concatenate(([-1],breaks,[N-1])))
    raw=perm.tobytes()
    # zlib on the raw array, and on the within-group delta form
    dz=len(zlib.compress(raw,6))
    delta=np.concatenate(([perm[0]],np.diff(perm.astype(np.int64)))).astype(np.int64)
    dzd=len(zlib.compress(delta.astype(np.int32).tobytes(),6))
    print(f'  {tag:>10}: ascending={asc:.3f}  mean monotone run={runs.mean():8.1f}  '
          f'raw={len(raw)/1e6:.2f} MB  zlib={dz/1e6:.2f} MB  delta+zlib={dzd/1e6:.2f} MB')
    return dict(ascending=round(asc,4), mean_run=round(float(runs.mean()),2),
                raw=len(raw), zlib=dz, delta_zlib=dzd)

print('permutation.bin (entity -> row):')
p_src=perm_stats(id_src,'source_id'); p_mor=perm_stats(id_mor,'morton')

# spatially-correlated attribute: primary_category (a real column on this bundle)
with pa.memory_map(f'{FIX}/views/s0/segments/seg-0/columns.arrow','rb') as src:
    cols=ipc.open_file(src).read_all()
pc_by_row=cols.column('primary_category').to_numpy()
pc_by_entity=np.empty(N,pc_by_row.dtype); pc_by_entity[row_entity]=pc_by_row

def postings_bytes(newid, vals, tag):
    tot=0
    for v in np.unique(vals):
        b=BitMap(newid[vals==v]); b.run_optimize(); tot+=len(b.serialize())
    print(f'  {tag:>10}: {tot/1e6:.3f} MB over {len(np.unique(vals))} values')
    return tot

print('\nprimary_category postings (spatially correlated attribute):')
c_src=postings_bytes(id_src,pc_by_entity,'source_id')
c_mor=postings_bytes(id_mor,pc_by_entity,'morton')
print(f'  gain: {c_src/max(c_mor,1):.2f}x')

json.dump(dict(permutation=dict(source_id=p_src,morton=p_mor),
               category_postings=dict(source_id=c_src,morton=c_mor,
                                      gain=round(c_src/max(c_mor,1),3))),
          open(f'{OUT}/m7_core_rows.json','w'), indent=1)
