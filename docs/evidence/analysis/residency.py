import math
PAGE=4096

print("=== WHEN DOES THE CANDIDATE-LIST FAST PATH ACTUALLY WORK? ===")
print("A node's candidate list holds w = cand_mult*k entries (top by priority).")
print("Survivors after masking ~ w*coverage. Need k. So need coverage >= 1/cand_mult.\n")
for cm in (4, 10, 40, 100, 400):
    print(f"  cand_mult={cm:4}  (w={cm*30:5} at k=30)   fast path works only above coverage {1/cm:7.2%}")

print("\nRealistic coverage: 1e4 grants against 1e5-1e6 categories, ~10 categories/item.")
print("Even with perfect grant-to-item leverage that is single-digit percent at best.")
print("=> the 4k candidate list serves ~nobody. Descent is the normal path, not the fallback.\n")

print("=== DESCENT COST IS LINEAR IN 1/COVERAGE, NOT LOGARITHMIC ===")
print("depth L = log4(k/(w*cov)); nodes visited = sum 4^i = (4^(L+1)-1)/3 ~ (4/3)*k/(w*cov)\n")
print(f"{'coverage':>10} {'depth':>6} {'nodes visited':>14} {'vs log claim':>13}")
for cov in (0.25,0.1,0.05,0.01,0.001,0.0001):
    w=120;k=30
    L=max(0,math.ceil(math.log(max(k/(w*cov),1),4)))
    nodes=(4**(L+1)-1)/3
    print(f"{cov:9.2%} {L:6} {nodes:14,.0f} {'log would be '+str(L):>13}")

print("\n=== THE EXACT SCAN IS BOUNDED. DESCENT IS NOT. ===")
print("Exact: take visible row IDs in the tile range (free from the bitmap),")
print("read their priorities, keep k lowest. Cost is bounded by the tile's priority block.\n")
T=10_000
prio_pages=T*4/PAGE
def distinct(k,P): return P*(1-(1-1/max(P,1))**k) if P>0 else 0
print(f"{'coverage':>10} {'visible/tile':>13} {'descent nodes':>14} {'exact pages':>12} {'winner':>10}")
for cov in (0.25,0.1,0.05,0.01,0.001,0.0001):
    vis=cov*T; w=120;k=30
    L=max(0,math.ceil(math.log(max(k/(w*cov),1),4))); nodes=(4**(L+1)-1)/3
    exact=min(distinct(min(vis,T),prio_pages),prio_pages)
    print(f"{cov:9.2%} {vis:13,.1f} {nodes:14,.0f} {exact:12.1f} {'exact' if exact<nodes else 'descent':>10}")

print("\n=== RESIDENT-SET STRATEGIES AT N=1e9 ===")
N=1e9
opts={
 'x,y (2x f32)':N*8, 'priority u32':N*4, 'priority u16 (quantised)':N*2,
 'rowid->entity u32':N*4, 'entity->row u32/slice':N*4,
 'term index (~1.5B/posting)':N*10*1.5, 'rep groups (w=120)':349525*120*16,
}
for k,v in opts.items(): print(f"  {k:32} {v/1e9:8.2f} GB")
print()
strategies={
 'A. everything resident':N*8+N*4+N*4,
 'B. x,y + prio-u16 resident':N*8+N*2,
 'C. prio-u16 only; x,y paged':N*2,
 'D. nothing resident':0,
}
for s,v in strategies.items(): print(f"  {s:32} {v/1e9:8.2f} GB resident")

print("\n=== CONCURRENCY: MASKS ARE THE SCALING TERM ===")
print("Roaring over 1e9 universe. Size depends entirely on Morton run-length (Phase 0 unknown).\n")
print(f"{'clustering':>16} {'MB/session':>11} " + " ".join(f"{('n='+str(n)):>10}" for n in (10,100,1000,10000)))
for rc,label in ((1.0,'none'),(0.25,'moderate'),(0.05,'strong'),(0.01,'very strong')):
    mb=(1e9/65536)*8192*rc/1e6
    row=" ".join(f"{mb*n/1000:9.2f}G" for n in (10,100,1000,10000))
    print(f"{label:>16} {mb:11.1f} {row}")

print("\n=== GATHER COST FOR THE SELECTED POINTS (irreducible unless x,y resident) ===")
for k in (10,30,60,120):
    pages=300*2*distinct(k,T*4/PAGE)
    print(f"  k={k:4}  {pages:8,.0f} pages/viewport  {max(pages/32*20/1000,pages*PAGE/3e9*1000):6.2f} ms cold   0 ms if x,y resident")
