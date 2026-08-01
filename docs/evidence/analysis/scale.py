import math
PAGE=4096
def h(x,u='B'):
    for s,d in (('P',1e15),('T',1e12),('G',1e9),('M',1e6),('K',1e3)):
        if abs(x)>=d: return f"{x/d:.2f}{s}{u}"
    return f"{x:.0f}{u}"

print("=== 1. WHAT BREAKS FIRST: ID SPACE ===")
print("u32 row IDs top out at 4.29e9.\n")
for N in (1e9,1e10,1e11,1e12):
    shards_u32 = math.ceil(N/2**32)
    print(f"  N={N:.0e}  global u32 rowids: {'OK' if N<2**32 else 'IMPOSSIBLE'}"
          f"   min shards for per-shard u32: {shards_u32:>6,}")
print("\n  => per-shard row IDs keep u32 (and keep Roaring32's bounded container directory).")
print("     Entity IDs go u64 globally. This is I4's split doing useful work at scale.\n")

print("=== 2. MONOLITHIC STRUCTURE SIZES (why sharding is forced) ===")
print(f"{'N':>7} {'x,y':>10} {'prio u16':>10} {'perm u64':>10} {'term idx':>10} {'TOTAL':>10}")
for N in (1e9,1e10,1e11,1e12):
    xy,pr,pe,ti = N*8,N*2,N*8,N*10*1.5
    print(f"{N:7.0e} {h(xy):>10} {h(pr):>10} {h(pe):>10} {h(ti):>10} {h(xy+pr+pe+ti):>10}")

print("\n=== 3. THE MASK IS THE REAL CEILING ===")
print("Roaring size tracks the UNIVERSE for dense masks, the CARDINALITY for sparse ones.")
print("Crossover: bitmap container at >4096/65536 = 6.25% density.\n")
print(f"{'N':>7} {'coverage':>9} {'authorised':>11} {'mask size':>11} {'representation':>16}")
for N in (1e9,1e10,1e11,1e12):
    for cov in (0.10, 0.01, 0.0001):
        card=N*cov
        if cov > 4096/65536:
            size=(N/65536)*8192; rep='bitmap containers'
        else:
            size=card*2*1.15; rep='array containers'
        print(f"{N:7.0e} {cov:8.2%} {h(card,''):>11} {h(size):>11} {rep:>16}")
    print()

print("=== 4. SHARDING BY MORTON RANGE ===")
SHARD=1e9
print(f"Shard = {SHARD:.0e} rows (keeps per-shard u32 rowids and ~10GB hot columns).\n")
print(f"{'N':>7} {'shards':>8} {'hot/shard':>10} {'nodes to hold it':>17}")
for N in (1e10,1e11,1e12):
    s=N/SHARD
    print(f"{N:7.0e} {s:8,.0f} {h(SHARD*10):>10} {f'{s:,.0f} x ~16GB RAM':>17}")

print("\n=== 5. VIEWPORT FAN-OUT BY ZOOM (the thing that actually decides feasibility) ===")
print("A viewport at zoom z covers 4^-z of the plane. Morton ranges -> shards touched.\n")
for N,label in ((1e10,'1e10'),(1e12,'1e12')):
    shards=N/SHARD
    print(f"  N={label}  ({shards:,.0f} shards)")
    print(f"    {'zoom':>5} {'frac of plane':>14} {'shards touched':>15} {'mask frags to build':>20}")
    for z in (0,2,4,6,8,10):
        frac=4.0**-z
        touched=max(1,min(shards, shards*frac))
        print(f"    {z:5} {frac:14.6f} {touched:15,.0f} {touched:20,.0f}")
    print()

print("=== 6. MASK BUILD COST AT SCALE (semi-join over the pair relation) ===")
print("Pairs = N x 10. Assume 100M pairs/sec/core scanned, 32 cores.\n")
print(f"{'N':>7} {'pairs':>9} {'full build':>12} {'per 1e9 shard':>14}")
for N in (1e9,1e10,1e11,1e12):
    pairs=N*10; full=pairs/(1e8*32); per=1e10/(1e8*32)
    print(f"{N:7.0e} {h(pairs,''):>9} {full:11.1f}s {per:13.2f}s")
print("\n  => full-corpus mask build is impossible past ~1e10. Per-shard, lazily, is ~3s.")

print("\n=== 7. WHAT IS SCALE-INVARIANT ===")
T=10_000
def distinct(k,P): return P*(1-(1-1/max(P,1))**k)
print("Per-tile costs depend on tile capacity and coverage, NOT on N:")
print(f"  exact-scan priority block : {T*4/PAGE:.1f} pages  (T={T:,})")
print(f"  gather at k=30            : {2*distinct(30,T*4/PAGE):.1f} pages")
print(f"  descent/exact crossover   : ~5% coverage")
print("  => the query algorithm does not change. Only placement and mask management do.")
