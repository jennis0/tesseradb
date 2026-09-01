# arXiv — the ladder's bottom rung, its only embedding corpus, and its only two-view one

**2,422,486 preprints**, from the metadata snapshot and one BGE-large-en-v1.5 topic embedding per
paper. Every other rung is geographic: its positions come from a projection, which is a pure
function, so a frame change costs a rerun. This one's positions come from UMAP over an embedding,
which is not — and that difference is the reason the rung is here rather than a matter of scale.

It is also **the demo corpus**: two clusterings over one point set — a flat one beside a
non-covering tree — a title on every cluster, and, optionally, a third clustering named by a
language model.

```bash
# the corpus: two layouts, two clusterings, a title on every cluster
~/venvs/projection/bin/python -m test_corpora.arxiv.prepare --sample 200000

# optional: a third clustering, named by a chat model (needs an endpoint — see below)
~/venvs/projection/bin/python -m test_corpora.arxiv.toponymy --llm mock

cd "$TESSERA_LADDER/arxiv" && tessera check --payloads && tessera build
```

## Two views over one entity space

Owner direction, 2026-09-01. The same papers, the same attributes and the same clusterings —
positioned twice, each layout in its own frame:

| view | title | route | positions |
|---|---|---|---|
| `knn` | Topic map | a cosine kNN graph in **full 1024 dimensions** (cuVS CAGRA, index built in fp16, k = 15) handed to UMAP as a `precomputed_knn`, so UMAP does the layout and nothing else | `points.parquet`, with every attribute column |
| `pca64` | Topic map (PCA-64) | PCA to 64 components first — the route `data/geometry.parquet` was built on | `points-pca64.parquet`: identity, position, access column |

**They are here to exercise the multi-view machinery on a real corpus**, and because `knn` is the
route that scales to the ladder's later embedding rungs. They are not a fidelity experiment: which
of the two projections better preserves the embedding is a question about UMAP, not about Tessera,
and nothing here measures it.

`knn` is the **anchor** (`[defaults].allocation_view`, decision 0112): entity ids are ordered by
Morton code in that view, so naming it explicitly is what stops a reordering of the view blocks
silently re-keying a rebuild. It is also the layout the clusterings are computed over. **Two
measured facts decided which view got the job**, and both are Tessera-side:

- **The `knn` route is 3× faster end to end** — 94 s against 280 s over the whole corpus.
  Reducing to 64 dimensions costs 24 s and then leaves UMAP a graph to build that is *slower*
  (257 s) than the one the card had already built in full dimension (27 s).
- **A cluster is 3.5-4.1 contiguous row runs in `knn` and 27-29 in `pca64`**, with two thirds to
  three quarters of each level under a tile-index node in the first and none of it in the second.
  Entities are ordered by Morton code in the anchor view, so the anchor decides how much of a
  membership the index can bound — a serving-cost property of the layout, and the build reports it
  per view.

**Every layer is named on both views**, which is what the second view is for: a `computed` content
is recomputed per viewer from `membership ∩ M_auth` — and, a view being a row space of its own, per
view — so every cluster carries a centroid, a box and a hull in each layout over one membership.

**The frames cannot be shared.** Each route's UMAP output has its own coordinate range, so
`extent = "auto"` fits a different box to each; one frame declared for both would put one route's
points in a corner of the other's grid, with no clamp and no error to say so.
`projection = "none"` on both: an embedding layout is not a map.

## Every cluster carries its own title

The c-TF-IDF label is **supplied `text` content on the cluster artifact itself**, ranked — the
specific description first, `a cluster of papers` second — with each rank's generating set recorded
as member rows at that rank. A client names an artifact from its own first content
(`clients/ts/deck/src/layer.ts`), so a cluster shows its title with no join.

It used to be an artifact of its own on a `topics/*` label layer that the client joined by
attachment. Those layers are gone, along with their sources and their `depends_on` edge; what they
declared is not: the layer gate, the artifact gate and the content's provenance requirement all
moved onto the clustering layer. The content's requirement stays `all` — the text is a synthesis of
the titles it was drawn from, so it is read only by a viewer who can already read every one of
them.

**The denominator is the layer's own sibling clusters, not the corpus.** Four terms, scored by how
much more a term occurs in this cluster than across the clusters it is drawn beside. Scoring
against a corpus average produced the register of a physics paper rather than a topic —
`production measurement sqrt`, `tev sqrt search`, `brauer production modular` — because a term
common to a whole discipline clears a corpus bar in every cluster of that discipline. A short
explicit stoplist (`CORPUS_STOPLIST`) removes the handful that survive both rules; it is kept short
on purpose, a stoplist that grows being a labeller hand-tuned rather than fixed.

**No distinctive terms is the fallback alone.** A cluster the labeller has nothing to say about
carries `a cluster of papers` and nothing above it. Dropping its content entirely is refused at
publication — the layer declares a supplied kind, and an artifact served without content its layer
declares cannot be told apart from one whose content was withheld — so the ranking is what absorbs
the case. That refusal is a real difference from the label-layer shape, where a cluster with no
distinctive terms simply had no label artifact.

## ⊘ `taxonomy/arxiv` is withdrawn

arXiv's own classification was a third layer here — two tiered levels, archives over subject
classes, the covering counterpart to HDBSCAN's non-covering tree. It is **withdrawn** (owner
ruling, 2026-09-01) on the rule Overture's taxonomy was withdrawn on: *a layer earns its place by
drawing something in the view it is declared over.*

Measured in the `knn` view — the box of the middle 90% of an artifact's members, as a share of the
map:

| layer | median | under 5% of the map | over 25% of it |
|---|---|---|---|
| `clusters/kmeans` | **1.0%** | 94% | — |
| `clusters/hdbscan` | **0.3%** | 94% | — |
| `taxonomy/arxiv` level 0 (archive) | 13.7% | — | **18%** |
| `taxonomy/arxiv` level 1 (subject class) | 9.6% | — | **12%** |

`hep-th` and `gr-qc` each cover 34% of the map and `physics.hist-ph` 64%. A cluster is compact in
the layout it was fitted in; a published category is not, because nothing put its papers in one
place. And it is not free: 97-98% of the layer's artifacts came back `everywhere` in the build's
own report, so all 209 were served on every viewport request to draw outlines that show nothing.

**The classification itself is not gone.** It is the `archive` and `primary_category` attributes —
`render = true`, so a client colours by them, and `index = true`, so a client filters on them.
That is what a published taxonomy over scattered points is good for; a hierarchy of artifacts over
it was not.

## What a client is given

| attribute | type | | |
|---|---|---|---|
| `archive` | category | render, index | `math`, `hep-th` — the colour-by axis |
| `primary_category` | category | render, index | `math.GT` — the finer filter |
| `submitted_at` | `timestamp_us` | render, index | a paper map is asked for a date range first |
| `title` | text | index | `match` and `phrase`; drill-down |
| `abstract` | text | index | as above |
| `authors` | text | index | the surnames `corpus.parquet` carries, joined |
| `arxiv_id` | keyword | index | the external identifier |

Prose lives in the record blob and reaches a client at drill-down: `render` on a text column is
refused, the hot column being a fixed-width slot per row.

## Three shapes over one corpus

The point of publishing all of them is that they behave differently, and the differences are the
system's subject rather than the clustering's:

| Layer | Shape | A coarser view is | A budget |
|---|---|---|---|
| `clusters/kmeans` | `flat` | nothing — every cluster is a peer | inert |
| `clusters/hdbscan` | `nested` — a tree, edges within one level | an **ancestor**, and the cut climbs to it | trades depth for count |
| `clusters/toponymy` | `tiered` — one level per rung of Toponymy's ladder | **a coarser rung** | inert |

**HDBSCAN's tree is here because its children do not exhaust it.** A fifth to a quarter of a
parent's points fall out as noise at each split rather than joining any child, so a parent's masked
count is not the sum of its children's. A rollup that unions the children and calls the result the
parent is wrong on every real hierarchy while passing on every planted one.

**Toponymy is the tiered shape over a density clustering**, and it is not covering either: a paper
that is noise on one rung belongs to nothing there. Its labels are the model's names, one per
cluster on every rung, and the coarser rungs were named from the finer ones beneath them. It is the
one layer that still writes its names as a label layer of its own.

## The two stages, and why they are two

`prepare.py` is the corpus. `toponymy.py` adds two layers to the directory it wrote, and splices
its declaration into the copy of `corpus.toml` beside the data.

They are separate because the second one needs a chat endpoint and because it *is* the run: over
the whole corpus the first stage is minutes and the second is an hour and a half, almost all of it
waiting on the model. Trying a different floor or a different model is then a rerun of the cheap
half of the pipeline rather than of all of it.

The declaration is split the same way. `corpus.toml` in git is complete and valid on its own — two
layers, no Toponymy — and `toponymy.toml` holds the third and its label layer, which the second
stage injects at two marker comments. A missing marker is a refusal: a splice that silently did
nothing would leave a build reading a declaration with no Toponymy layer while the stage's files
sat beside it.

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
and CuPy on the GPU, scikit-learn on the CPU, and — for the second stage — toponymy,
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

Two more reports land in `bundle/reports/`. `containment.json` names every parent/child edge whose
child holds a member its parent does not, and `disclosure.json` records every gate and every member
requirement the declaration set — the document to diff when asking what a change did to who may see
what.

## Measured

**By this script, 2026-09-01**, WSL2, 12 cores, one RTX 3080 (10 GB), whole corpus, box otherwise
idle: **13 m 0 s** end to end, 22.9 GB peak RSS. Streaming the 9.9 GB embedding file is 79 s; the
two routes are 94 s (`knn` — 65 s for the CAGRA graph, 27 s for the layout) and 280 s (`pca64` —
24 s for PCA, 257 s for the layout); cuML's HDBSCAN 262 s and its k-means 1 s. 64 PCA components
keep 82.2% of the variance, and 97.2% of the CAGRA graph's rows came back with themselves first —
the other 2.8% were repaired (`routes.py`).

**The `pca64` route is 3.0× the `knn` route**, which inverts the reason PCA was there: full
dimension is the cheaper route, not the expensive one. ⊘ An earlier run on a contended box read
153 s and 574 s — 3.8× — so the ratio is stable around 3-4× and the walls are not comparable across
runs.

HDBSCAN at a floor of 6,056: 59 selected clusters, 21.6% of papers in none of them, 209 nodes 27
deep before the chain collapse and 192 nodes 13 deep after it; a mean stray share of 12.4% over the
87 internal clusters, of which only 3 are exhausted by their children. 64 k-means clusters,
157 … 90,696 members. 45,943 candidate terms after dropping corpus vocabulary; 64 of 64 k-means
clusters and 190 of 192 HDBSCAN clusters got a distinctive title, the other two carrying the
fallback alone. 256 artifacts, 19,707,995 member rows, two layers.

`tessera build` over that output: **54.5 s**, a 1.5 GB bundle, 4,163,155 (paper, category) pairs,
87 splits of which 84 are non-covering, and no containment violation. `tessera verify --deep`
passes: 1 partition, 2 views, 2 segments, 4,844,972 rows. `run_demo.sh --deployment … --no-viewer`
serves it and measures all five principals, 243 papers visible at the narrowest.

⊘ **The 20,000-paper figures this README used to quote are gone rather than superseded**, and so
are the whole-corpus ones: they were taken against seeded `umap-learn` on the CPU and against
`data/geometry.parquet`, which no run of this script now produces. Do not compare across them.

### The build's own report, verbatim

The frames, one per view — the two ranges are what "the frames cannot be shared" means as numbers:

```
view 'knn': quantising against x [-20.591736488342285, 20.849623374938965], y [-20.217525177001953, 21.223834686279297]
        the data spans x [-17.757638931274414, 18.015525817871094], y [-19.811237335205078, 20.817546844482422] — 56574 x 64252 of the 65536 x 65536 cells
        2422486 point(s) placed, none on the frame's edge
view 'pca64': quantising against x [-20.70003490447998, 19.814113426208497], y [-20.45219783782959, 20.061950492858887]
        the data spans x [-20.216201782226563, 19.330280303955078], y [-20.05500030517578, 19.664752960205078] — 63972 x 64252 of the 65536 x 65536 cells
        2422486 point(s) placed, none on the frame's edge
```

The resolution, one per view:

```
view 'knn': 2422486 point(s) landed in 2387592 distinct cell(s) — 98.6% of them have a position of their own
view 'pca64': 2422486 point(s) landed in 2330210 distinct cell(s) — 96.2% of them have a position of their own
```

**And the per-view artifact layouts**, which are why `knn` is the anchor. The same 256 artifacts
over the same memberships, laid out in each view:

```
artifact layouts, chosen from the bundle's own row space (256 ms):
  clusters/hdbscan level 0 [knn]: 192 artifact(s) with rows, 0.219 everywhere, 4.1 blocks/artifact, overlapping — served rows
  clusters/kmeans level 0 [knn]: 64 artifact(s) with rows, 0.328 everywhere, 3.5 blocks/artifact, disjoint — served rows
artifact layouts, chosen from the bundle's own row space (275 ms):
  clusters/hdbscan level 0 [pca64]: 192 artifact(s) with rows, 1.000 everywhere, 26.6 blocks/artifact, overlapping — served rows
  clusters/kmeans level 0 [pca64]: 64 artifact(s) with rows, 1.000 everywhere, 29.0 blocks/artifact, disjoint — served rows
```

**Read the `everywhere` fraction and the blocks per artifact.** `everywhere` is the share of a
level too wide for any node of the tile index, and blocks per artifact is how many contiguous
row-ID runs a cluster's membership breaks into once entities are ordered by Morton code in that
view. In `knn` the two clusterings sit at 0.22 and 0.33 with 3.5-4.1 blocks each; in `pca64` both
are at **1.000** with 26.6 and 29.0. The same clusters, the same members, and in one layout they
are compact in row space while in the other not one of them fits under a tile-index node.

**Contiguity in entity space is the highest-leverage property in the index**, so this is what
`[defaults].allocation_view = "knn"` buys — and it is a measurement two separate builds compared
offline could not produce, being the same membership resolved twice by the engine into the
structure it serves from.

⊘ **It says nothing about latency**, which nothing here measured. It is a property of the stored
layout, not a served-request figure.

### Twenty cluster titles from each layer

The c-TF-IDF titles the build serves, sampled from the whole-corpus run:

| `clusters/kmeans` | `clusters/hdbscan` |
|---|---|
| graphene electronic magnetic films | string strings ads theories |
| language speech text translation | traffic vehicles autonomous vehicle |
| groups algebras homology knots | market financial stock trading |
| attacks security adversarial detection | simulations galaxies galaxy formation |
| varieties curves surfaces moduli | neural |
| turbulence flow flows turbulent | accretion turbulence instability magnetic |
| representations decays mathrm local | groups algebras spaces manifolds |
| representations cohomology local forms | spin states phase optical |
| dark neutrino matter neutrinos | graphs graph number cycles |
| microlensing x-ray blg- technicolor | entanglement field spin theories |
| protoplanetary disks financial market | collisions nuclear gev nuclei |
| superconductors spin superconductivity superconducting | circuits algorithm computation circuit |
| granular dynamics liquid polymer | seismic earthquake inversion earthquakes |
| elliptic curves hera navier-stokes | geometry gravity gauge manifolds |
| equations equation element numerical | brain neural spiking eeg |
| search tev collisions higgs | groups algebras varieties cohomology |
| groups categories rings finite | groups algebras homotopy cohomology |
| galaxies galaxy survey cluster | object image visual robot |
| physics graph atmosphere comet | thermodynamics nonequilibrium brownian active |
| solar magnetic coronal simulations | graph regression bayesian neural |

Against a corpus denominator the same clusters read `production measurement sqrt`,
`tev sqrt search` and `brauer production modular` — the register of a physics paper rather than its
subject. Two things this sample also shows honestly: a k-means cell can straddle two subjects
(`protoplanetary disks financial market`), which is the clustering and not the labeller; and a
cluster where only one term clears `MIN_CLUSTER_SHARE` gets a one-word title (`neural`) rather than
three filler terms after it.
