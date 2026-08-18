import math

PAGE = 4096

def quadtree(N, leaf_cap):
    leaves = N / leaf_cap
    depth = math.ceil(math.log(max(leaves,1), 4))
    nodes = sum(4**i for i in range(depth+1))
    return leaves, depth, nodes

def distinct_pages(k, pages_spanned):
    """expected distinct pages touched by k uniformly scattered picks"""
    P = max(pages_spanned, 1.0)
    return P * (1 - (1 - 1/P)**k)

def sizes(N, leaf_cap=10_000, k=30, cand_mult=4, hot_bytes=20,
          mask_density=0.10, run_compress=1.0, views=1):
    leaves, depth, nodes = quadtree(N, leaf_cap)
    cand_w = cand_mult * k
    # rep group entry: x,y float32 + priority u32 + rowid u32
    rep_entry = 4+4+4+4
    rep = nodes * cand_w * rep_entry
    hot = N * hot_bytes
    perm = N * 4 * views
    # roaring mask: containers of 2^16 over universe N
    containers = N / 65536
    # dense containers -> 8KB bitmap; run_compress models spatial clustering
    mask = containers * 8192 * run_compress
    # term index: N * terms_per_item postings, ~1.5 bytes/posting compressed
    terms = N * 10 * 1.5
    tile_table = leaves * 8
    return dict(leaves=leaves, depth=depth, nodes=nodes, cand_w=cand_w,
                rep=rep, hot=hot, perm=perm, mask=mask, terms=terms,
                tile_table=tile_table)

def viewport_io(N, tiles_in_view=300, leaf_cap=10_000, k=30, cand_mult=4,
                cols=3, col_bytes=4):
    cand_w = cand_mult*k
    # Design A: gather k scattered rows per tile from each global column
    pages_per_tile_col = leaf_cap*col_bytes/PAGE
    a_pages = tiles_in_view * cols * distinct_pages(k, pages_per_tile_col)
    # Design B: one contiguous rep-group read per node
    rep_bytes_per_node = cand_w * 16
    b_pages = tiles_in_view * max(1, math.ceil(rep_bytes_per_node/PAGE))
    # mask containers touched (tile range / container size), clustered
    containers_touched = min(tiles_in_view, math.ceil(tiles_in_view*leaf_cap/65536)+tiles_in_view*0.15)
    mask_pages = containers_touched * 2
    return a_pages, b_pages, mask_pages

def latency_ms(pages, qd=32, io_us=20, bw_gbs=3.0):
    lat_bound = pages/qd*io_us/1000
    bw_bound = pages*PAGE/(bw_gbs*1e9)*1000
    return max(lat_bound, bw_bound)

def gb(x): return x/1e9

print("=== STRUCTURE SIZING (leaf_cap=10k, k=30, cand=4k) ===")
print(f"{'N':>6} {'nodes':>9} {'rep':>9} {'hot cols':>10} {'perm':>8} {'terms':>8} {'mask/session':>13}")
for N in (1e7, 1e8, 1e9):
    s = sizes(int(N))
    print(f"{N:6.0e} {s['nodes']:9,.0f} {gb(s['rep']):8.3f}G {gb(s['hot']):9.2f}G "
          f"{gb(s['perm']):7.2f}G {gb(s['terms']):7.2f}G {s['mask']/1e6:12.1f}M")

print("\n=== VIEWPORT I/O (300 tiles, 3 hot columns) ===")
print(f"{'N':>6} {'A: global':>10} {'B: rep grp':>11} {'ratio':>7} {'A ms':>7} {'B ms':>7} {'mask pg':>8}")
for N in (1e7, 1e8, 1e9):
    a,b,m = viewport_io(int(N))
    print(f"{N:6.0e} {a:10,.0f} {b:11,.0f} {a/b:6.1f}x {latency_ms(a):6.2f} {latency_ms(b):6.2f} {m:8,.0f}")

print("\n=== REP GROUP SIZE vs k and leaf capacity (N=1e9) ===")
print(f"{'k':>5} " + " ".join(f"{('T='+str(t)):>9}" for t in (1_000,5_000,10_000,50_000)))
for k in (30, 60, 120, 250):
    row=[]
    for T in (1_000,5_000,10_000,50_000):
        s = sizes(int(1e9), leaf_cap=T, k=k)
        row.append(f"{gb(s['rep']):8.2f}G")
    print(f"{k:5} " + " ".join(f"{r:>9}" for r in row))

print("\n=== MASK SIZE vs spatial clustering (N=1e9, 10% authorised) ===")
print("run_compress models how well authorised items form runs in Morton order")
for rc,label in ((1.0,'none (uniform)'),(0.25,'moderate'),(0.05,'strong'),(0.01,'very strong')):
    s = sizes(int(1e9), run_compress=rc)
    m = s['mask']/1e6
    print(f"  {label:16} {m:8.1f} MB/session   100 sessions = {m*100/1000:7.2f} GB   "
          f"1000 sessions = {m*1000/1000:8.1f} GB")

print("\n=== DESCENT FALLBACK: extra node reads vs coverage (design B) ===")
print("levels descended until ~k survive, and resulting rep-group reads per tile")
for cov in (0.5, 0.1, 0.01, 0.001, 0.0001):
    cand_w = 120
    lvl = 0
    survivors = cand_w*cov
    reads = 1
    while survivors < 30 and lvl < 10:
        lvl += 1
        reads += 4**lvl        # merging children's lists
        survivors = cand_w*cov*(4**lvl)
    print(f"  coverage {cov:8.2%}  levels={lvl}  node reads/tile={reads:6,}  "
          f"300 tiles -> {reads*300:9,} reads  {latency_ms(reads*300):7.1f} ms")
