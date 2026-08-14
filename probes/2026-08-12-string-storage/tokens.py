"""What a token index over prose costs, against the flat column it would replace — at three scales.

The open question `probes/2026-08-12-filter-placement/` arm 3 could not answer: postings lost to CSR
on a *surname*-shaped column (32.3 B/entity against 24.0) because 400,000 bitmaps with a singleton
tail have no repetition to amortise. Prose is a different vocabulary shape, and this measures
whether it is different enough — and, because the answer must hold at 10⁹, **how the cost moves with
scale rather than at one point**.

Scale matters in both directions and they fight:

* a **head** token's posting densifies as the corpus grows, so its cost per entry falls toward
  Roaring's bitmap-container rate;
* a **tail** token's entries spread across more container keys, so its cost per entry rises toward
  one container's overhead per entry.

Which wins is the whole question, and a single-scale measurement cannot answer it. Reporting three
real scales lets the trend be read instead of assumed. Scales are **prefixes in submission order**,
which is how `probes/dataset.md` §5 rule 1 says a scale is taken.

Roaring bitmaps are built and **serialised**, not modelled — the cost of a posting is what croaring
writes, which is where arm 3's figure came from too.

Usage:
    reference/.venv/bin/python probes/2026-08-12-string-storage/tokens.py \
        --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json
"""

import argparse
import collections
import json
import os
import re

from pyroaring import BitMap

ap = argparse.ArgumentParser()
ap.add_argument("--snapshot", required=True)
ap.add_argument("--scales", default="250000,1000000,2400000")
ap.add_argument("--limit", type=int, default=2_400_000)
args = ap.parse_args()

SCALES = [int(s) for s in args.scales.split(",")]
PATH = os.path.expanduser(args.snapshot)

titles, authors = [], []
with open(PATH, "rb") as f:
    for line in f:
        if len(titles) >= args.limit:
            break
        try:
            d = json.loads(line)
        except Exception:
            continue
        titles.append(" ".join((d.get("title") or "").split()))
        authors.append([a[0] for a in (d.get("authors_parsed") or [])])

print(f"read {len(titles):,} records (snapshot order = submission order = entity order)")

# Deliberately minimal: lowercase, split on non-alphanumerics, no stemming and no stopword list.
# Stopwords would cut the head — the part that amortises best — so their absence makes the postings
# figure conservative rather than flattering.
TOKEN = re.compile(r"[a-z0-9]+")


def index_cost(docs, n):
    postings = collections.defaultdict(BitMap)
    pairs = 0
    for e in range(n):
        for t in set(docs[e]):
            postings[t].add(e)
            pairs += 1
    serialised = sum(len(bm.serialize()) for bm in postings.values())
    keys = sorted(postings)
    front, prev = 0, ""
    for k in keys:
        j = 0
        while j < min(len(k), len(prev), 255) and k[j] == prev[j]:
            j += 1
        front += 1 + len(k[j:].encode())
        prev = k
    sizes = sorted((len(bm) for bm in postings.values()), reverse=True)
    return {
        "vocab": len(postings),
        "per_entity": pairs / n,
        "singleton_frac": sum(1 for s in sizes if s == 1) / len(sizes),
        "head_frac": sum(sizes[:1000]) / pairs,
        "postings": serialised / n,
        "dict": front / n,
        "index": (serialised + front) / n,
    }


title_tokens = [TOKEN.findall(t.lower()) for t in titles]
author_tokens = [[s.lower() for s in a] for a in authors]

for label, docs, flat_src in [
    ("title, tokenised (prose)", title_tokens, titles),
    ("authors, per surname (names)", author_tokens, [" ".join(a) for a in authors]),
]:
    print(f"\n### {label}\n")
    print(
        f"{'scale':>10} {'vocab':>10} {'val/ent':>8} {'singl.':>7} {'top1k':>7} "
        f"{'postings':>9} {'dict':>7} {'INDEX':>8} {'flat':>8} {'ratio':>7}"
    )
    for n in SCALES:
        if n > len(titles):
            continue
        r = index_cost(docs, n)
        flat = sum(len(v.encode()) for v in flat_src[:n]) / n + 8
        print(
            f"{n:>10,} {r['vocab']:>10,} {r['per_entity']:>8.1f} {r['singleton_frac']:>6.1%} "
            f"{r['head_frac']:>6.1%} {r['postings']:>9.2f} {r['dict']:>7.2f} "
            f"{r['index']:>8.2f} {flat:>8.2f} {flat/r['index']:>6.2f}x"
        )

print(
    "\nB/entity columns are bytes per entity. `ratio` is flat-column ÷ index.\n"
    "Read the trend across scales, not any single row: extrapolating one point to 10^9 is exactly\n"
    "what this measurement exists to avoid."
)
