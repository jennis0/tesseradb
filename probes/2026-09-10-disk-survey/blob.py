import sys, os, glob
import pyarrow as pa
import pyarrow.ipc as ipc

def read_dir(p):
    with pa.memory_map(p, 'r') as src:
        try:
            r = ipc.open_file(src)
            tbl = r.read_all()
        except pa.ArrowInvalid:
            src.seek(0)
            r = ipc.open_stream(src)
            tbl = r.read_all()
    return tbl

root = sys.argv[1]; n = int(sys.argv[2])
d = os.path.join(root, 'v00000/partitions/default/attrs/record')
dirf = os.path.join(d, 'directory.arrow')
blocks = os.path.join(d, 'blocks.bin')
t = read_dir(dirf)
cols = t.column_names
unc = t.column('uncompressed_len').to_pylist()
cl = t.column('compressed_len').to_pylist()
rowoff = t.column(cols[-1]) if 'row_offsets' not in cols else t.column('row_offsets')
nblocks = len(unc)
tot_unc = sum(unc); tot_c = sum(cl)
bsz = os.path.getsize(blocks)
dsz = os.path.getsize(dirf)
hz = os.path.getsize(os.path.join(d,'hasrow.roaring'))
# rows
try:
    nrows = sum(len(x) for x in rowoff.to_pylist())
except Exception:
    nrows = None
print("root", root)
print("  columns", cols)
print("  blocks", nblocks, "rows-with-blob", nrows)
print("  framed row bytes (sum uncompressed_len) %d  = %.3f B/item" % (tot_unc, tot_unc/n))
print("  blocks.bin %d (%.3f B/item)  directory.arrow %d (%.3f)  hasrow %d" % (bsz, bsz/n, dsz, dsz/n, hz))
print("  sum compressed_len %d  (== blocks.bin: %s)" % (tot_c, tot_c==bsz))
print("  zstd ratio framed/compressed = %.3fx  (compressed/framed = %.3f)" % (tot_unc/tot_c, tot_c/tot_unc))
print("  mean uncompressed block %.0f B (target 262144)" % (tot_unc/nblocks))
if nrows:
    print("  addressing B per has-row entity = %.3f  (directory+hasrow)/rows" % ((dsz+hz)/nrows))
    print("  framed bytes per row %.1f ; compressed bytes per row %.2f" % (tot_unc/nrows, tot_c/nrows))
