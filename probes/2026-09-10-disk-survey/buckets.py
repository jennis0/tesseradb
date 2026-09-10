import json,os,sys,re
from collections import defaultdict
sys.path.insert(0,'/tmp/claude-1000/-home-joe-code-tessera/973cd74f-37a3-482c-acd7-5b3858baf8e0/scratchpad')
from classify import kind

BUCKET = {
 "view segment: columns.arrow":"A viewport",
 "view segment: morton.u32":"A viewport",
 "view: permutation.bin":"A viewport",
 "access terms: postings.arrow":"A viewport",
 "term dictionary":"A viewport",
 "manifest / CURRENT":"A viewport",
 "SEGMENTS-*.json":"A viewport",
 "view segment: published.roaring":"A viewport",
 "view segment: elevation.roaring":"A viewport",
 "view segment: dem.roaring":"A viewport",
 "artifact members (.tsmb)":"B layers",
 "row-column (.tslb)":"B layers",
 "artifact tile-index (.tsti)":"B layers",
 "containment (.tscp)":"B layers",
 "record blob: artifact extents":"B layers",
 "attr column: values.arrow":"C filter/category",
 "attr column: presence.roaring":"C filter/category",
 "attr column: postings.arrow":"C filter/category",
 "attr column: dict.bin":"C filter/category",
 "view: row-entity.u32":"C filter/category",
 "record blob: blocks.bin":"D drill-down",
 "record blob: directory.arrow":"D drill-down",
 "record blob: hasrow.roaring":"D drill-down",
 "entity->terms (offsets/terms/hasrow)":"D drill-down",
 "entities: external-ids-*.arrow":"D drill-down",
 "entities: ext-locator.u32":"D drill-down",
 "access terms: pairs.parquet (oracle)":"E never at serve",
 "reports (build only)":"E never at serve",
}

def bucket(k):
    return BUCKET.get(k, "? "+k)

def walk(bundle):
    cur=json.load(open(os.path.join(bundle,"CURRENT"))); pref=cur["prefix"]
    man=json.load(open(os.path.join(bundle,pref,"MANIFEST.json"))); n=man["entity_id_high_water"]
    agg=defaultdict(int); tot=0
    for root,dirs,files in os.walk(bundle):
        for f in files:
            fp=os.path.join(root,f); rel=os.path.relpath(fp,bundle)
            rel2=rel[len(pref)+1:] if rel.startswith(pref+"/") else rel
            b=os.stat(fp).st_blocks*512
            agg[bucket(kind(rel2))]+=b; tot+=b
    return n,tot,agg

def from_tsv(path,n):
    agg=defaultdict(int); tot=0
    for l in list(open(path))[1:]:
        first,last,size,blocks,p=l.rstrip("\n").split("\t")
        if p.startswith(".build-tmp/") or p.endswith((".spool",".spill",".scratch")): continue
        rel=p[len("v00000/"):] if p.startswith("v00000/") else p
        b=int(blocks); agg[bucket(kind(rel))]+=b; tot+=b
    return n,tot,agg

rows=[]
rows.append(("gbif 125.8M (probe)",)+from_tsv("probes/2026-09-10-build-disk/200m-after.files.tsv",125789091))
for b in sys.argv[1:]:
    rows.append((b,)+walk(b))
order=["A viewport","B layers","C filter/category","D drill-down","E never at serve"]
for name,n,tot,agg in rows:
    print(f"\n== {name}  items={n:,}  total {tot/1e6:.1f} MB = {tot/n:.2f} B/item")
    for k in order+[x for x in agg if x not in order]:
        if k in agg:
            print(f"   {k:22s} {agg[k]/1e6:>9.2f} MB  {agg[k]/n:>7.3f} B/item  {100*agg[k]/tot:>5.1f}%")
