# arXiv — the ladder's bottom rung, its only embedding corpus, and its only two-view one

**2,422,486 preprints**, from the metadata snapshot and one BGE-large-en-v1.5 topic embedding per
paper. Every other rung is geographic: its positions come from a projection, which is a pure
function, so a frame change costs a rerun. This one's positions come from UMAP over an embedding,
which is not — and that difference is the reason the rung is here rather than a matter of scale.

It is also the corpus that carries the artifact catalogue: three hierarchy shapes over one point
set, a covering hierarchy beside a non-covering one, a TF-IDF title on every cluster, and names
written by a language model.

```bash
# the corpus: two layouts, two clusterings, the taxonomy, a title on every cluster
~/venvs/projection/bin/python -m test_corpora.arxiv.prepare --sample 200000

# optional: a fourth clustering, named by a chat model (needs an endpoint — see below)
~/venvs/projection/bin/python -m test_corpora.arxiv.toponymy --llm mock

cd "$TESSERA_LADDER/arxiv" && tessera check --payloads && tessera build

# what the two layouts cost, judged on the properties a bundle stores
~/venvs/projection/bin/python -m test_corpora.arxiv.compare
```

## Two views over one entity space

Owner direction, 2026-09-01. The same papers, the same attributes, the same taxonomy and the same
clusterings — positioned twice, each layout in its own frame:

| view | route | positions |
|---|---|---|
| `knn` | a cosine kNN graph in **full 1024 dimensions** (cuVS CAGRA, index built in fp16, k = 15) handed to UMAP as a `precomputed_knn`, so UMAP does the layout and nothing else | `points.parquet`, with every attribute column |
| `pca64` | PCA to 64 components first — the route `data/geometry.parquet` was built on | `points-pca64.parquet`: identity, position, access column |

`knn` is the **anchor** (`[defaults].allocation_view`, decision 0112): entity ids are ordered by
Morton code in that view, so naming it explicitly is what stops a reordering of the view blocks
silently re-keying a rebuild. It is also the layout the clusterings are computed over.

**Every layer is named on both views, and that is the experiment.** A `computed` content is
recomputed per viewer from `membership ∩ M_auth` — and, a view being a row space of its own, per
view — so each cluster and each subject class carries a centroid, a box and a hull *in each
projection over one membership*. Whether PCA-64 scatters a concept that full dimension holds
together stops being an aggregate statistic about the point cloud and becomes a property of the
artifacts the server serves. Comparing two separate builds offline could not have produced it.

**The frames cannot be shared.** Each route's UMAP output has its own coordinate range, so
`extent = "auto"` fits a different box to each; one frame declared for both would put one route's
points in a corner of the other's grid, with no clamp and no error to say so.
`projection = "none"` on both: an embedding layout is not a map.

`compare.py` measures the two against each other — 2D neighbourhood recall against an exact
full-dimension brute force, category purity, tile occupancy under Morton order, and frame use. Its
docstring records the **two confounds** that were found the hard way, both of which make a
plausible measurement say the opposite of the truth.

## Every cluster carries its own title

The TF-IDF label is **supplied `text` content on the cluster artifact itself**, ranked — the
specific description first, `a cluster of papers` second — with each rank's generating set recorded
as member rows at that rank. A client names an artifact from its own first content
(`clients/ts/deck/src/layer.ts`), so a cluster shows its title with no join.

It used to be an artifact of its own on a `topics/*` label layer that the client joined by
attachment. Those layers are gone, along with their sources and their `depends_on` edge; what they
declared is not: the layer gate, the artifact gate and the content's provenance requirement all
moved onto the clustering layer. The content's requirement stays `all` — the text is a synthesis of
the titles it was drawn from, so it is read only by a viewer who can already read every one of
them.

**No distinctive terms is the fallback alone.** A cluster the labeller has nothing to say about
carries `a cluster of papers` and nothing above it. Dropping its content entirely is refused at
publication — the layer declares a supplied kind, and an artifact served without content its layer
declares cannot be told apart from one whose content was withheld — so the ranking is what absorbs
the case. That refusal is a real difference from the label-layer shape, where a cluster with no
distinctive terms simply had no label artifact.

The taxonomy's artifacts carry no supplied content either: an archive's key *is* its published
name, and a TF-IDF description of `math` would replace a fact with a statistic.

## The two stages, and why they are two

`prepare.py` is the corpus. `toponymy.py` adds two layers to the directory it wrote, and splices
its declaration into the copy of `corpus.toml` beside the data.

They are separate because the second one needs a chat endpoint and because it *is* the run: over
the whole corpus the first stage is minutes and the second is an hour and a half, almost all of it
waiting on the model. Trying a different floor or a different model is then a rerun of the cheap
half of the pipeline rather than of all of it.

The declaration is split the same way. `corpus.toml` in git is complete and valid on its own —
three layers, no Toponymy — and `toponymy.toml` holds the fourth and its label layer, which the
second stage injects at two marker comments. A missing marker is a refusal: a splice that silently
did nothing would leave a build reading a declaration with no Toponymy layer while the stage's
files sat beside it.

## Four shapes over one corpus

The point of publishing all of them is that they behave differently, and the differences are the
system's subject rather than the clustering's:

| Layer | Shape | A coarser view is | A budget |
|---|---|---|---|
| `clusters/kmeans` | `flat` | nothing — every cluster is a peer | inert |
| `clusters/hdbscan` | `nested` — a tree, edges within one level | an **ancestor**, and the cut climbs to it | trades depth for count |
| `taxonomy/arxiv` | `tiered` — two levels, edges **between** them | **another level**, which the client picks | inert |
| `clusters/toponymy` | `tiered` — one level per rung of Toponymy's ladder | **a coarser rung** | inert |

**HDBSCAN's tree is here because its children do not exhaust it.** A fifth to a quarter of a
parent's points fall out as noise at each split rather than joining any child, so a parent's masked
count is not the sum of its children's. A rollup that unions the children and calls the result the
parent is wrong on every real hierarchy while passing on every planted one.

**The taxonomy is the covering counterpart.** Every paper's primary category sits in exactly one
archive, so an archive *is* the union of its classes — and the two layers put a covering and a
non-covering hierarchy side by side over the same points.

**Toponymy is the tiered shape over a density clustering** rather than a published taxonomy, which
puts those two cases side by side in turn: its rungs are not covering, since a paper that is noise
on one rung belongs to nothing there. Its labels are the model's names, one per cluster on every
rung, and the coarser rungs were named from the finer ones beneath them. It is the one layer that
still writes its names as a label layer of its own.

## Reproducibility, exactly

This is the ladder's one irreproducible corpus, and the imprecise version of that claim is worth
replacing with the exact one. Both routes run cuML's UMAP with a fixed `random_state`
(`routes.py`), and **that is enough for one of them and not the other.** Two runs at 200,000 on
2026-09-01, same seed, same sample:

| | run 1 | run 2 |
|---|---|---|
| `pca64` | x [-13.16, 16.03] y [-14.72, 18.22] | **identical** |
| `knn` | x [-14.16, 9.16] y [-15.01, 14.13] | x [-13.79, 8.87] y [-15.24, 13.29] |

⊘ **The `knn` route is not reproducible, and the seed is not where it goes wrong.** UMAP is handed
a graph, and the graph is what moves: CAGRA's index build is an approximate construction on the
GPU and takes no seed, so the two runs laid out two different neighbour graphs. It shows up
downstream — the same run pair gave 70 and 75 selected HDBSCAN clusters, 184 and 188 in the tree.
`pca64` reproduced bit for bit, PCA being deterministic and cuML's UMAP being seeded.

Neither view is `data/geometry.parquet` — which every figure in `docs/evidence/` was taken against,
and which was built by a **different** UMAP under no seed at all. A build figure taken against one
geometry is not comparable with a figure taken against another, and the manifest records which run
produced which.

The CPU `umap-learn` path this rung used to run is gone, and with it `--umap reuse` and the
`geometry.parquet` reuse route. Speed won over fidelity: this is a demonstrator, and on one
200,000-point graph cuML lays out in 6.4 s where seeded `umap-learn` took 82.7 s.

## The environment

```bash
uv venv --python 3.12 ~/venvs/projection
VIRTUAL_ENV=~/venvs/projection uv pip install \
    --extra-index-url https://pypi.nvidia.com -r test_corpora/arxiv/requirements.txt
```

Not `~/venvs/ingest`, which is the geographic rungs' DuckDB and PyArrow. This rung needs cuVS, cuML
and CuPy on the GPU, scikit-learn and SciPy on the CPU, and — for the second stage — toponymy,
sentence-transformers and a CPU torch. `requirements.txt` says which are pinned and why.

⊘ **The `hdbscan` package is not in this environment**, and the rung does not need it: cuML's
HDBSCAN exposes the same condensed tree as a record array, which `prepare.py`'s `Tree` reads
directly. Its `condensed_tree_` *property* wraps `hdbscan` and raises without it, which is why the
private array is what is read.

**Sources.** `TESSERA_DATA` names the checkout holding `data/`; a worktree has none of its own.
This rung's source is *derived* rather than staged: `data/` is what `probes/build_corpus.py` and
`probes/build_embeddings.py` produced, and the share's `arxiv-tessera/2026-07-27/` is a mirror of
that directory, a backup rather than a publisher's bytes.

**The language model.** `toponymy.py` talks to any OpenAI-compatible chat endpoint; the default is
the one this machine hosts, Unsloth Studio on `http://127.0.0.1:8888/v1` serving Qwen 3.6 35B-A3B
through a llama-server with four slots. Reasoning is switched off through the chat template — a
name is 128 tokens of JSON, and a thinking trace in front of it is where the tokens and the grammar
both go wrong.

⊘ **From WSL2 the Windows loopback is not reachable in the default NAT networking mode**, and both
Unsloth Studio (which also wants a bearer token) and the llama-server behind it bind to `127.0.0.1`
on the Windows side. Either put WSL into mirrored networking (`networkingMode=mirrored` in
`.wslconfig`), forward a port on the Windows side, or point `--url` at wherever the model actually
answers. The stage pings the endpoint before it spends anything on keyphrases, so a wrong URL fails
in the first second rather than the last.

`--llm mock` names each cluster after its first keyphrase. It exists so the plumbing can be checked
in seconds without a model on the other end; its labels are not labels, and the manifest says so.

## Serving it

```bash
D="$TESSERA_LADDER/arxiv"
./run_demo.sh --deployment "$D/tessera.toml" \
    --terms "$(cat "$D/category-terms.txt")" --ranks "$D/category-ranks.json" \
    --label 'arXiv: two projections' --prose title,abstract
```

`prepare.py` writes the deployment file and the two demo inputs beside the data. **The terms file
is not optional decoration**: a term id names a different set in every dictionary, so without
`--terms` every principal measures empty against the demo's synthetic `0..200` and the viewer opens
on a blank map with nothing to say why.

A grant is written in the corpus's own vocabulary, because the access terms are the category names
the points file carries: `{"terms": ["math.GT"]}` is a principal. The pair worth comparing is a set
of categories against one of them rather than two unrelated ones — two unrelated categories see two
disjoint sets of clusters, which shows nothing, where a subset relationship puts *the same clusters*
in front of both principals with a different count beside each.

## The frame report, which is the thing to read

Each view's `extent` is `auto`, which squares a box around the coordinates the run produced. Every
build says what that box does to the data, because an extent is four plausible-looking numbers
whatever the corpus holds — and here it says it **twice**, once per view, which is where the two
layouts stop being an assertion.

Three numbers make that a healthy build. The data's bounds sit **inside** the frame with headroom.
Nothing **clamps** — quantisation clamps rather than filters, so a point outside the frame is stored
on its edge, and past half the corpus the build refuses outright. And nearly every point keeps **a
position of its own**, so two papers far apart in the embedding are far apart on the map.

**What the failure looks like, since this pipeline used to produce it.** Writing raw UMAP
coordinates against a stated frame of `0,65536,0,65536` puts the whole corpus in the bottom-left
corner: the bounds line reads `18 x 23 of the 65536 x 65536 cells`, nothing clamps, no error is
raised anywhere, and the last line collapses to a percent or two — every cluster piled into a
handful of cells that no amount of zooming separates. A frame is not a formatting choice, which is
why it belongs to the view in the declaration rather than to whoever typed the build command.

Two more reports land beside the bundle in `reports/`. `containment.json` names every parent/child
edge whose child holds a member its parent does not, and `disclosure.json` records every gate and
every member requirement the declaration set — the document to diff when asking what a change did
to who may see what.

## Measured

**By this script, 2026-09-01**, WSL2, 12 cores, one RTX 3080 (10 GB, shared), whole corpus:
**19 m 39 s** end to end, 22.7 GB peak RSS. Streaming the 9.9 GB embedding file is 99 s; the two
routes are 153 s (`knn` — 99 s for the CAGRA graph, 49 s for the layout) and 574 s (`pca64` — 211 s
for PCA, 363 s for the layout); cuML's HDBSCAN 274 s and its k-means under 1 s. 64 PCA components
keep 82.2% of the variance, and 97.2% of the CAGRA graph's rows came back with themselves first —
the other 2.8% were repaired (`routes.py`).

**The `pca64` route is 3.8× the `knn` route**, which inverts the reason PCA was there. Reducing to
64 dimensions costs 211 s and then makes UMAP's own graph build *slower* than handing it a graph
built in full dimension on the card: 363 s against 49 s. Full dimension is the cheaper route here,
not the expensive one.

HDBSCAN at a floor of 6,056: 73 selected clusters, 24.8% of papers in none of them, 217 nodes 32
deep before the chain collapse and 186 nodes 12 deep after it; a mean stray share of 14.0% over the
77 internal clusters, **none of which its children exhaust**. 64 k-means clusters, 206 … 98,987
members. 45,958 candidate terms after dropping corpus vocabulary; 64 of 64 k-means clusters and 184
of 186 HDBSCAN clusters got a distinctive title, the other two carrying the fallback alone. 459
artifacts, 23,225,589 member rows, three layers.

`tessera build` over that output: **52.5 s**, a 1.4 GB bundle, 4,163,155 (paper, category) pairs,
115 splits of which 77 are non-covering, and no containment violation. `tessera verify --deep`
passes: 1 partition, 2 views, 2 segments, 4,844,972 rows.

⊘ **The 20,000-paper figures this README used to quote are gone rather than superseded**, and so
are the whole-corpus ones: they were taken against seeded `umap-learn` on the CPU and against
`data/geometry.parquet`, which no run of this script now produces. Do not compare across them.

### The build's own report, verbatim

The frames, one per view — the two ranges are what "the frames cannot be shared" means as numbers:

```
view 'knn': quantising against x [-20.766528968811034, 21.15820873260498], y [-19.73285186767578, 22.191885833740233]
        the data spans x [-17.41645050048828, 17.808130264282227], y [-19.32182502746582, 21.780858993530273] — 55064 x 64252 of the 65536 x 65536 cells
        2422486 point(s) placed, none on the frame's edge
view 'pca64': quantising against x [-20.70003490447998, 19.814113426208497], y [-20.45219783782959, 20.061950492858887]
        the data spans x [-20.216201782226563, 19.330280303955078], y [-20.05500030517578, 19.664752960205078] — 63972 x 64252 of the 65536 x 65536 cells
        2422486 point(s) placed, none on the frame's edge
```

The resolution, one per view:

```
view 'knn': 2422486 point(s) landed in 2389563 distinct cell(s) — 98.6% of them have a position of their own
view 'pca64': 2422486 point(s) landed in 2330210 distinct cell(s) — 96.2% of them have a position of their own
```

**And the per-view artifact layouts, which are the point of the whole exercise.** The same 459
artifacts over the same memberships, laid out in each view:

```
artifact layouts, chosen from the bundle's own row space (267 ms):
  clusters/hdbscan level 0 [knn]: 186 artifact(s) with rows, 0.312 everywhere, 3.8 blocks/artifact, overlapping — served rows
  clusters/kmeans level 0 [knn]: 64 artifact(s) with rows, 0.391 everywhere, 3.5 blocks/artifact, disjoint — served rows
  taxonomy/arxiv level 0 [knn]: 38 artifact(s) with rows, 0.974 everywhere, 24.3 blocks/artifact, disjoint — served rows
  taxonomy/arxiv level 1 [knn]: 171 artifact(s) with rows, 0.982 everywhere, 25.2 blocks/artifact, disjoint — served rows
artifact layouts, chosen from the bundle's own row space (256 ms):
  clusters/hdbscan level 0 [pca64]: 186 artifact(s) with rows, 1.000 everywhere, 26.2 blocks/artifact, overlapping — served rows
  clusters/kmeans level 0 [pca64]: 64 artifact(s) with rows, 1.000 everywhere, 29.7 blocks/artifact, disjoint — served rows
  taxonomy/arxiv level 0 [pca64]: 38 artifact(s) with rows, 1.000 everywhere, 27.9 blocks/artifact, disjoint — served rows
  taxonomy/arxiv level 1 [pca64]: 171 artifact(s) with rows, 0.988 everywhere, 31.3 blocks/artifact, disjoint — served rows
```

**Read the `everywhere` fraction and the blocks per artifact.** `everywhere` is the share of a
level too wide for any node of the tile index, and blocks per artifact is how many contiguous
row-ID runs a cluster's membership breaks into once entities are ordered by Morton code in that
view. In `knn` the two clusterings sit at 0.31 and 0.39 with under 4 blocks each; in `pca64` every
level is at **1.000** with 26 to 30. The same clusters, the same members, and in one layout they
are compact in row space while in the other not one of them fits under a tile-index node.

That is the measurement the two views were built to make, and it is not one an offline comparison
of two builds could produce: it is the same membership resolved twice, by the engine, into the
structure it actually serves from. **Contiguity in entity space is the highest-leverage property in
the index** — so on this corpus the route that keeps it is the one that never reduces.

⊘ **It is one corpus and one clustering pair, and it says nothing about latency**, which nothing
here measured. It is a property of the stored layout, not a served-request figure.

### What `compare.py` says, whole corpus

```
                      knn      pca64
2D recall           8.30%      9.85%
purity             72.29%     72.60%
frame use          37.27%     42.83%
z8                 13,754     14,547   distinct cells, fitted frame
                   32,093     27,910   distinct cells, 1-99% frame
z12             1,129,975    820,033   distinct cells, fitted frame
                1,585,766  1,155,839   distinct cells, 1-99% frame
z16             2,393,465  2,332,988   distinct cells, fitted frame
                2,348,471  2,312,312   distinct cells, 1-99% frame
```

**The premise the two routes were built to test is NOT confirmed.** PCA-64 does not lose
neighbourhoods or scatter concepts relative to full dimension: 2D recall@15 is 9.85% against 8.30%
and category purity 72.60% against 72.29% — both marginally in *favour* of the reduced route, and
both differences small enough that no conclusion should be hung on their direction. At 200,000 the
same pair read 18.22% / 16.52% and 71.58% / 71.50%. Whatever PCA discards at 64 components, the
2D layout was never going to carry it: an 8-10% recall is UMAP's own 1024 → 2 loss dominating, and
that is what the two routes have in common.

Where they differ is everything downstream of *how the layout fills the grid* — frame use, cell
occupancy at zoom 12, and above all the artifact layout. The reasons to prefer `knn` on this corpus
are that it is 3.8× cheaper to compute and that its clusters are contiguous in row space; neither
is the fidelity argument the experiment set out to make.

⊘ **`compare.py` is 15 minutes at the whole corpus**, almost all of it the exact brute-force ground
truth over 20,000 queries and the two 2.4×10⁶-point KD-trees. It reads the embeddings again.
