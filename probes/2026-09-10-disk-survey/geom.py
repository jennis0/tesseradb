import sys, os, glob, numpy as np
import pyarrow as pa, pyarrow.ipc as ipc
root=sys.argv[1]; n=int(sys.argv[2])
for segdir in sorted(glob.glob(root+'/v00000/partitions/*/views/*/segments/*')):
    view = segdir.split('/views/')[1].split('/')[0]
    m = np.memmap(os.path.join(segdir,'morton.u32'), dtype='<u4', mode='r')
    with pa.memory_map(os.path.join(segdir,'columns.arrow'),'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
        res = t.column('residual').combine_chunks().to_numpy(zero_copy_only=False).astype('<u4')
    rows = len(m)
    dm = np.unique(m).size
    dr = np.unique(res).size
    # how many low bits of the residual are ever non-zero
    orr = np.bitwise_or.reduce(res)
    lowbits = 0
    v = int(orr)
    lz = 0
    while lz < 32 and not (v >> lz) & 1: lz += 1
    print("%-10s view=%-8s rows=%d" % (root.split('/')[-2], view, rows))
    print("   distinct morton cells %d (%.4f of rows) ; mean pts/occupied cell %.2f" % (dm, dm/rows, rows/dm))
    print("   distinct residuals %d ; OR of all residuals 0x%08x ; lowest set bit %d" % (dr, orr, lz))
    # occupancy at coarser depths
    for d in (8, 10, 12, 14, 16):
        shift = 32 - 2*d
        c = np.unique(m >> shift).size if shift else np.unique(m).size
        print("      depth %2d: %d occupied tiles (%d per axis)" % (d, c, 1<<d))
