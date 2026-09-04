# The cut on a real condensed tree

**Result: met.** On HDBSCAN's own condensed tree over the arXiv corpus, a principal holding only
the term that covers a parent's stray members is served **that parent and no child of it**, with a
masked count equal to exactly the members they can see — and the build's containment report named
that split in advance.

Run 2026-08-18. `check.py` is the whole probe; it builds a bundle, serves it, and asserts.

```bash
# the corpus, if it is not already written
~/venvs/arxiv/bin/python -m test_corpora.arxiv.prepare --sample 50000 --out /tmp/nbout

cargo build --release -p tessera-cli
~/venvs/arxiv/bin/python probes/2026-08-18-condensed-tree/check.py /tmp/nbout
```

⊘ **The producer changed after this run.** The corpus came from `notebooks/arxiv-corpus.ipynb`,
which was ported to `test_corpora/arxiv/` on 2026-08-28 and deleted; the command above is the
current way to write the same corpus, and it collapses the condensed tree's chains where the
notebook of 2026-08-18 did not. The figures below are the 2026-08-18 run's and a rerun will not
reproduce the tree's depth.

## What was measured

| | |
|---|---|
| corpus | 50 000 arXiv papers, uniform sample, UMAP geometry |
| tree | HDBSCAN's condensed tree, `min_cluster_size` 125 — 263 clusters, depth to 51 |
| splits | **131 internal nodes, 124 of them non-covering** |
| the split tested | 1 647 members, 2 children, **687 (41.7%) held by no child** |
| served to the stray principal | the parent alone, masked count **687** |
| its children served | **none** |

## Why this is the check the stage exists for

HDBSCAN's children are subsets of their parents and **do not exhaust them**: points fall out as
noise at each split rather than joining any child. On this run **124 of 131 splits lose members
that way** — the non-covering case is not an edge case in a real hierarchy, it is the overwhelming
majority.

So a viewer who can see a parent's stray members and nothing else has a positive masked count on
the parent and **zero on every one of its children**. The parent is served alone, with no children
beneath it. Every construction that assumes a covering hierarchy gets this wrong, and gets it wrong
*invisibly*, because each number it reports is individually plausible:

- a **rollup** that unions the children's masked counts and calls the result the parent's would
  report 0 for a parent this principal can see 687 members of;
- a **cut** that required a child before it would draw an ancestor would blank the region entirely,
  and blank it in a way indistinguishable from "there is nothing here";
- a **budget** that climbed to a fixed depth rather than to a visible ancestor would do the same at
  any budget below this parent's depth. It does not: the probe asserts the parent survives budgets
  of 1, 2 and 10.

None of this is a disclosure question. Each node passed its own criterion against its own masked
count, which is the whole of decision 0080. What the tree's shape decides is only which of the
nodes a viewer may see is the one drawn.

## Two things the probe is careful about

**The tree is real; the grant is the instrument.** The hierarchy is HDBSCAN's own output. The term
is minted over one parent's stray set, because no arXiv category coincides with a split's noise —
and the claim under test is about the tree's shape, not the term's provenance.

**The split must clear the layer's existence criterion**, and the probe reads that criterion out of
`layers.toml` rather than assuming it. The first split it picked kept 726 members from its children
and was still withheld — 726 of 41 566 is 1.7%, under the layer's 5% floor — which is correct and
proves nothing about the frontier. Isolating the frontier means satisfying the criterion, so the
probe requires a stray share comfortably above it. A probe that had quietly relaxed the criterion
instead would have been testing a configuration nobody ships.

## What the build's report now carries

`reports/containment.json` gained a `splits` block: per internal node, its member count, its child
count and how many of its members no child holds — sorted by stray, capped at 100 listed with the
number not listed stated beside it. It decides nothing; it is what lets an operator meet a cluster
that appears without its children here, rather than in a support question.

The `violations` list is unchanged and is a different thing: a child holding a member its *parent*
does not, which is a fault. It was empty for this corpus, as it should be.
