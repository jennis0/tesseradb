import sys, os, struct, random
import pyarrow as pa, pyarrow.ipc as ipc
from pyarrow import Codec
def read_dir(p):
    with pa.memory_map(p,'r') as src:
        try: return ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); return ipc.open_stream(src).read_all()
root,n,ns = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
d=os.path.join(root,'v00000/partitions/default/attrs/record')
t=read_dir(os.path.join(d,'directory.arrow'))
off=t.column('compressed_offset').to_pylist(); cl=t.column('compressed_len').to_pylist(); unc=t.column('uncompressed_len').to_pylist()
nb=len(off); random.seed(2)
idx=list(range(nb)) if nb<=ns else sorted(random.sample(range(nb),ns))
c=Codec('zstd'); c3=Codec('zstd', compression_level=3)
f=open(os.path.join(d,'blocks.bin'),'rb')
tu=0; tc=0; chars_only=0; chars_c=0; nrows=0
for b in idx:
    f.seek(off[b]); buf=c.decompress(pa.py_buffer(f.read(cl[b])), decompressed_size=unc[b]).to_pybytes()
    tu+=len(buf); tc+=cl[b]
    out=bytearray(); p=0; L=len(buf)
    while p<L:
        ent,plen=struct.unpack_from('<II',buf,p); p+=8; end=p+plen; nrows+=1
        while p<end:
            tag,kind=struct.unpack_from('<HB',buf,p); p+=3
            if kind==12:
                bl,=struct.unpack_from('<I',buf,p); p+=4
                out.extend(buf[p:p+bl]); p+=bl
            elif kind in (0,1,5): out.extend(buf[p:p+1]); p+=1
            elif kind in (2,6): out.extend(buf[p:p+2]); p+=2
            elif kind in (3,7,9): out.extend(buf[p:p+4]); p+=4
            else: out.extend(buf[p:p+8]); p+=8
    chars_only+=len(out); chars_c+=len(c3.compress(pa.py_buffer(bytes(out))))
f.close()
frac=tu/sum(unc)
print("%s: %d blocks (%.3f%% of the blob), %d rows" % (root, len(idx), 100*frac, nrows))
print("  framed %d -> shipped %d (%.4f) ; values only %d -> %d (%.4f)" % (tu, tc, tc/tu, chars_only, chars_c, chars_c/chars_only))
print("  framing costs %d compressed B over the sample = %.3f B/item, %.2f%% of the shipped blocks"
      % (tc-chars_c, (tc-chars_c)/frac/n, 100*(tc-chars_c)/tc))
