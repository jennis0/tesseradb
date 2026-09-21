#!/usr/bin/env python3
"""Measure the MeSH descriptor DAG and the ancestor-closure cost of one PubMed chunk.

Standard library only. Run with ~/venvs/projection/bin/python; see README.md beside this file.

  measure.py [--tree PATH] [--chunk PATH]

The descriptor DAG: D is a parent of E iff some tree number of E has, as its dotted prefix
(last `.NNN` component stripped), a tree number owned by D. Edges are deduplicated.
"""

import argparse
import json
import statistics
import time
from collections import Counter, defaultdict, deque

TREE = "/mnt/nas/tessera/datasets/mesh/2025/mtrees2025.bin"
CHUNK = "/mnt/nas/tessera/datasets/medcpt-pubmed/2026-08-27/pubmed_chunk_18.json"
CORPUS_ARTICLES = 35_920_666


def load_tree(path):
    """name(lowercased) -> set of tree numbers; tree number -> name."""
    name_to_tns = defaultdict(set)
    tn_to_name = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            name, tn = line.rsplit(";", 1)
            tn = tn.strip()
            name = name.strip().lower()  # lines carry a leading space
            name_to_tns[name].add(tn)
            tn_to_name[tn] = name
    return name_to_tns, tn_to_name


def build_dag(name_to_tns, tn_to_name):
    """parents[E] = {D...}; children[D] = {E...}; self_loops = [(name, child_tn, parent_tn)]."""
    parents = defaultdict(set)
    children = defaultdict(set)
    self_loops = []
    for e, tns in name_to_tns.items():
        for tn in tns:
            if "." not in tn:
                continue  # a root position
            ptn = tn.rsplit(".", 1)[0]
            d = tn_to_name[ptn]  # every prefix is present (verified at acquisition)
            if d == e:
                self_loops.append((e, tn, ptn))
                continue
            parents[e].add(d)
            children[d].add(e)
    return parents, children, self_loops


def tarjan_scc(nodes, children):
    """Iterative Tarjan. Returns list of SCCs (lists of nodes)."""
    index = {}
    low = {}
    on_stack = set()
    stack = []
    sccs = []
    counter = 0
    for root in nodes:
        if root in index:
            continue
        work = [(root, iter(children.get(root, ())))]
        index[root] = low[root] = counter
        counter += 1
        stack.append(root)
        on_stack.add(root)
        while work:
            v, it = work[-1]
            advanced = False
            for w in it:
                if w not in index:
                    index[w] = low[w] = counter
                    counter += 1
                    stack.append(w)
                    on_stack.add(w)
                    work.append((w, iter(children.get(w, ()))))
                    advanced = True
                    break
                elif w in on_stack:
                    low[v] = min(low[v], index[w])
            if advanced:
                continue
            work.pop()
            if work:
                u = work[-1][0]
                low[u] = min(low[u], low[v])
            if low[v] == index[v]:
                comp = []
                while True:
                    w = stack.pop()
                    on_stack.discard(w)
                    comp.append(w)
                    if w == v:
                        break
                sccs.append(comp)
    return sccs


def topo_order(nodes, parents, children):
    """Kahn's algorithm; returns order (list). Raises if a cycle remains."""
    indeg = {n: len(parents.get(n, ())) for n in nodes}
    q = deque(n for n in nodes if indeg[n] == 0)
    order = []
    while q:
        n = q.popleft()
        order.append(n)
        for c in children.get(n, ()):
            indeg[c] -= 1
            if indeg[c] == 0:
                q.append(c)
    if len(order) != len(nodes):
        raise RuntimeError("cycle remains; topological order impossible")
    return order


def depths(order, parents):
    longest = {}
    shortest = {}
    for n in order:
        ps = parents.get(n, ())
        if not ps:
            longest[n] = shortest[n] = 0
        else:
            longest[n] = 1 + max(longest[p] for p in ps)
            shortest[n] = 1 + min(shortest[p] for p in ps)
    return longest, shortest


def ancestor_sets(order, parents):
    """Full ancestor set per node, in topological order (memoised union)."""
    anc = {}
    for n in order:
        s = set()
        for p in parents.get(n, ()):
            s.add(p)
            s |= anc[p]
        anc[n] = frozenset(s)
    return anc


def iter_chunk(path):
    """Yield (pmid, record) from the pretty-printed {pmid: {...}} object without json.load.

    The file is read as one string (1.4 GB) and decoded value by value with raw_decode, so the
    peak is the text plus one record, not the whole parsed object.
    """
    with open(path, encoding="utf-8") as f:
        text = f.read()
    dec = json.JSONDecoder()
    n = len(text)
    i = text.index("{") + 1
    while True:
        # skip whitespace and commas to the next key or the closing brace
        while i < n and text[i] in " \t\r\n,":
            i += 1
        if text[i] == "}":
            return
        key, i = dec.raw_decode(text, i)
        while text[i] in " \t\r\n":
            i += 1
        assert text[i] == ":"
        i += 1
        while text[i] in " \t\r\n":
            i += 1
        val, i = dec.raw_decode(text, i)
        yield key, val


def descriptors_of(m):
    out = set()
    for entry in m.split("|"):
        if not entry:
            continue
        name = entry.split("!", 1)[0]
        if name.endswith("*"):
            name = name[:-1]
        out.add(name)
    return out


def fmt_dist(counter, keys=None):
    keys = sorted(counter) if keys is None else keys
    return "\n".join(f"| {k} | {counter[k]:,} |" for k in keys)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tree", default=TREE)
    ap.add_argument("--chunk", default=CHUNK)
    args = ap.parse_args()
    t0 = time.time()

    name_to_tns, tn_to_name = load_tree(args.tree)
    parents, children, self_loops = build_dag(name_to_tns, tn_to_name)
    nodes = sorted(name_to_tns)
    n_edges = sum(len(s) for s in parents.values())
    roots = [n for n in nodes if n not in parents]
    indeg = Counter(len(parents.get(n, ())) for n in nodes)

    print("## 1. Descriptor DAG")
    print(f"nodes {len(nodes):,}; edges (deduplicated, self-loops excluded) {n_edges:,}; "
          f"roots (no parent) {len(roots):,}")
    print(f"tree numbers {len(tn_to_name):,}; root positions (no dot) "
          f"{sum(1 for t in tn_to_name if '.' not in t):,}")
    print("in-degree distribution (parents -> descriptors):")
    print(fmt_dist(indeg))
    print(f"max in-degree {max(indeg)}: "
          + "; ".join(n for n in nodes if len(parents.get(n, ())) == max(indeg)))
    print()

    print("## 2. Self-loops")
    self_loop_names = sorted({n for n, _, _ in self_loops})
    print(f"self-loop edge instances {len(self_loops)}; distinct descriptors {len(self_loop_names)}")
    for n, tn, ptn in self_loops[:10]:
        print(f"  {n}: {tn} under {ptn}")
    print()

    print("## 3. Cycles of length >= 2 (self-loops removed)")
    sccs = tarjan_scc(nodes, children)
    big = [c for c in sccs if len(c) > 1]
    print(f"SCCs {len(sccs):,}; SCCs of size > 1: {len(big)}; "
          f"acyclic: {'yes' if not big else 'NO'}")
    for c in big[:10]:
        print(f"  size {len(c)}: {sorted(c)}")
    print()

    print("## 4. Depth")
    if big:
        print("graph has cycles; depth and closure skipped (see SCC list above)")
        longest = shortest = anc = None
    else:
        order = topo_order(nodes, parents, children)
        longest, shortest = depths(order, parents)
        ld = Counter(longest.values())
        sd = Counter(shortest.values())
        print("depth distribution (0 = root):")
        print("| depth | by longest path | by shortest path |")
        print("|---|---|---|")
        for d in range(max(max(ld), max(sd)) + 1):
            print(f"| {d} | {ld[d]:,} | {sd[d]:,} |")
        print(f"max longest {max(ld)}; max shortest {max(sd)}")
        differ = sum(1 for n in nodes if longest[n] != shortest[n])
        print(f"descriptors whose longest and shortest depth differ: {differ:,} "
              f"({100 * differ / len(nodes):.1f}%)")
        gap = Counter(longest[n] - shortest[n] for n in nodes)
        print("gap (longest - shortest) distribution:")
        print(fmt_dist(gap))
        print(f"mean longest depth {statistics.mean(longest.values()):.2f}; "
              f"mean shortest depth {statistics.mean(shortest.values()):.2f}")
        anc = ancestor_sets(order, parents)
        asz = [len(anc[n]) for n in nodes]
        print(f"ancestor-set size per descriptor: mean {statistics.mean(asz):.2f}, "
              f"median {statistics.median(asz)}, max {max(asz)} "
              f"({max(nodes, key=lambda n: len(anc[n]))})")
        # compare with the tree-number (positional) depth
        tn_depth = Counter(t.count(".") for t in tn_to_name)
        print("tree-number depth distribution for reference (0 = root position):")
        print(fmt_dist(tn_depth))
    print()

    print("## 5. Chunk 18 closure")
    t1 = time.time()
    n_articles = 0
    n_indexed = 0
    n_resolved_articles = 0
    unresolved_mentions = 0
    resolved_mentions = 0
    per_article = []
    per_closure = []
    for pmid, rec in iter_chunk(args.chunk):
        n_articles += 1
        m = rec.get("m") or ""
        descs = descriptors_of(m)
        if not descs:
            continue
        n_indexed += 1
        resolved = {d for d in descs if d in name_to_tns}
        unresolved_mentions += len(descs) - len(resolved)
        resolved_mentions += len(resolved)
        if not resolved:
            continue
        n_resolved_articles += 1
        per_article.append(len(resolved))
        if anc is not None:
            closure = set(resolved)
            for d in resolved:
                closure |= anc[d]
            per_closure.append(len(closure))
    t_chunk = time.time() - t1
    print(f"articles {n_articles:,}; MeSH-indexed (non-empty m) {n_indexed:,}; "
          f"with >= 1 resolved descriptor {n_resolved_articles:,}")
    print(f"distinct-descriptor mentions: resolved {resolved_mentions:,}, dropped (unresolved) "
          f"{unresolved_mentions:,} ({100 * unresolved_mentions / (resolved_mentions + unresolved_mentions):.2f}%)")
    print(f"(a) resolved descriptors per article: mean {statistics.mean(per_article):.2f}, "
          f"median {statistics.median(per_article)}, max {max(per_article)}")
    if per_closure:
        print(f"(b) ancestor closure per article: mean {statistics.mean(per_closure):.2f}, "
              f"median {statistics.median(per_closure)}, max {max(per_closure)}")
        tot_a = sum(per_article)
        tot_c = sum(per_closure)
        print(f"(c) member rows: without closure {tot_a:,}; with closure {tot_c:,}; "
              f"ratio {tot_c / tot_a:.3f}")
        print("(c') closure-size distribution (deciles): "
              + ", ".join(str(q) for q in statistics.quantiles(per_closure, n=10)))
        print("## 6. Extrapolation")
        frac = n_resolved_articles / n_articles
        est_indexed = CORPUS_ARTICLES * frac
        print(f"corpus {CORPUS_ARTICLES:,} articles x chunk-18 indexed fraction {frac:.4f} "
              f"= {est_indexed:,.0f} articles with resolved MeSH")
        print(f"rows without closure: {est_indexed * statistics.mean(per_article):,.0f}")
        print(f"rows with closure:    {est_indexed * statistics.mean(per_closure):,.0f}")
        print(f"(rows per article: {tot_a / n_articles:.3f} / {tot_c / n_articles:.3f} over all "
              f"chunk-18 articles, indexed or not)")
    print()
    print(f"wall-clock: total {time.time() - t0:.1f}s, of which chunk pass {t_chunk:.1f}s")


if __name__ == "__main__":
    main()
