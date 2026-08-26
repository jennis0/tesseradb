#!/usr/bin/env python3
"""Collapse the single-child chains of an already-written HDBSCAN layer.

`arxiv-corpus.ipynb` now does this as it writes (section 4): a node whose largest child holds more
than `CHAIN` of its members is not a level of the tree — the condensed tree emits one such node
per paper that detaches near the root, so the top of the tree is a chain of forty-odd nodes each
holding nearly everything, and a budgeted cut spends its whole budget on them. Such a node is
dropped and its children re-parented to its nearest kept ancestor; the root is kept whatever its
shape, because a cut climbs to it. Leaves are never dropped, so the selection is intact.

This script is the same transform over the files, for a directory the notebook wrote before it
learned to: the HDBSCAN clusters, their members, the topics attached to them and the topics'
members. A topic whose text is the `(no distinctive terms)` placeholder is dropped with the
chains — the client shows no label rather than that string. Every other file is left alone, the
UMAP included, which is why this exists rather than a re-run.

    python3 notebooks/collapse-hdbscan.py data/notebook-2m4-live
"""
import collections
import json
import pathlib
import sys

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

CHAIN = 0.9
PLACEHOLDER = "(no distinctive terms)"

out = pathlib.Path(sys.argv[1])
clusters = pq.read_table(out / "clusters-hdbscan.parquet")
members = pq.read_table(out / "clusters-hdbscan-members.parquet")
topics = pq.read_table(out / "topics-hdbscan.parquet")
topic_members = pq.read_table(out / "topics-hdbscan-members.parquet")

rows = clusters.to_pylist()
parent_of = {r["key"]: r["parent"] for r in rows}
size = collections.Counter(members.column("key").to_pylist())
children_of = collections.defaultdict(list)
for key, parent in parent_of.items():
    if parent is not None:
        children_of[parent].append(key)
roots = [k for k, p in parent_of.items() if p is None]


def depth(key, parent_of):
    d = 0
    while parent_of[key] is not None:
        key = parent_of[key]
        d += 1
    return d


def is_chain(key):
    kids = children_of[key]
    return bool(kids) and key not in roots and max(size[c] for c in kids) > CHAIN * size[key]


dropped = {k for k in parent_of if is_chain(k)}
kept_parent = {}
for key, parent in parent_of.items():
    while parent in dropped:
        parent = parent_of[parent]
    kept_parent[key] = parent
before = (len(parent_of), max(depth(k, parent_of) for k in parent_of))
kept = {k: p for k, p in kept_parent.items() if k not in dropped}
after = (len(kept), max(depth(k, kept) for k in kept))

keep_rows = [dict(r, parent=kept[r["key"]]) for r in rows if r["key"] not in dropped]
pq.write_table(pa.table({
    name: pa.array([r[name] for r in keep_rows], clusters.schema.field(name).type)
    for name in clusters.column_names
}), out / "clusters-hdbscan.parquet")
keep_keys = pa.array(list(kept))
pq.write_table(members.filter(pc.is_in(members.column("key"), keep_keys)),
               out / "clusters-hdbscan-members.parquet")

# A topic goes with its cluster, and a placeholder topic goes regardless.
placeholder = 0
keep_topics = []
for r in topics.to_pylist():
    if r["attached_key"] in dropped:
        continue
    if r["contents"] and r["contents"][0] and r["contents"][0][0] == PLACEHOLDER:
        placeholder += 1
        continue
    keep_topics.append(r)
pq.write_table(pa.table({
    name: pa.array([r[name] for r in keep_topics], topics.schema.field(name).type)
    for name in topics.column_names
}), out / "topics-hdbscan.parquet")
label_keys = pa.array([r["key"] for r in keep_topics])
pq.write_table(topic_members.filter(pc.is_in(topic_members.column("key"), label_keys)),
               out / "topics-hdbscan-members.parquet")

manifest_path = out / "manifest.json"
if manifest_path.exists():
    manifest = json.loads(manifest_path.read_text())
    manifest["hdbscan"].update({
        "clusters_in_tree": after[0], "max_depth": after[1], "labels": len(keep_topics),
        "chains_collapsed": {"nodes_before": before[0], "depth_before": before[1],
                             "rule": f"largest child > {CHAIN:.0%} of the parent",
                             "placeholder_topics_dropped": placeholder},
    })
    manifest_path.write_text(json.dumps(manifest, indent=2))

print(f"clusters/hdbscan: {before[0]} nodes, depth {before[1]}  ->  {after[0]} nodes, depth {after[1]} "
      f"({len(dropped)} chain nodes collapsed; {sum(1 for k in kept if not children_of[k])} leaves untouched)")
print(f"topics/hdbscan: {topics.num_rows} -> {len(keep_topics)} "
      f"({len(topics) - len(keep_topics) - placeholder} on collapsed nodes, {placeholder} placeholders)")
print(f"member rows: {members.num_rows:,} -> {pq.ParquetFile(out / 'clusters-hdbscan-members.parquet').metadata.num_rows:,}")
