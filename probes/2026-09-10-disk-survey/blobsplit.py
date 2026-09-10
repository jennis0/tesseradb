import sys, os, struct, random
import pyarrow as pa, pyarrow.ipc as ipc
from pyarrow import Codec

def read_dir(p):
    with pa.memory_map(p,'r') as src:
        try: return ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); return ipc.open_stream(src).read_all()

root, n, nsample = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
d = os.path.join(root,'v00000/partitions/default/attrs/record')
t = read_dir(os.path.join(d,'directory.arrow'))
off = t.column('compressed_offset').to_pylist(); cl = t.column('compressed_len').to_pylist(); unc = t.column('uncompressed_len').to_pylist()
nb=len(off); random.seed(1)
idx = list(range(nb)) if nb<=nsample else sorted(random.sample(range(nb), nsample))
codec = Codec('zstd')   # pyarrow default level is 1; use explicit level 3
try:
    codec3 = Codec('zstd', compression_level=3)
except TypeError:
    codec3 = codec
f=open(os.path.join(d,'blocks.bin'),'rb')
per_tag_raw = {}
per_tag_buf = {}
tot_unc=0; tot_c=0; tot_recomp=0
for b in idx:
    f.seek(off[b]); raw=f.read(cl[b])
    buf = codec.decompress(pa.py_buffer(raw), decompressed_size=unc[b]).to_pybytes()
    tot_unc += len(buf); tot_c += cl[b]
    tot_recomp += len(codec3.compress(pa.py_buffer(buf)))
    parts = {}
    p=0; L=len(buf)
    while p<L:
        ent, plen = struct.unpack_from('<II', buf, p); p+=8
        end=p+plen
        while p<end:
            tag, kind = struct.unpack_from('<HB', buf, p); s=p; p+=3
            if kind==12:
                bl,=struct.unpack_from('<I', buf, p); p+=4+bl
            elif kind in (0,1,5): p+=1
            elif kind in (2,6): p+=2
            elif kind in (3,7,9): p+=4
            else: p+=8
            parts.setdefault(tag, bytearray()).extend(buf[s:p])
            per_tag_raw[tag] = per_tag_raw.get(tag,0) + (p-s)
    for tag, ba in parts.items():
        per_tag_buf[tag] = per_tag_buf.get(tag,0) + len(codec3.compress(pa.py_buffer(bytes(ba))))
f.close()
frac = tot_unc/sum(unc)
print("root %s  sampled %d blocks (%.3f%% of the blob)" % (root, len(idx), 100*frac))
print("  whole-block: uncompressed %d -> shipped %d (recompressed here %d, %.3fx of shipped)" % (tot_unc, tot_c, tot_recomp, tot_recomp/tot_c))
print("  per-tag, each field's bytes compressed alone at level 3:")
tot_alone=0
for tag in sorted(per_tag_buf):
    tot_alone += per_tag_buf[tag]
    print("     tag %d: framed %12d (%.2f B/item) -> alone %12d (%.3f B/item)" % (tag, per_tag_raw[tag], per_tag_raw[tag]/frac/n, per_tag_buf[tag], per_tag_buf[tag]/frac/n))
print("  sum of the parts %d vs the whole %d = %.3fx (joint compression's credit)" % (tot_alone, tot_c, tot_alone/tot_c))
