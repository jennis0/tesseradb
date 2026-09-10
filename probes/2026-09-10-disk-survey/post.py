import sys, os, glob, numpy as np
import pyarrow as pa, pyarrow.ipc as ipc

def roaring_card(buf, pos, end):
    # portable roaring: cookie u32
    cookie = int.from_bytes(buf[pos:pos+4],'little'); pos+=4
    NO_RUN = 12346; RUN = 12347
    if cookie == NO_RUN:
        size = int.from_bytes(buf[pos:pos+4],'little'); pos+=4
        hasrun = False
    elif (cookie & 0xFFFF) == RUN:
        size = (cookie >> 16) + 1
        hasrun = True
        pos += (size + 7)//8
    else:
        raise ValueError('cookie %d' % cookie)
    hdr = np.frombuffer(buf[pos:pos+4*size], dtype='<u2')
    cards = hdr[1::2].astype(np.int64) + 1
    return int(cards.sum()), size

root=sys.argv[1]; n=int(sys.argv[2])
for p in sorted(glob.glob(root+'/v00000/partitions/*/attrs/*/postings.arrow')):
    col = p.split('/attrs/')[1].split('/')[0]
    sz = os.path.getsize(p)
    with pa.memory_map(p,'r') as src:
        try: t = ipc.open_file(src).read_all()
        except pa.ArrowInvalid:
            src.seek(0); t = ipc.open_stream(src).read_all()
        keyed = 'term_id' in t.column_names
        a = t.column('posting').combine_chunks()
        bufs = [b for b in a.buffers() if b is not None]
        if str(a.type).startswith('large'):
            offs = np.frombuffer(bufs[-2], dtype='<i8')[a.offset:a.offset+len(a)+1]
        else:
            offs = np.frombuffer(bufs[-2], dtype='<i4')[a.offset:a.offset+len(a)+1].astype(np.int64)
        vals = np.frombuffer(bufs[-1], dtype=np.uint8)
        nterms = len(a)
        lens = np.diff(offs)
        tags = vals[offs[:-1]]
        n0 = int((tags==0).sum()); n1 = int((tags==1).sum())
        card0 = int(((lens[tags==0]-1)//4).sum())
        # roaring: parse each
        card1 = 0; conts = 0
        vb = memoryview(bufs[-1])
        idx = np.nonzero(tags==1)[0]
        for i in idx:
            c, s = roaring_card(vb, int(offs[i])+1, int(offs[i+1]))
            card1 += c; conts += s
        tot = card0 + card1
        b0 = int(lens[tags==0].sum()); b1 = int(lens[tags==1].sum())
    print("%s %s" % (root, col))
    print("   file %d B (%.3f B/item) ; terms %d ; postings %d (%.2f per item)" % (sz, sz/n, nterms, tot, tot/n))
    print("   arrow offsets %d B (8/term) ; payload %d B ; keyed=%s (+4 B/term key)" % (8*(nterms+1), b0+b1, keyed))
    print("   tag0 (small list) terms %d postings %d bytes %d (%.2f B/posting)" % (n0, card0, b0, b0/max(card0,1)))
    print("   tag1 (roaring)    terms %d postings %d bytes %d (%.3f B/posting) containers %d" % (n1, card1, b1, b1/max(card1,1), conts))
    print("   whole file per posting %.3f B" % (sz/max(tot,1)))
