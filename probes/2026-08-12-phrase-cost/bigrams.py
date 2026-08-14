"""What exact multi-token match ("phrase") would cost over a text column, measured three ways.

The token index (`2026-08-12-string-storage/` arm 3) stores one Roaring posting per token and
answers `match` as AND-of-postings inside the candidate. That cannot express adjacency: "permission
masked" as a phrase needs one of

1. **verify against the record** — AND the tokens' postings, then decompress the survivors'
   records and check adjacency there. Zero storage; query cost is result-bound. Nothing to measure
   here beyond what the record store already measured (169 us per block read).
2. **token-bigram terms** — index each adjacent token pair as a term of its own, so a two-token
   phrase is one posting and a longer one is an AND of overlapping bigrams (plus a verify whose
   survivor set is tiny). Keeps the posting-is-a-Roaring-bitmap shape. Storage measured here.
3. **positional postings** — per (term, doc), the token positions, delta-varint encoded. The
   Lucene shape. Exact for any length with no verify, but the posting stops being a bitmap:
   payloads need a doc-ordered posting format and a two-phase evaluator. Position bytes measured
   here (exact varint cost on the real token streams); the mechanism cost is the real price and
   is argued in the design, not here.

Also measured: the per-entry term-frequency byte a scored `match` (BM25-shape) would need, since
TF shares the payload problem positions have and the design should price them together.

Postings are pyroaring, serialised, not modelled; `hybrid` stores a singleton term's one entity as
a bare u32 plus a flag bit. Scales are prefixes in snapshot order (entity order). `abstract` runs
at smaller scales: its token volume per entity is ~12x a title's.

Usage:
    reference/.venv/bin/python probes/2026-08-12-phrase-cost/bigrams.py \
        --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json
"""

import argparse
import json
import os
import re

from pyroaring import BitMap

ap = argparse.ArgumentParser()
ap.add_argument("--snapshot", required=True)
ap.add_argument("--title-scales", default="250000,1000000,2400000")
ap.add_argument("--abstract-scales", default="100000,250000")
ap.add_argument("--limit", type=int, default=2_400_000)
args = ap.parse_args()

PATH = os.path.expanduser(args.snapshot)
TOKEN = re.compile(r"[a-z0-9]+")
SINGLETON_ROARING = len(BitMap([7]).serialize())  # serialised size of a one-entity bitmap

titles, abstracts = [], []
with open(PATH, "rb") as f:
    for line in f:
        if len(titles) >= args.limit:
            break
        try:
            d = json.loads(line)
        except Exception:
            continue
        titles.append(" ".join((d.get("title") or "").split()))
        if len(abstracts) < max(int(s) for s in args.abstract_scales.split(",")):
            abstracts.append(" ".join((d.get("abstract") or "").split()))

print(f"read {len(titles):,} titles, {len(abstracts):,} abstracts (snapshot order = entity order)")


def varint(n):
    b = 1
    while n >= 0x80:
        n >>= 7
        b += 1
    return b


def front_coded(keys):
    total, prev = 0, ""
    for k in keys:
        j = 0
        while j < min(len(k), len(prev), 255) and k[j] == prev[j]:
            j += 1
        total += 1 + len(k[j:].encode())
        prev = k
    return total


def add(post, term, e):
    """Postings map holding a bare int until a term's second entity, then a BitMap."""
    cur = post.get(term)
    if cur is None:
        post[term] = e
    elif isinstance(cur, int):
        if cur != e:
            post[term] = BitMap([cur, e])
    else:
        cur.add(e)


def posting_costs(post):
    roaring = hybrid = 0
    singles = 0
    for v in post.values():
        if isinstance(v, int):
            roaring += SINGLETON_ROARING
            hybrid += 4
            singles += 1
        else:
            s = len(v.serialize())
            roaring += s
            hybrid += s
    hybrid += (len(post) + 7) // 8
    return roaring, hybrid, singles / len(post)


def measure(label, docs, n):
    uni, bi = {}, {}
    occurrences = 0   # token occurrences (what positions must encode)
    entries = 0       # distinct (term, doc) pairs (what a TF byte is charged per)
    pos_bytes = 0     # exact delta-varint position bytes over the real streams
    for e in range(n):
        toks = docs[e]
        occurrences += len(toks)
        seen = {}
        for i, t in enumerate(toks):
            seen.setdefault(t, []).append(i)
        entries += len(seen)
        for t, positions in seen.items():
            add(uni, t, e)
            prev = 0
            for p in positions:
                pos_bytes += varint(p - prev)
                prev = p
        for i in range(len(toks) - 1):
            add(bi, f"{toks[i]} {toks[i+1]}", e)

    uni_roar, uni_hyb, uni_single = posting_costs(uni)
    bi_roar, bi_hyb, bi_single = posting_costs(bi)
    uni_dict = front_coded(sorted(uni))
    bi_dict = front_coded(sorted(bi))

    print(
        f"{label:>9} {n:>10,} {occurrences/n:>6.1f} {entries/n:>6.1f} "
        f"{len(uni):>10,} {(uni_hyb+uni_dict)/n:>7.1f} "
        f"{len(bi):>11,} {bi_single:>6.1%} {(bi_hyb+bi_dict)/n:>8.1f} {bi_dict/n:>6.1f} "
        f"{entries/n:>6.1f} {pos_bytes/n:>7.1f}"
    )


print(
    f"\n{'field':>9} {'scale':>10} {'tok/e':>6} {'ent/e':>6} "
    f"{'uni vocab':>10} {'UNI B/e':>7} "
    f"{'bi vocab':>11} {'bi sgl':>6} {'BI B/e':>8} {'bidic':>6} "
    f"{'TF B/e':>6} {'POS B/e':>7}"
)
title_toks = [TOKEN.findall(t.lower()) for t in titles]
for n in [int(s) for s in args.title_scales.split(",")]:
    if n <= len(title_toks):
        measure("title", title_toks, n)
del title_toks
abstract_toks = [TOKEN.findall(a.lower()) for a in abstracts]
for n in [int(s) for s in args.abstract_scales.split(",")]:
    if n <= len(abstract_toks):
        measure("abstract", abstract_toks, n)

print(
    "\nUNI B/e = unigram hybrid postings + front-coded dictionary (the shipped-design index).\n"
    "BI B/e  = the same pair for token bigrams — option 2's whole additional cost.\n"
    "TF B/e  = one u8 term frequency per (term, doc) entry — what a scored match adds.\n"
    "POS B/e = exact delta-varint bytes of every token position — option 3's payload, excluding\n"
    "          the posting-format change that is its real price.\n"
    "Tokeniser is the probe convention (lowercase, [a-z0-9]+): adequate for cost shape on this\n"
    "English corpus; vocabulary shape under a UAX#29 segmenter or on a CJK corpus is NOT measured."
)
