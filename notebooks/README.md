# Notebooks

Python that **produces build inputs**, not Python that runs anything. Everything here writes files
a Rust binary later reads: CLAUDE.md's rule that Python is a first-class consumer and never a
component holds, and nothing in this directory sits on a request path, produces a served artifact,
or belongs to any trusted computing base.

| | |
|---|---|
| [`arxiv-corpus.ipynb`](arxiv-corpus.ipynb) | the whole corpus, end to end: the arXiv sources, UMAP, two clusterings, arXiv's own taxonomy, TF-IDF labels, and the configuration that reads the lot |
| [`run-corpus.sh`](run-corpus.sh) | builds a bundle from what the notebook wrote and serves it, then prints two principals to compare |

## The short way

```bash
notebooks/run-corpus.sh --notebook --sample 50000
```

Runs the notebook, builds the bundle, starts the server, and prints a pair of ready-to-paste
principals — eight arXiv categories against one of the eight, so both are looking at the same
clusters and the difference is the count beside each. On a 50 000-paper run the broad principal is
served 132 clusters and the narrow one 84; on a cluster they both see, 172 members against 66.
Neither number is the cluster's own size and neither viewer is told what that is.

Without `--notebook` it builds from whatever is already in the output directory; `--build-only`
stops before serving.

## Setting up, and running the notebook on its own

```bash
python3 -m venv notebooks/.venv
notebooks/.venv/bin/pip install -r notebooks/requirements.txt
TESSERA_DATA=/path/to/checkout/data notebooks/.venv/bin/jupyter lab notebooks/
```

**A worktree has no `data/` of its own** — point `TESSERA_DATA` at the checkout that holds it.

Headless, which is how it is checked:

```bash
TESSERA_DATA=… TESSERA_NOTEBOOK_SAMPLE=50000 \
  notebooks/.venv/bin/jupyter nbconvert --to notebook --execute \
  --output /tmp/executed.ipynb notebooks/arxiv-corpus.ipynb
```

## Three shapes over one corpus

The point of publishing all three is that they behave differently, and the differences are the
system's subject rather than the clustering's:

| Layer | Shape | A coarser view is | A budget |
|---|---|---|---|
| `clusters/kmeans` | `flat` | nothing — every cluster is a peer | inert |
| `clusters/hdbscan` | `nested` — a tree, edges within one level | an **ancestor**, and the cut climbs to it | trades depth for count |
| `taxonomy/arxiv` | `tiered` — two levels, edges **between** them | **another level**, which the client picks | inert |

The two hierarchies differ in a way worth seeing on a 50 000-paper run. The clustering's splits
lose members — **124 of its 131** keep points that none of their children hold, because HDBSCAN
sheds noise at every split. The taxonomy's **32 splits lose none**: every paper's primary category
sits in exactly one archive, so an archive is exactly the union of its classes. A rollup that
summed children would be right about arXiv's taxonomy and wrong about the clustering beside it.

## The knobs

Environment variables, all optional, so a run can be scripted without editing cells:

| | |
|---|---|
| `TESSERA_DATA` | the checkout holding `data/`. Default `/home/joe/code/tessera/data` |
| `TESSERA_NOTEBOOK_OUT` | where the build inputs go. Default `$TESSERA_DATA/notebook` |
| `TESSERA_NOTEBOOK_SAMPLE` | papers to take, uniformly. Default 200 000; `0` takes all 2 422 486 |
| `TESSERA_NOTEBOOK_UMAP` | `reuse` (default) reads `geometry.parquet`; `recompute` runs PCA and UMAP here |

**UMAP is reused by default and that is deliberate.** `data/geometry.parquet` is a *hashed*
artifact rather than a reproducible one — it was built with cuML on a GPU, which is not
bit-reproducible even under a fixed seed — so reusing it is what keeps a notebook run comparable
with every measurement already taken against the corpus. The recompute path is CPU UMAP, which
*is* reproducible under a fixed `random_state`, and costs roughly a minute per 10 000 points.

## Measured

At `SAMPLE=50000` with UMAP reused, on WSL2: **19 seconds** end to end, of which the TF-IDF labels
are 8 and everything else is under 4. HDBSCAN over the full 2 422 486 papers is 67 seconds, so the
whole-corpus run is tractable as long as UMAP is not recomputed.

## Why a notebook and not a script

`probes/` already holds the scripts that built the shipped corpus, and they work. What they do not
do is *explain themselves in one place*: the geometry, the clustering, the labels and the layer
declarations were four separate steps whose relationship lived in a memo. The notebook is one
readable pass with the reasoning beside each step and its outputs checked at the end — which is
what makes the corpus something a reader can re-derive rather than an artifact they have to trust.
