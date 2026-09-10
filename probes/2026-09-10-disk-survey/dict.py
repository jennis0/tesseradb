import sys, os, glob, struct
root=sys.argv[1]; n=int(sys.argv[2])
for p in sorted(glob.glob(root+'/v00000/partitions/*/attrs/*/dict.bin')):
    col = p.split('/attrs/')[1].split('/')[0]
    sz = os.path.getsize(p)
    with open(p,'rb') as f:
        f.seek(-28, 2); foot = f.read(28)
    ver, K, keys, blocks, blen = struct.unpack('<IIIIQ', foot[:24])
    restarts = 8*(blocks+1)
    print("%-14s %-16s %12d B  %.3f B/item | keys %d  K=%d  blocks %d" % (root.split('/')[-2], col, sz, sz/n, keys, K, blocks))
    print("                 front-coded blocks %d B (%.2f B/key) ; restart table %d B (%.2f B/key) ; footer 28" % (blen, blen/max(keys,1), restarts, restarts/max(keys,1)))
