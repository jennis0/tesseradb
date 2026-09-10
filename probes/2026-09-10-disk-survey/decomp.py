import os, sys, json, re, collections

def kind(rel):
    p = rel.split('/')
    if rel.endswith('MANIFEST.json') or rel == 'CURRENT': return 'manifest / CURRENT'
    if p[0] == 'reports': return 'reports'
    if p[0] == 'dictionary': return 'global term dictionary'
    # partitions/<ph>/...
    if p[0] == 'partitions':
        q = p[2:]
        if q[0] == 'attrs':
            name = q[1]
            if name == 'record':
                if len(q) > 2 and q[2] == 'extents':
                    f = q[3]
                    if f.endswith('blocks.bin'): return 'record blob: artifact extent blocks'
                    if f.endswith('directory.arrow'): return 'record blob: artifact extent directory'
                    return 'record blob: artifact extent other'
                if q[2] == 'blocks.bin': return 'record blob: blocks'
                if q[2] == 'directory.arrow': return 'record blob: directory'
                return 'record blob: other'
            f = q[-1]
            if f == 'postings.arrow': return 'attr token/term index: postings'
            if f == 'dict.bin': return 'attr token/term index: dictionary'
            if f == 'values.arrow': return 'attr values (indexed scalar)'
            if f == 'presence.roaring': return 'attr presence'
            if f.endswith('.roaring'): return 'attr roaring other'
            return 'attr other'
        if q[0] == 'views':
            f = q[-1]
            if f == 'morton.u32': return 'geometry: morton'
            if f == 'columns.arrow': return 'render columns'
            if f == 'permutation.bin': return 'view: permutation'
            if f == 'row-entity.u32': return 'view: row-entity'
            if 'presence' in q: return 'view: presence roaring'
            if f.startswith('SEGMENTS'): return 'view: segments json'
            return 'view: other (%s)' % f
        if q[0] == 'terms':
            f = q[-1]
            if f == 'postings.arrow': return 'access terms: postings'
            if f == 'pairs.parquet': return 'access terms: pairs.parquet (oracle)'
            return 'access terms: other'
        if q[0] == 'members': return 'layer membership (.tsmb)'
        if q[0] == 'containment': return 'containment (.tscp)'
        if q[0] == 'tile-index': return 'tile index (.tsti)'
        if q[0] == 'row-column': return 'row-column (label/leaf)'
        if q[0] == 'entities':
            f = q[-1]
            if f == 'offsets.u32': return 'entity->terms: offsets'
            if f == 'terms.u32': return 'entity->terms: terms'
            if f.startswith('external-ids'): return 'external ids (arrow)'
            if f == 'ext-locator.u32': return 'external ids: locator'
            if f.endswith('.roaring'): return 'entity->terms: hasrow'
            return 'entity->terms: other (%s)' % f
        if q[0].startswith('SEGMENTS'): return 'segments json'
        if q[0] == 'artifacts': return 'artifacts'
        return 'partition other: ' + '/'.join(q[:-1])
    return 'other: ' + rel

root = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 0
rows = []
for dirpath, dirnames, filenames in os.walk(root):
    for f in filenames:
        fp = os.path.join(dirpath, f)
        st = os.lstat(fp)
        rel = os.path.relpath(fp, root)
        # strip leading vNNNNN/
        rel2 = re.sub(r'^v\d+/', '', rel)
        rows.append((kind(rel2), rel, st.st_size, st.st_blocks*512))
agg = collections.defaultdict(lambda: [0,0,0])
for k, rel, ap, al in rows:
    a = agg[k]; a[0]+=ap; a[1]+=al; a[2]+=1
tot_ap = sum(a[0] for a in agg.values()); tot_al = sum(a[1] for a in agg.values())
print("ROOT", root, "items", n)
print("TOTAL apparent %d (%.3f GB)  allocated %d (%.3f GB)" % (tot_ap, tot_ap/1e9, tot_al, tot_al/1e9))
print("%-44s %14s %14s %8s %8s %6s" % ("kind","apparent B","allocated B","B/item","%bundle","files"))
for k, a in sorted(agg.items(), key=lambda kv: -kv[1][0]):
    bpi = a[0]/n if n else 0
    print("%-44s %14d %14d %8.3f %7.2f%% %6d" % (k, a[0], a[1], bpi, 100*a[0]/tot_ap, a[2]))
print()
print("--- largest files ---")
for k, rel, ap, al in sorted(rows, key=lambda r: -r[2])[:15]:
    print("%14d %8.3f  %s" % (ap, ap/n if n else 0, rel))
