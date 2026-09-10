"""Decode TSMB membership extents: count artifacts, membership entries, and bytes.

Format (crates/tessera-store/src/membership.rs module doc):
  header  := "TSMB" | u16 version | u16 reserved | u32 count | u32 ordinal_lo
  offsets := u64 LE x (count+1)
  payload := count blobs

Blob (tessera-lifecycle::membership::encode_record):
  u16 key_len | key | u16 view_len | view | u16 content_count | u32 members_len | members ...

members := CRoaring portable serialisation.
"""
import struct, sys, os, glob

SERIAL_COOKIE_NO_RUNCONTAINER = 12346
SERIAL_COOKIE = 12347
NO_OFFSET_THRESHOLD = 4


def roaring_stats(b):
    """Return (cardinality, n_containers, kinds dict, container_payload_bytes)."""
    off = 0
    cookie, = struct.unpack_from('<I', b, off); off += 4
    if (cookie & 0xFFFF) == SERIAL_COOKIE:
        size = (cookie >> 16) + 1
        run_flag_bytes = (size + 7) // 8
        flags = b[off:off + run_flag_bytes]; off += run_flag_bytes
        has_run = True
    elif cookie == SERIAL_COOKIE_NO_RUNCONTAINER:
        size, = struct.unpack_from('<I', b, off); off += 4
        flags = b'\x00' * ((size + 7) // 8)
        has_run = False
    else:
        raise ValueError('bad roaring cookie %d' % cookie)
    keys = []
    cards = []
    for i in range(size):
        k, c = struct.unpack_from('<HH', b, off); off += 4
        keys.append(k); cards.append(c + 1)
    if (not has_run) or size >= NO_OFFSET_THRESHOLD:
        off += 4 * size
    kinds = {'array': 0, 'bitset': 0, 'run': 0}
    payload = 0
    for i in range(size):
        is_run = has_run and (flags[i >> 3] >> (i & 7)) & 1
        c = cards[i]
        if is_run:
            nruns, = struct.unpack_from('<H', b, off)
            nbytes = 2 + 4 * nruns
            kinds['run'] += 1
        elif c <= 4096:
            nbytes = 2 * c
            kinds['array'] += 1
        else:
            nbytes = 8192
            kinds['bitset'] += 1
        off += nbytes
        payload += nbytes
    return sum(cards), size, kinds, payload, off


def _cookie_ok(b, at):
    if at + 4 > len(b):
        return False
    c, = struct.unpack_from('<I', b, at)
    return c == SERIAL_COOKIE_NO_RUNCONTAINER or (c & 0xFFFF) == SERIAL_COOKIE


def blob_members(blob):
    """Locate the membership bytes. bundle_format >= 8 carries a view name between the key and
    the content count; earlier formats do not. Both are tried and the one whose members start
    with a valid Roaring cookie wins."""
    kl, = struct.unpack_from('<H', blob, 0)
    base = 2 + kl
    # layout A: view present
    try:
        vl, = struct.unpack_from('<H', blob, base)
        a = base + 2 + vl
        cc, = struct.unpack_from('<H', blob, a)
        a += 2
        ml, = struct.unpack_from('<I', blob, a)
        a += 4
        if ml <= len(blob) - a and _cookie_ok(blob, a):
            return blob[a:a + ml], 'A'
    except struct.error:
        pass
    # layout B: no view
    b = base
    cc, = struct.unpack_from('<H', blob, b)
    b += 2
    ml, = struct.unpack_from('<I', blob, b)
    b += 4
    if ml <= len(blob) - b and _cookie_ok(blob, b):
        return blob[b:b + ml], 'B'
    raise ValueError('cannot locate members in blob of %d bytes: %s' % (len(blob), blob[:32].hex()))


def read_extent(path):
    with open(path, 'rb') as f:
        data = f.read()
    assert data[0:4] == b'TSMB', path
    version, = struct.unpack_from('<H', data, 4)
    count, = struct.unpack_from('<I', data, 8)
    ordinal_lo, = struct.unpack_from('<I', data, 12)
    offs_at = 16
    payload_at = offs_at + (count + 1) * 8
    offs = struct.unpack_from('<%dQ' % (count + 1), data, offs_at)
    total_card = 0
    total_member_bytes = 0
    total_blob_bytes = 0
    kinds = {'array': 0, 'bitset': 0, 'run': 0}
    containers = 0
    biggest = 0
    nonempty = 0
    for i in range(count):
        blob = data[payload_at + offs[i]: payload_at + offs[i + 1]]
        total_blob_bytes += len(blob)
        mb, layout = blob_members(blob)
        total_member_bytes += len(mb)
        card, nc, k, pay, used = roaring_stats(mb)
        total_card += card
        containers += nc
        for key in kinds:
            kinds[key] += k[key]
        if card:
            nonempty += 1
        biggest = max(biggest, card)
    return dict(path=path, file_bytes=len(data), version=version, artifacts=count,
                ordinal_lo=ordinal_lo, entries=total_card, member_bytes=total_member_bytes,
                blob_bytes=total_blob_bytes, offsets_bytes=(count + 1) * 8,
                containers=containers, kinds=kinds, biggest=biggest, nonempty=nonempty)


def summarise(bundle):
    files = sorted(glob.glob(os.path.join(bundle, '*/partitions/*/members/*.tsmb')))
    agg = dict(file_bytes=0, artifacts=0, entries=0, member_bytes=0, blob_bytes=0,
               offsets_bytes=0, containers=0, biggest=0, nonempty=0)
    kinds = {'array': 0, 'bitset': 0, 'run': 0}
    per = []
    for p in files:
        r = read_extent(p)
        per.append(r)
        for k in ('file_bytes', 'artifacts', 'entries', 'member_bytes', 'blob_bytes',
                  'offsets_bytes', 'containers', 'nonempty'):
            agg[k] += r[k]
        agg['biggest'] = max(agg['biggest'], r['biggest'])
        for k in kinds:
            kinds[k] += r['kinds'][k]
    agg['kinds'] = kinds
    return agg, per


if __name__ == '__main__':
    for bundle in sys.argv[1:]:
        agg, per = summarise(bundle)
        e = agg['entries'] or 1
        print(bundle)
        print('  files=%d artifacts=%d nonempty=%d entries=%d' % (len(per), agg['artifacts'], agg['nonempty'], agg['entries']))
        print('  tsmb bytes=%d  member(roaring) bytes=%d  blob bytes=%d  offset table=%d' %
              (agg['file_bytes'], agg['member_bytes'], agg['blob_bytes'], agg['offsets_bytes']))
        print('  B/entry: file %.3f  roaring %.3f' % (agg['file_bytes'] / e, agg['member_bytes'] / e))
        print('  containers=%d  %s  biggest artifact=%d' % (agg['containers'], agg['kinds'], agg['biggest']))
        for r in per:
            print('    %-50s art=%-8d entries=%-12d roaring=%-12d B/e=%.3f' %
                  (os.path.basename(r['path']), r['artifacts'], r['entries'], r['member_bytes'],
                   r['member_bytes'] / (r['entries'] or 1)))
