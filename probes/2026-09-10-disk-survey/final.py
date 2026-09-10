import os, sys, json, re, glob, collections
import pyarrow as pa, pyarrow.ipc as ipc

def arrow_col_bytes(p):
    with pa.memory_map(p,'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
    out={}
    for f in t.schema:
        c=t.column(f.name)
        out[f.name]=sum(b.size for ch in c.chunks for b in ch.buffers() if b is not None)
    return out, t.num_rows, os.path.getsize(p)

def decompose(root, n, label):
    man = json.load(open(glob.glob(root+'/v*/MANIFEST.json')[0]))
    decl = {d['name']: d for d in man.get('declared_scalars',[])}
    groups = collections.defaultdict(int)
    detail = collections.defaultdict(int)
    for dirpath,_,files in os.walk(root):
        for fn in files:
            fp=os.path.join(dirpath,fn)
            sz=os.lstat(fp).st_size
            rel=re.sub(r'^v\d+/','',os.path.relpath(fp,root))
            q = rel.split('/')
            g=None
            if rel.endswith('MANIFEST.json') or rel=='CURRENT' or q[0]=='reports': g='manifest, reports'
            elif q[0]=='dictionary': g='global term dictionary'
            elif q[0]=='partitions':
                r=q[2:]
                if r[0]=='attrs':
                    name=r[1]
                    if name=='record': g='record blob'
                    else:
                        ty = decl.get(name,{}).get('arrow_type','?')
                        f=r[-1]
                        if ty=='text': g='text index (dict + postings)'
                        elif ty=='keyword': g='keyword dict + ordinals'
                        else: g='scalar values + presence + category postings'
                        detail['%s:%s:%s'%(g,name,f)] += sz
                elif r[0]=='views':
                    f=r[-1]
                    if f=='morton.u32': g='geometry: morton cell'
                    elif f=='columns.arrow':
                        cols,rows,tot = arrow_col_bytes(fp)
                        tid=cols.pop('tessera_id',0); res=cols.pop('residual',0)
                        rend=sum(cols.values())
                        pad = tot-(tid+res+rend)
                        groups['geometry: residual'] += res
                        groups['wire identity (tessera_id)'] += tid
                        groups['render columns'] += rend + pad
                        continue
                    elif f in ('permutation.bin','row-entity.u32'): g='view maps (permutation, row-entity)'
                    else: g='view maps (permutation, row-entity)'
                elif r[0]=='terms': g='access terms (+ oracle pairs)'
                elif r[0] in ('members','containment','tile-index','row-column','artifacts'): g='layers (members, row-column, containment, tile index)'
                elif r[0]=='entities':
                    f=r[-1]
                    if f.startswith('external-ids') or f=='ext-locator.u32': g='external-id sidecar (opt-in)'
                    else: g='labels (entities/terms)'
                else: g='other: '+r[0]
            else: g='other: '+rel
            groups[g]+=sz
    tot=sum(groups.values())
    print("\n### %s — %d items, %d B (%.2f GB), %.1f B/item, bundle_format %d" % (label, n, tot, tot/1e9, tot/n, man['bundle_format']))
    print("| kind | bytes | B/item | %% of bundle |")
    print("|---|---|---|---|")
    for k,v in sorted(groups.items(), key=lambda kv:-kv[1]):
        print("| %s | %s | %.2f | %.1f%% |" % (k, "{:,}".format(v), v/n, 100*v/tot))
    print("| **total** | **%s** | **%.2f** | **100%%** |" % ("{:,}".format(tot), tot/n))
    return groups, tot

if __name__=='__main__':
    for root,n,label in [
        ('data/ladder/gbif-64p/bundle',25846007,'gbif (64-part spread)'),
        ('data/ladder/geonames/bundle-final',13463857,'geonames'),
        ('data/ladder/treeoflife-1m/bundle',1000000,'treeoflife-1m'),
        ('data/ladder/medcpt-1m/bundle-probe',1000000,'medcpt-1m (no abstracts)'),
        ('data/ladder/medcpt-1m-abs/bundle-probe',1000000,'medcpt-1m-abs'),
        ('data/ladder/medcpt-10m-abs/bundle-auto',10000000,'medcpt-10m-abs'),
        ('data/rung4-run/bundle',102117343,'paperseek (rung 4, whole)'),
    ]:
        decompose(root,n,label)
