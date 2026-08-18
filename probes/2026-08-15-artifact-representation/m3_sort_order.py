"""M3 - the signature-sort tiebreak: (signature, source_id) vs (signature, morton).

The major key stays the term signature, so the posting win must be unchanged;
only the order WITHIN a signature group moves. Measures both membership size and
posting size, because the second is what the change must not regress.
"""
import numpy as np, json
import pyarrow.parquet as pq
from pyroaring import BitMap

FIX = '/tmp/tessera-bench/fixtures/2422486/attrs-both/v00000/partitions/default'
OUT = 'probes/2026-08-15-artifact-representation'

d = np.load(f'{OUT}/tier_a_assign.npz')
row_entity = np.fromfile(f'{FIX}/views/s0/row-entity.u32', dtype=np.uint32)
N = len(row_entity)
entity_row = np.empty(N, np.uint32); entity_row[row_entity] = np.arange(N, dtype=np.uint32)

t = pq.read_table(f'{FIX}/terms/pairs.parquet')
ent = t.column('entity_id').to_numpy().astype(np.int64)
trm = t.column('term_id').to_numpy().astype(np.int64)
order = np.lexsort((trm, ent))
ent, trm = ent[order], trm[order]
starts = np.searchsorted(ent, np.arange(N+1))
print(f'pairs={len(ent)} entities={N} terms={trm.max()+1}')

# signature key: hash the sorted term tuple per entity (stable, order-preserving
# enough for grouping - we only need equal signatures to collide and group)
sig = np.zeros(N, np.uint64)
for e in range(N):
    s, x = starts[e], starts[e+1]
    h = np.uint64(1469598103934665603)
    for tt in trm[s:x]:
        h ^= np.uint64(tt); h = np.uint64(h * np.uint64(1099511628211))
    sig[e] = h
print('signatures:', len(np.unique(sig)), 'distinct')

# current order is the identity on entity ids (the bundle already allocated this way).
# alternative: regroup by signature, order within group by morton (== row rank).
def assign_ids(within_key):
    o = np.lexsort((within_key, sig))       # signature major, `within_key` minor
    newid = np.empty(N, np.uint32); newid[o] = np.arange(N, dtype=np.uint32)
    return newid

id_src = assign_ids(np.arange(N, dtype=np.int64))   # (signature, source_id)  == shipped
id_mor = assign_ids(entity_row.astype(np.int64))    # (signature, morton rank)

def membership_bytes(newid):
    tot = 0
    for layer in ['L0','L1','L2']:
        lab = d[layer]
        ids_by_row = newid[row_entity]                # row -> new entity id
        for c in range(lab.max()+1):
            bm = BitMap(ids_by_row[lab == c]); bm.run_optimize()
            tot += len(bm.serialize())
    return tot

def posting_bytes(newid):
    tot = 0
    for tid in range(int(trm.max())+1):
        e = ent[trm == tid]
        if len(e) == 0: continue
        bm = BitMap(newid[e]); bm.run_optimize()
        tot += len(bm.serialize())
    return tot

ms, mm = membership_bytes(id_src), membership_bytes(id_mor)
ps, pm = posting_bytes(id_src), posting_bytes(id_mor)
print(f'membership: source_id {ms/1e6:.3f} MB -> morton {mm/1e6:.3f} MB  ({ms/max(mm,1):.2f}x)')
print(f'postings:   source_id {ps/1e6:.3f} MB -> morton {pm/1e6:.3f} MB  ({ps/max(pm,1):.2f}x)')
json.dump(dict(membership_src=ms, membership_morton=mm,
               postings_src=ps, postings_morton=pm,
               membership_gain=round(ms/max(mm,1),3),
               posting_change=round(ps/max(pm,1),3)),
          open(f'{OUT}/m3_sort_order.json','w'), indent=1)
