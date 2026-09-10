import sys, os, glob
import pyarrow as pa, pyarrow.ipc as ipc
root=sys.argv[1]; n=int(sys.argv[2])
for p in sorted(glob.glob(root+'/v00000/partitions/*/views/*/segments/*/columns.arrow')):
    with pa.memory_map(p,'r') as src:
        try: r=ipc.open_file(src); t=r.read_all()
        except pa.ArrowInvalid:
            src.seek(0); r=ipc.open_stream(src); t=r.read_all()
        print(p.split('/views/')[1], os.path.getsize(p), "%.3f B/item"%(os.path.getsize(p)/n), "rows", t.num_rows)
        for f in t.schema:
            col = t.column(f.name)
            nb = sum(b.size for c in col.chunks for b in c.buffers() if b is not None)
            print("    %-22s %-16s %12d  %.3f B/row" % (f.name, f.type, nb, nb/t.num_rows))
