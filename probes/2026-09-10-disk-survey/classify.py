import json,os,sys,re
from collections import defaultdict

def kind(rel):
    p = rel
    if p == "CURRENT" or p.endswith("MANIFEST.json"): return "manifest / CURRENT"
    if p.startswith("reports/"): return "reports (build only)"
    if p.startswith("dictionary/"): return "term dictionary"
    # strip partitions/<name>/
    m = re.match(r"partitions/[^/]+/(.*)", p)
    if not m: return "other: "+p
    q = m.group(1)
    if q.startswith("SEGMENTS"): return "SEGMENTS-*.json"
    if q.startswith("containment/"): return "containment (.tscp)"
    if q.startswith("members/"): return "artifact members (.tsmb)"
    if q.startswith("row-column/"): return "row-column (.tslb)"
    if q.startswith("tile-index/"): return "artifact tile-index (.tsti)"
    if q.startswith("artifacts/"): return "artifact store: "+q.split("/",2)[-1]
    if q.startswith("terms/pairs.parquet"): return "access terms: pairs.parquet (oracle)"
    if q.startswith("terms/"): return "access terms: postings.arrow"
    if q.startswith("entities/terms/"): return "entity->terms (offsets/terms/hasrow)"
    if q.startswith("entities/ext-locator"): return "entities: ext-locator.u32"
    if q.startswith("entities/external-ids"): return "entities: external-ids-*.arrow"
    if q.startswith("entities/"): return "entities: "+q.split("/",1)[1]
    if q.startswith("attrs/record/extents/"): return "record blob: artifact extents"
    if q.startswith("attrs/record/"):
        return "record blob: "+q.rsplit("/",1)[-1]
    if q.startswith("attrs/"):
        rest = q.split("/",2)
        f = rest[2] if len(rest)>2 else rest[-1]
        return "attr column: "+f
    if q.startswith("views/"):
        parts = q.split("/")
        if "segments" in parts:
            return "view segment: "+parts[-1]
        return "view: "+parts[-1]
    if q.startswith("annotations") or q.startswith("overlay"): return "overlay/annotation: "+q
    return "other: "+q

def run(bundle):
    cur = json.load(open(os.path.join(bundle,"CURRENT")))
    pref = cur["prefix"]
    man = json.load(open(os.path.join(bundle,pref,"MANIFEST.json")))
    n = man.get("entity_id_high_water")
    agg_a=defaultdict(int); agg_b=defaultdict(int); cnt=defaultdict(int)
    tot_a=tot_b=0
    for root,dirs,files in os.walk(bundle):
        for f in files:
            fp=os.path.join(root,f)
            rel=os.path.relpath(fp,bundle)
            # normalise version prefix away
            rel2 = rel[len(pref)+1:] if rel.startswith(pref+"/") else rel
            st=os.stat(fp)
            a=st.st_blocks*512; b=st.st_size
            k=kind(rel2)
            agg_a[k]+=a; agg_b[k]+=b; cnt[k]+=1
            tot_a+=a; tot_b+=b
    print(f"\n===== {bundle}  items={n:,}  files={sum(cnt.values())}")
    print(f"  total allocated {tot_a/1e9:.3f} GB = {tot_a/n:.2f} B/item ; apparent {tot_b/1e9:.3f} GB = {tot_b/n:.2f} B/item")
    print(f"  {'kind':50s} {'n':>4s} {'alloc MB':>10s} {'B/item':>9s} {'%':>6s}")
    for k in sorted(agg_a, key=lambda x:-agg_a[x]):
        print(f"  {k:50s} {cnt[k]:>4d} {agg_a[k]/1e6:>10.2f} {agg_a[k]/n:>9.3f} {100*agg_a[k]/tot_a:>5.1f}%")
    return n, tot_a, agg_a

for b in sys.argv[1:]:
    run(b)
