import sys, os, struct, random
import pyarrow as pa, pyarrow.ipc as ipc
from pyarrow import Codec

def read_dir(p):
    with pa.memory_map(p,'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
    return t

root, n, nsample = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
d = os.path.join(root,'v00000/partitions/default/attrs/record')
t = read_dir(os.path.join(d,'directory.arrow'))
off = t.column('compressed_offset').to_pylist()
cl  = t.column('compressed_len').to_pylist()
unc = t.column('uncompressed_len').to_pylist()
nb = len(off)
random.seed(0)
idx = list(range(nb)) if nb <= nsample else sorted(random.sample(range(nb), nsample))
codec = Codec('zstd')
f = open(os.path.join(d,'blocks.bin'),'rb')
tot_chars=0; tot_framed=0; tot_rows=0; tot_comp=0
tags={}
kinds={}
for b in idx:
    f.seek(off[b]); raw=f.read(cl[b])
    buf = codec.decompress(pa.py_buffer(raw), decompressed_size=unc[b]).to_pybytes()
    tot_framed += len(buf); tot_comp += cl[b]
    p=0; L=len(buf)
    while p < L:
        ent, plen = struct.unpack_from('<II', buf, p); p += 8
        end = p + plen
        tot_rows += 1
        while p < end:
            tag, kind = struct.unpack_from('<HB', buf, p); p += 3
            kinds[kind]=kinds.get(kind,0)+1
            if kind == 12:
                bl, = struct.unpack_from('<I', buf, p); p += 4
                tot_chars += bl; p += bl
                tags[tag] = tags.get(tag,0) + bl
            elif kind == 0 or kind == 1 or kind == 5: p += 1
            elif kind in (2,6): p += 2
            elif kind in (3,7,9): p += 4
            elif kind in (4,8,11,10): p += 8
            else: raise SystemExit('kind %d unsupported' % kind)
        assert p == end, (p,end)
f.close()
frac = tot_framed / sum(unc)
print("root %s  blocks sampled %d of %d (%.2f%% of uncompressed bytes)" % (root, len(idx), nb, 100*frac))
print("  rows sampled %d ; chars %d ; framed %d" % (tot_rows, tot_chars, tot_framed))
print("  chars / framed = %.4f ; framing overhead = %.2f B/row" % (tot_chars/tot_framed, (tot_framed-tot_chars)/tot_rows))
est_chars = tot_chars / frac
print("  ESTIMATED whole-blob chars %.0f = %.3f B/item" % (est_chars, est_chars/n))
print("  blocks.bin / chars = %.4f" % (os.path.getsize(os.path.join(d,'blocks.bin')) / est_chars))
print("  per-tag chars B/item:", {k: round(v/frac/n,3) for k,v in sorted(tags.items())})
print("  kind histogram:", dict(sorted(kinds.items())))
