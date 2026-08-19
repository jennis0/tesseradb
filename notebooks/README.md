# Notebooks

Python that **produces build inputs**, not Python that runs anything. Everything here writes files
a Rust binary later reads: CLAUDE.md's rule that Python is a first-class consumer and never a
component holds, and nothing in this directory sits on a request path, produces a served artifact,
or belongs to any trusted computing base.

| | |
|---|---|
| [`arxiv-corpus.ipynb`](arxiv-corpus.ipynb) | the whole corpus, end to end: the arXiv sources, UMAP, two clusterings, arXiv's own taxonomy, TF-IDF labels, and the one declaration that reads the lot |
| [`run-corpus.sh`](run-corpus.sh) | writes this deployment's `tessera.toml`, runs `tessera build`, serves what it wrote, and prints two principals to compare |

## The short way

```bash
notebooks/run-corpus.sh --notebook --sample 50000
```

Runs the notebook, builds the bundle, starts the server, opens the viewer, and prints a pair of
ready-to-paste principals — eight arXiv categories against one of the eight, so both are looking at
the same clusters and the difference is the count beside each. On a 50 000-paper run the broad
principal is served 263 HDBSCAN clusters and the narrow one 178; on the largest cluster they both
see, 18 389 members against 3 760. Neither number is the cluster's own size and neither viewer is
told what that is.

**The tree they are shown differs too, and that is the more interesting half.** The broad
principal's 263 clusters hang off **one** root; the narrow principal's 178 hang off **eleven**. A
proportional criterion does not shrink downward — a small child can clear a bar its parent misses —
so a narrow viewer's tree comes apart into pieces whose tops are the highest clusters they qualify
for. The viewer draws exactly that, and no more: a piece's top is indented flush left, identically
to a cluster that has no parent at all, and nothing says a coarser one exists above it.

**The labels are the one thing neither of those two is shown.** A topic's text is a synthesis of
the two hundred titles it was drawn from, and it is declared as one — so it is served only to a
viewer who can already read every one of them. Eight categories is not enough; a principal holding
the whole vocabulary sees all sixty-four k-means topics with their text. That is the containment
test doing its job, not a missing label.

Without `--notebook` it builds from whatever is already in the output directory; `--build-only`
stops before serving.

## One declaration, and the frame it carries

The notebook writes a single `schema.toml` — the corpus, the view, the vocabularies, the attribute
columns and the five layers, every `source` a path relative to itself. `run-corpus.sh` writes the
`tessera.toml` beside it that says where the bundle goes, and the build is then `tessera build`
with no flags but the identity decision.

**The extent lives in that declaration, as `extent = "auto"`, and this is the failure the whole
arrangement exists to prevent.** The notebook writes raw UMAP coordinates, spanning about −17…18,
and the build fits a square box around them. It used to scale them by hand onto a grid a command
line named, and a frame that does not fit the data does not fail — quantisation clamps, so the
bundle is well-formed with the geometry wrong. Every build now prints what its frame does:

```
view 's0': quantising against x [-21.68873016357422, 23.519910736083986], y [-21.566141052246095, 23.64249984741211]
        the data spans x [-16.606388092041016, 18.43756866455078], y [-21.1229190826416, 23.199277877807617] — 50802 x 64252 of the 65536 x 65536 cells
        50000 point(s) placed, none on the frame's edge
view 's0': 50000 point(s) landed in 49945 distinct cell(s) — 99.9% of them have a position of their own
```

Read the last two lines. Nothing **clamped** onto the frame's edge, so no position is the frame's
rather than its own; and 99.9% of the points **keep a position of their own**, so papers far apart
in the embedding are far apart on the map. The old hand-scaled mistake reads instead as `18 x 23 of
the 65536 x 65536 cells` with zero clamps and a percentage in the low single digits — no error
anywhere, and a map that is one speck in a corner.

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

At `SAMPLE=50000` with UMAP reused, on WSL2: **10 seconds** end to end for the notebook — the
sample draw is 4 of them, HDBSCAN 1.5, and nothing else reaches a second. HDBSCAN over the full 2 422 486 papers is 67 seconds, so the
whole-corpus run is tractable as long as UMAP is not recomputed.

## Why a notebook and not a script

`probes/` already holds the scripts that built the shipped corpus, and they work. What they do not
do is *explain themselves in one place*: the geometry, the clustering, the labels and the layer
declarations were four separate steps whose relationship lived in a memo. The notebook is one
readable pass with the reasoning beside each step and its outputs checked at the end — which is
what makes the corpus something a reader can re-derive rather than an artifact they have to trust.
