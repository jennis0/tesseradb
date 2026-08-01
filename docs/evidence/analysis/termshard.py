import math
def h(x,u='B'):
    for s,d in (('P',1e15),('T',1e12),('G',1e9),('M',1e6),('K',1e3)):
        if abs(x)>=d: return f"{x/d:.2f}{s}{u}"
    return f"{x:.0f}{u}"

print("=== A. DISK WINS AT SCALE: FLEET SIZE, NOT LATENCY ===")
N=1e12; hot=N*10; total=N*33
for ram,nvme,label in ((0.128e12,0,'all-resident (128GB/node)'),
                       (0.128e12,8e12,'disk-backed (128GB RAM + 8TB NVMe)')):
    if nvme==0: nodes=math.ceil(hot/ram)
    else: nodes=math.ceil(total/nvme)
    print(f"  {label:38} {nodes:6,} nodes")
print("  => ~1 order of magnitude in fleet cost. Latency cost is ~8ms/viewport,")
print("     paid ONCE because at working zoom a viewport hits a single shard.\n")

print("=== B. DUPLICATION BREAKS EXACT COUNTING ===")
print("Item with terms {A,B}; user holds both. It sits in block A and block B.")
print("range_cardinality(A)+range_cardinality(B) double-counts it.\n")
tpi=10
for overlap in (0.1,0.3,0.5,0.8):
    inflate=1+ (tpi-1)*overlap
    print(f"  P(user holds a given co-term)={overlap:4.0%} -> counts inflated by {inflate:5.2f}x")
print("\n  Counts are the load-bearing primitive (I2, every density/cluster/frontier).")
print("  Deduping before counting means materialising the union - exactly what")
print("  range arithmetic exists to avoid. So: duplication is out.\n")

print("=== C. DISJOINT PARTITIONING IS THE SOUND VERSION (this is S12 generalised) ===")
print("Each item in exactly ONE partition -> counts are additive, sampling composes.")
print("But partitioning BY TERM gives terrible fan-out:\n")
print(f"{'partition key':>28} {'partitions':>11} {'reachable/user':>15} {'verdict':>10}")
for key,P,reach in (('term (1e6 terms)',1e6,1e4),
                    ('term, top-100 + tail bucket',100,80),
                    ('permission signature',1e4,1e3),
                    ('permission signature (coarse)',256,60),
                    ('Morton range only',1e3,4)):
    v='bad' if reach>50 else ('ok' if reach>8 else 'good')
    print(f"{key:>28} {P:11,.0f} {reach:15,.0f} {v:>10}")
print("\n  Fan-out per viewport is the cost. Morton sharding wins because a viewport")
print("  is spatially local; term sharding loses because a user is term-diffuse.\n")

print("=== D. WHAT PERMISSION-ALIGNED GROUPING ACTUALLY BUYS: MASK COLLAPSE ===")
print("If items are grouped by identical term-set signature, a group is wholly")
print("visible or wholly invisible. The mask becomes a union of complete ranges.\n")
N=1e12
print(f"{'signatures':>12} {'items/group':>13} {'mask (runs)':>12} {'mask (scattered)':>17} {'ratio':>9}")
for S in (1e2,1e3,1e4,1e5,1e6,1e9):
    per=N/S
    runs=S*0.5          # visible groups, 8 bytes per run (start,len)
    runsz=runs*8
    scattered=(N/65536)*8192*1.0   # dense bitmap containers
    print(f"{S:12,.0f} {h(per,''):>13} {h(runsz):>12} {h(scattered):>17} {scattered/runsz:8.0f}x")
print("\n  Even at 1e6 distinct signatures the mask is 4MB vs 125GB. The collapse is")
print("  total until signatures approach item count.\n")

print("=== E. THE CATCH: SIGNATURES FIGHT MORTON ORDER ===")
print("Partition-major, Morton-minor: each partition is Morton-ordered internally,")
print("so a tile is contiguous WITHIN a partition. A viewport queries each reachable")
print("partition separately and merges.\n")
tiles=300
for S,reach in ((256,60),(1e3,120),(1e4,1e3)):
    print(f"  {S:6,.0f} partitions, {reach:6,.0f} reachable -> {tiles*reach/1000:8,.0f}K tile lookups/viewport"
          f"   {'viable' if reach<=120 else 'too much fan-out'}")
print("\n  => coarse permission-aligned partitioning (1e2-1e3) is the sweet spot:")
print("     mask collapses to runs, counts stay additive, fan-out stays bounded.")

print("\n=== F. IS IT MEASURABLE UP FRONT? YES, AND IT IS ALREADY A PHASE 0 ARTEFACT ===")
print("  The DNF pass already computes each item's term set.")
print("  Hash it -> signature. Count distinct. Plot the size distribution.")
print("  That single histogram decides:")
print("    - whether permission-aligned partitioning is available at all")
print("    - the Morton spatial-autocorrelation question (same root cause)")
print("    - whether masks are 4MB or 125GB per session at 1e12")
