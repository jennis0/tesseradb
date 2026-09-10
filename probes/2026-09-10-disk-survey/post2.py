import sys, os, glob, math, numpy as np
import pyarrow as pa, pyarrow.ipc as ipc

def roaring_card(buf, pos):
    cookie = int.from_bytes(buf[pos:pos+4],'little'); pos+=4
    if cookie == 12346:
        size = int.from_bytes(buf[pos:pos+4],'little'); pos+=4
    elif (cookie & 0xFFFF) == 12347:
        size = (cookie >> 16) + 1
        pos += (size + 7)//8
    else:
        raise ValueError('cookie %d' % cookie)
    hdr = np.frombuffer(buf[pos:pos+4*size], dtype='<u2')
    return int((hdr[1::2].astype(np.int64) + 1).sum())

root=sys.argv[1]; n=int(sys.argv[2]); N=int(sys.argv[3])  # N = entity space size for the bound
only = sys.argv[4] if len(sys.argv)>4 else None
for p in sorted(glob.glob(root+'/v00000/partitions/*/attrs/*/postings.arrow')):
    col = p.split('/attrs/')[1].split('/')[0]
    if only and col != only: continue
    sz = os.path.getsize(p)
    with pa.memory_map(p,'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
        a = t.column('posting').combine_chunks()
        bufs=[b for b in a.buffers() if b is not None]
        offs = np.frombuffer(bufs[-2], dtype='<i8')[a.offset:a.offset+len(a)+1]
        vals = np.frombuffer(bufs[-1], dtype=np.uint8)
        vb = memoryview(bufs[-1])
        lens = np.diff(offs); tags = vals[offs[:-1]]
        cards = np.zeros(len(a), dtype=np.int64)
        cards[tags==0] = (lens[tags==0]-1)//4
        for i in np.nonzero(tags==1)[0]:
            cards[i] = roaring_card(vb, int(offs[i])+1)
    c = cards[cards>0].astype(np.float64)
    f = c/N
    H = -(f*np.log2(f) + (1-f)*np.log2(1-f))
    bits = float((N*H).sum())
    tot = int(cards.sum())
    print("%s %s: terms %d, postings %d (%.2f/item)" % (root.split('/')[-2], col, len(a), tot, tot/n))
    print("   spent  %14d B = %.3f B/item = %.3f B/posting" % (sz, sz/n, sz/tot))
    print("   0-order entropy floor over the whole entity space: %.0f B = %.3f B/item = %.3f B/posting  (ratio spent/floor %.2fx)"
          % (bits/8, bits/8/n, bits/8/tot, sz/(bits/8)))
