import sys, os, glob, numpy as np
import pyarrow as pa, pyarrow.ipc as ipc
def roaring_card(buf,pos):
    ck=int.from_bytes(buf[pos:pos+4],'little'); pos+=4
    if ck==12346:
        size=int.from_bytes(buf[pos:pos+4],'little'); pos+=4
    elif (ck&0xFFFF)==12347:
        size=(ck>>16)+1; pos+=(size+7)//8
    else: raise ValueError(ck)
    hdr=np.frombuffer(buf[pos:pos+4*size],dtype='<u2')
    return int((hdr[1::2].astype(np.int64)+1).sum())
root=sys.argv[1]; n=int(sys.argv[2])
p=glob.glob(root+'/v*/partitions/*/terms/postings.arrow')[0]
with pa.memory_map(p,'r') as src:
    try: t=ipc.open_file(src).read_all()
    except pa.ArrowInvalid:
        src.seek(0); t=ipc.open_stream(src).read_all()
a=t.column('posting').combine_chunks()
bufs=[b for b in a.buffers() if b is not None]
offs=np.frombuffer(bufs[-2],dtype='<i8')[a.offset:a.offset+len(a)+1]
vals=np.frombuffer(bufs[-1],dtype=np.uint8); vb=memoryview(bufs[-1])
lens=np.diff(offs); tags=vals[offs[:-1]]
card=0
for i in range(len(a)):
    card += (lens[i]-1)//4 if tags[i]==0 else roaring_card(vb,int(offs[i])+1)
et = glob.glob(root+'/v*/partitions/*/entities/terms/*')
esz = sum(os.path.getsize(x) for x in et)
print("%-40s terms %d, access postings %d (%.3f/item)" % (root, len(a), card, card/n))
print("   terms/postings.arrow %d B (%.4f B/item)  |  entities/terms/ %d B (%.3f B/item) = %.0fx"
      % (os.path.getsize(p), os.path.getsize(p)/n, esz, esz/n, esz/os.path.getsize(p)))
