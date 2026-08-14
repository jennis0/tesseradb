"""What a keyword column costs under each candidate layout, and the list re-run on real values.

Two questions this measures, both feeding the records-and-search design:

1. **The keyword family's storage arithmetic.** A short exact-match string column (an ID, a
   submitter name, a DOI) could be stored four ways: the flat `utf8` column the scan reads today; a
   sorted front-coded dictionary plus a per-entity ordinal (dictionary encoding, bought for the
   sorted domain rather than for compression); the dictionary plus per-term postings; or a
   block-compressed record behind an exactly-answering index. The layouts compose differently —
   the ordinal column *is* a record, postings are not — so the comparison is between whole layouts,
   not parts.

2. **The list mechanism on real values** — the re-run `2026-08-12-record-and-searchability.md` §4
   asks for before the authors mechanism is fixed. `2026-08-12-filter-placement/` arm 3 measured
   CSR at 24.0 B/entity against postings at 32.3 on a *synthetic* surname-shaped column;
   `2026-08-12-string-storage/` measured real surnames' postings at 20.9. This closes the pair by
   computing both layouts on the same real values. (Timing is not re-run: arm 3's timing cells are
   over u32 codes in both layouts, and postings won every cell by 10-40x under a skew *less*
   favourable to them than the real one, so the timing verdict transfers; storage is what inverts
   with the vocabulary and is what needed real values.)

Postings are built with pyroaring and **serialised**, not modelled, matching the two earlier
campaigns. `hybrid` postings store a singleton term's single entity as a bare u32 (4 B) instead of
a serialised Roaring bitmap, plus one bit per term to say which encoding — the shape a
singleton-heavy vocabulary wants.

Scales are prefixes in snapshot order (= submission order = entity order), per `probes/dataset.md`
§5 rule 1.

Usage:
    reference/.venv/bin/python probes/2026-08-12-keyword-and-list-storage/keyword.py \
        --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json
"""

import argparse
import collections
import json
import os

import pyarrow as pa
from pyroaring import BitMap

ap = argparse.ArgumentParser()
ap.add_argument("--snapshot", required=True)
ap.add_argument("--scales", default="250000,1000000,2400000")
ap.add_argument("--limit", type=int, default=2_400_000)
args = ap.parse_args()

SCALES = [int(s) for s in args.scales.split(",")]
PATH = os.path.expanduser(args.snapshot)
ZSTD = pa.Codec("zstd")
BLOCK = 256 * 1024  # the 2.44x point from 2026-08-12-string-storage arm 1

ids, submitters, dois, surnames, cats = [], [], [], [], []
with open(PATH, "rb") as f:
    for line in f:
        if len(ids) >= args.limit:
            break
        try:
            d = json.loads(line)
        except Exception:
            continue
        ids.append(d.get("id") or "")
        submitters.append(d.get("submitter") or "")
        dois.append(d.get("doi") or "")
        surnames.append([a[0].lower() for a in (d.get("authors_parsed") or [])])
        cats.append((d.get("categories") or "").split())

print(f"read {len(ids):,} records (snapshot order = entity order)")


def front_coded(keys):
    """Bytes of a front-coded sorted dictionary: shared-prefix length byte + suffix per key."""
    total, prev = 0, ""
    for k in keys:
        j = 0
        while j < min(len(k), len(prev), 255) and k[j] == prev[j]:
            j += 1
        total += 1 + len(k[j:].encode())
        prev = k
    return total


def postings_cost(post):
    """(roaring bytes, hybrid bytes) for a {term: BitMap} map."""
    roaring = sum(len(bm.serialize()) for bm in post.values())
    hybrid = sum(4 if len(bm) == 1 else len(bm.serialize()) for bm in post.values())
    hybrid += (len(post) + 7) // 8  # one flag bit per term
    return roaring, hybrid


def zstd_blocks(values):
    """Block-compressed record: concatenated value bytes in 256 KiB blocks, plus an i64 offset
    per value into the uncompressed stream (the drill-down needs to find its value)."""
    blob, out, cur = [], 0, 0
    buf = bytearray()
    for v in values:
        buf += v
        if len(buf) >= BLOCK:
            out += len(ZSTD.compress(bytes(buf)))
            buf.clear()
    if buf:
        out += len(ZSTD.compress(bytes(buf)))
    return out + 8 * len(values)


def keyword_column(label, raw_values, n):
    """One single-valued keyword-shaped column at scale n."""
    values = raw_values[:n]
    present = [(e, v) for e, v in enumerate(values) if v]
    presence_bm = BitMap(e for e, _ in present)
    presence = 0 if len(present) == n else len(presence_bm.serialize())
    vals = [v for _, v in present]
    np = len(present)
    if np == 0:
        return

    flat = sum(len(v.encode()) for v in vals) + 8 * np

    keys = sorted(set(vals))
    dict_b = front_coded(keys)

    post = collections.defaultdict(BitMap)
    for e, v in present:
        post[v].add(e)
    roaring, hybrid = postings_cost(post)

    codes = 4 * np
    record = zstd_blocks([v.encode() for v in vals])

    singl = sum(1 for bm in post.values() if len(bm) == 1) / len(post)

    # The three whole layouts. Each must supply the record (entity -> value) and the
    # exact-match/prefix route; presence is common to all and reported separately.
    lay_flat = flat                       # record = the column; search = the text scan
    lay_dict = dict_b + codes             # record = ordinal -> dict; search = fixed-width scan
    lay_post = dict_b + hybrid + record   # record = compressed blocks; search = dict + postings

    print(
        f"{label:>10} {n:>10,} {len(keys):>10,} {np/n:>6.1%} {singl:>6.1%} "
        f"{flat/np:>7.1f} {dict_b/np:>6.1f} {roaring/np:>6.1f} {hybrid/np:>6.1f} {record/np:>6.1f} "
        f"| {lay_flat/np:>7.1f} {lay_dict/np:>7.1f} {lay_post/np:>7.1f} | {presence/n:>5.2f}"
    )


def list_column(label, raw_lists, n):
    """One list-shaped column at scale n: CSR-of-codes vs postings, both above the same dict."""
    lists = [sorted(set(v)) for v in raw_lists[:n]]
    pairs = sum(len(v) for v in lists)
    if pairs == 0:
        return
    flat = sum(len(" ".join(v).encode()) for v in lists if v) + 8 * sum(1 for v in lists if v)

    keys = sorted({t for v in lists for t in v})
    dict_b = front_coded(keys)

    post = collections.defaultdict(BitMap)
    for e, v in enumerate(lists):
        for t in v:
            post[t].add(e)
    roaring, hybrid = postings_cost(post)

    csr = 4 * (n + 1) + 4 * pairs  # offsets + one u32 code per (entity, value)
    singl = sum(1 for bm in post.values() if len(bm) == 1) / len(post)

    lay_csr = dict_b + csr            # record = CSR ordinals -> dict; search = CSR scan
    lay_post = dict_b + hybrid + csr  # postings as accelerator, CSR kept as the record
    lay_post_only = dict_b + hybrid   # postings alone (no per-entity record) — for reference

    print(
        f"{label:>10} {n:>10,} {len(keys):>10,} {pairs/n:>6.2f} {singl:>6.1%} "
        f"{flat/n:>7.1f} {dict_b/n:>6.1f} {roaring/n:>6.1f} {hybrid/n:>6.1f} {csr/n:>6.1f} "
        f"| {lay_csr/n:>7.1f} {lay_post/n:>7.1f} {lay_post_only/n:>7.1f} |"
    )


print(
    "\n### single-valued keyword columns  (B per present entity)\n\n"
    f"{'column':>10} {'scale':>10} {'distinct':>10} {'pres.':>6} {'singl':>6} "
    f"{'flat':>7} {'dict':>6} {'roar':>6} {'hybr':>6} {'zstd':>6} "
    f"| {'FLAT':>7} {'DICT+C':>7} {'IX+REC':>7} | {'prs/n':>5}"
)
for n in SCALES:
    if n > len(ids):
        continue
    keyword_column("id", ids, n)
    keyword_column("submitter", submitters, n)
    keyword_column("doi", dois, n)

print(
    "\n### list columns  (B per entity)\n\n"
    f"{'column':>10} {'scale':>10} {'distinct':>10} {'v/ent':>6} {'singl':>6} "
    f"{'flat':>7} {'dict':>6} {'roar':>6} {'hybr':>6} {'csr':>6} "
    f"| {'CSR':>7} {'CSR+IX':>7} {'IXonly':>7} |"
)
for n in SCALES:
    if n > len(ids):
        continue
    list_column("surnames", surnames, n)
    list_column("categories", cats, n)

print(
    "\nLayouts: FLAT = the utf8 column as shipped (search = text scan).\n"
    "DICT+C = front-coded sorted dictionary + one u32 ordinal per present entity\n"
    "         (record = ordinal lookup; eq/prefix = dictionary lookup -> ordinal-range test in a\n"
    "         fixed-width scan; contains = a scan over the dictionary's own bytes).\n"
    "IX+REC = dictionary + hybrid postings + 256KiB-zstd record (search never touches the record).\n"
    "CSR    = offsets + one u32 code per (entity,value) above the dictionary.\n"
    "CSR+IX = the same with hybrid postings derived on top (the accelerator layout).\n"
    "Presence bytes (Roaring, per entity over all n) are reported separately: every layout pays\n"
    "them identically where the column is partial."
)
