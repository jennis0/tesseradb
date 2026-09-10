import sys, os, glob, numpy as np
import pyarrow as pa, pyarrow.ipc as ipc

def compact(v):  # gather even bits of 32-bit into low 16
    x = v & np.uint32(0x55555555)
    x = (x | (x >> np.uint32(1))) & np.uint32(0x33333333)
    x = (x | (x >> np.uint32(2))) & np.uint32(0x0F0F0F0F)
    x = (x | (x >> np.uint32(4))) & np.uint32(0x00FF00FF)
    x = (x | (x >> np.uint32(8))) & np.uint32(0x0000FFFF)
    return x

root=sys.argv[1]
for segdir in sorted(glob.glob(root+'/v00000/partitions/*/views/*/segments/*')):
    view = segdir.split('/views/')[1].split('/')[0]
    m = np.array(np.memmap(os.path.join(segdir,'morton.u32'), dtype='<u4', mode='r'))
    with pa.memory_map(os.path.join(segdir,'columns.arrow'),'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
        res = t.column('residual').combine_chunks().to_numpy(zero_copy_only=False).astype(np.uint32)
    qx = (compact(m).astype(np.uint64) << np.uint64(16)) | compact(res).astype(np.uint64)
    qy = (compact(m >> np.uint32(1)).astype(np.uint64) << np.uint64(16)) | compact(res >> np.uint32(1)).astype(np.uint64)
    out=[]
    for nm, q in (('x',qx),('y',qy)):
        u = np.unique(q)
        d = np.diff(u)
        d = d[d>0]
        g = int(np.gcd.reduce(d)) if len(d) else 0
        out.append("%s: distinct %d (log2 %.1f), min step %d, gcd step %d -> %.1f usable bits"
                   % (nm, len(u), np.log2(len(u)) if len(u) else 0, int(d.min()) if len(d) else 0, g,
                      32 - (np.log2(g) if g>0 else 0)))
    print("%-40s view=%-8s rows=%d" % (root, view, len(m)))
    for o in out: print("     ", o)
