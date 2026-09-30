# MeSH descriptor DAG and ancestor-closure cost — 2026-09-01

**What was measured.** The shape of the MeSH 2025 *descriptor* graph (as opposed to the tree-number
tree, which is a strict tree — see the dataset README), and what it would cost to store each
article's MeSH membership *closed under ancestors*, measured on one PubMed chunk and extrapolated
to the corpus.

**Inputs.**

- `/mnt/nas/tessera/datasets/mesh/2025/mtrees2025.bin` — 64,883 `Descriptor Name;TreeNumber`
  lines, 30,954 distinct descriptors. Lines carry a leading space; names are stripped and
  lowercased for the join.
- `/mnt/nas/tessera/datasets/medcpt-pubmed/2026-08-27/pubmed_chunk_18.json` — 1.39 GB,
  940,707 articles keyed by PMID. The `m` field is parsed as `descriptor!qualifier*` entries,
  qualifier and major-topic star stripped, distinct descriptor set per article. Descriptors that
  do not resolve against the tree file are dropped (owner ruling).

**Definition.** Descriptor D is a parent of descriptor E iff some tree number of E, with its last
`.NNN` component removed, is a tree number owned by D. Edges are deduplicated across tree numbers.

**Command.**

```
~/venvs/projection/bin/python probes/2026-09-01-mesh-dag/measure.py
```

Standard library only (Python 3.12.12 in `~/venvs/projection`; `ijson` is not installed, so the
chunk is read as one string and decoded record by record with `raw_decode`). Wall-clock **32 s**
on the WSL2 box, of which 31 s is the chunk pass; peak RSS 2.8 GB.

## 1. The descriptor DAG

| | |
|---|---|
| nodes (descriptors) | 30,954 |
| edges, deduplicated | **42,287** |
| roots (no parent) | **110** |
| tree-number root positions, for comparison | 115 |
| self-loops | **0** |
| strongly connected components of size > 1 | **0** — the graph is acyclic |

Five of the 115 root positions belong to descriptors that also sit below another descriptor
elsewhere, which is why there are 110 roots rather than 115.

In-degree (number of distinct parents per descriptor):

| parents | descriptors |
|---|---|
| 0 | 110 |
| 1 | 21,558 |
| 2 | 7,504 |
| 3 | 1,472 |
| 4 | 256 |
| 5 | 43 |
| 6 | 11 |

Max in-degree is 6, held by eleven descriptors: `22q11 deletion syndrome`, `ataxia
telangiectasia`, `goblet cells`, `hepatolenticular degeneration`, `interleukin receptor common
gamma subunit`, `mitochondrial trifunctional protein`, `oculocerebrorenal syndrome`, `susac
syndrome`, `theranostic nanomedicine`, `tuberous sclerosis`, `xeroderma pigmentosum`. 30.0% of
descriptors (9,286) have more than one parent.

## 2. Self-loops

None. No descriptor owns both a tree number and that number's dotted prefix.

## 3. Cycles

None. Tarjan's algorithm over the 30,954 nodes finds 30,954 singleton components. Every
measurement below is therefore on the DAG as built; nothing had to be condensed or skipped.

## 4. Depth

Depth from a root, 0 = root, by longest and by shortest path:

| depth | by longest path | by shortest path |
|---|---|---|
| 0 | 110 | 110 |
| 1 | 1,248 | 1,909 |
| 2 | 3,185 | 6,643 |
| 3 | 4,928 | 8,806 |
| 4 | 6,172 | 6,108 |
| 5 | 5,523 | 3,355 |
| 6 | 3,958 | 1,671 |
| 7 | 2,859 | 1,216 |
| 8 | 1,688 | 669 |
| 9 | 808 | 346 |
| 10 | 236 | 60 |
| 11 | 127 | 40 |
| 12 | 60 | 21 |
| 13 | 16 | 0 |
| 14 | 29 | 0 |
| 15 | 2 | 0 |
| 16 | 4 | 0 |
| 17 | 1 | 0 |

| | |
|---|---|
| max depth by longest path | **17** |
| max depth by shortest path | 12 |
| max tree-number depth (positional), for comparison | 12 |
| mean depth, longest / shortest | 4.67 / 3.59 |
| descriptors whose longest and shortest depth differ | **14,700 (47.5%)** |
| largest gap between the two | 14 (six descriptors) |
| ancestor-set size per descriptor: mean / median / max | 7.00 / 6 / 50 (`obesity hypoventilation syndrome`) |

The longest-path depth of 17 exceeds the deepest tree number (12) because a path through the
descriptor DAG may hop between tree positions at each step: a descriptor deep in one branch has
a child whose *other* tree number is deep in another branch, and so on. Depth is therefore not a
property a descriptor carries on its own in this graph.

## 5. Chunk 18: ancestor closure per article

| | |
|---|---|
| (d) articles in chunk 18 | 940,707 |
| (d) MeSH-indexed (non-empty `m`) | 816,393 (86.8%) |
| with at least one *resolved* descriptor | 816,364 (29 articles lose every descriptor to the vintage mismatch) |
| distinct-descriptor mentions resolved / dropped | 8,132,820 / 507,559 (5.87% dropped) |

Per article with at least one resolved descriptor:

| | mean | median | max |
|---|---|---|---|
| (a) distinct resolved descriptors | **9.96** | 10 | 46 |
| (b) ancestor closure (descriptors plus every ancestor) | **55.58** | 54 | 240 |

The dataset README's 10.6 mean counts unresolved mentions too (8,640,379 / 816,393 = 10.58);
9.96 is the mean after the drop.

Closure-size deciles: 20, 29, 38, 46, 54, 62, 70, 80, 93.

| (c) member rows, chunk 18 | |
|---|---|
| without closure | 8,132,820 |
| with closure | **45,375,723** |
| ratio | **5.58×** |

The ratio is exactly 55.58 / 9.96. Per resolved descriptor the DAG has a mean of 7.0 ancestors,
so a naive 8× is the upper bound; overlap between the ancestors of an article's descriptors
brings it to 5.6×.

## 6. Extrapolation to the corpus

**This is an extrapolation from one chunk** (chunk 18, PMIDs 18,000,000–18,999,999, articles to
2009) applied to 35,920,666 articles. It assumes chunk 18's indexed fraction (86.8%) and its
per-article means hold across the corpus. The dataset README shows they do not hold uniformly:
MeSH coverage is 100% in chunk 0 and 37.5% in chunk 37, so the true corpus figure is a weighted
average over a strong time trend, and the newest chunks pull it down.

| | rows |
|---|---|
| articles with resolved MeSH (35,920,666 × 0.8678) | 31,172,659 |
| member rows without closure (× 9.96) | **≈ 311 million** (310,549,736) |
| member rows with closure (× 55.58) | **≈ 1.73 billion** (1,732,660,850) |

Per article over the whole chunk, indexed or not: 8.65 rows without closure, 48.24 with.
