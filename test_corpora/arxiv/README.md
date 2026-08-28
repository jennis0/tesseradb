# arXiv — the ladder's bottom rung, and its only embedding corpus

**2,422,486 preprints**, from the metadata snapshot and one BGE-large-en-v1.5 topic embedding per
paper. Every other rung is geographic: its positions come from a projection, which is a pure
function, so a frame change costs a rerun. This one's positions come from UMAP over an embedding,
which is not — and that difference is the reason the rung is here rather than a matter of scale.

It is also the corpus that carries the artifact catalogue: three hierarchy shapes over one point
set, a covering hierarchy beside a non-covering one, and two kinds of label — TF-IDF terms, and
names written by a language model.

```bash
# the corpus: three clusterings, the taxonomy, TF-IDF labels
~/venvs/arxiv/bin/python -m test_corpora.arxiv.prepare --sample 200000

# optional: a fourth clustering, named by a chat model (needs an endpoint — see below)
~/venvs/arxiv/bin/python -m test_corpora.arxiv.toponymy --llm mock

cd "$TESSERA_LADDER/arxiv" && tessera check && tessera build
```

## The two stages, and why they are two

`prepare.py` is the corpus. `toponymy.py` adds two layers to the directory it wrote, and splices
its declaration into the copy of `corpus.toml` beside the data.

They are separate because the second one needs a chat endpoint and because it *is* the run: over
the whole corpus the first stage is twenty minutes and the second is an hour and a half, almost all
of it waiting on the model. Trying a different floor or a different model is then a rerun of the
cheap half of the pipeline rather than of all of it.

The declaration is split the same way. `corpus.toml` in git is complete and valid on its own — five
layers, no Toponymy — and `toponymy.toml` holds the sixth and seventh, which the second stage
injects at two marker comments. A missing marker is a refusal: a splice that silently did nothing
would leave a build reading a declaration with no Toponymy layer while the stage's files sat beside
it.

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
rung, and the coarser rungs were named from the finer ones beneath them.

## The three geometries, and which one a run produced

This is the ladder's one irreproducible corpus, and the imprecise version of that claim is worth
replacing with the exact one. `data/geometry.parquet` was built with cuML on a GPU and is
bit-reproducible under no seed at all. umap-learn is reproducible when *seeded* — and seeding also
makes it single-threaded, which is hours for the whole corpus against minutes. So:

| | |
|---|---|
| `--umap reuse` | reads `data/geometry.parquet`, the projection every figure in `docs/evidence/` was taken against. Reach for it when a run has to be comparable with those |
| `--umap recompute`, seeded | reproducible bit for bit from this script alone |
| `--umap recompute`, parallel | a **hashed artifact** in the same sense `geometry.parquet` is: computed once, kept, named by what produced it rather than re-derived |

`--seeded auto` draws the line at 200,000 papers. The manifest records which of the three a run
produced, and a build figure taken against one is not comparable with a figure taken against
another.

## The environment

```bash
uv venv --python 3.12 ~/venvs/arxiv
VIRTUAL_ENV=~/venvs/arxiv uv pip install -r test_corpora/arxiv/requirements.txt
```

Not `~/venvs/ingest`, which is the geographic rungs' DuckDB and PyArrow. This rung needs
scikit-learn, umap-learn, hdbscan, and — for the second stage — toponymy, sentence-transformers and
a CPU torch. `requirements.txt` says which are pinned and why.

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

## Measured

**By this script, 2026-08-28**, WSL2, 12 cores, `--sample 20000`, seeded: **118 s** end to end — of
which streaming the sample's rows out of the 9.9 GB embedding file is 70 s and seeded UMAP 39 s.
64 PCA components keep 82.3% of the variance. HDBSCAN's condensed tree is 263 nodes 20 deep before
the chain collapse and 245 nodes 14 deep after it; 97 selected clusters, 17.5% of papers in none of
them, and a mean stray share of 8.8% over the 113 internal clusters. 815 artifacts, 320,970 member
rows, five layers.

**By the notebook this rung replaces**, and therefore to be re-measured before quoting — the
pipeline is the same but the code is not:

| Run | Wall | What dominated |
|---|---|---|
| 20,000, mock, 2026-08-24 | 370 s | the Toponymy pass 261 s, of which **217 s is BGE encoding the 19,249-phrase keyphrase vocabulary on the CPU** at ~90 phrases/s. 194 model calls for 204 clusters |
| 2,422,486, PCA + parallel UMAP only | 982 s | UMAP; loading and PCA 88 s. 64 components keep 82.2% |
| 50,000, live, 2026-08-25 | 1,875 s | the Toponymy pass 1,760 s — 235 s encoding, ~1,500 s in **477 model calls** for 441 clusters (rungs of 289, 101, 37, 14), four in flight, ~3 s per call |
| 2,422,486, live, 2026-08-25 | 6,294 s | the Toponymy pass 5,338 s — 276 s encoding and **870 model calls** for 797 clusters (574, 161, 46, 16 at a floor of 1,211 papers), two in flight, ~5.8 s per call. Parallel UMAP 521 s, HDBSCAN 53 s, writing the 102.7 M member rows 70 s |

In none of the live runs did a prompt need truncating, no name came back empty, and every name on
every rung was distinct. Names run 2.3 words at the root to 11.1 at the leaves — Toponymy's detail
ladder, and `lowest_detail_level` is the knob if the leaves are too long for a map label.

**The keyphrase encode does not grow with the corpus** — the vocabulary is capped — and the model
calls grow with the cluster count, which the floor sets. `--min-cluster` is what prices the run.

`tessera build` over the whole-corpus output: **5 m 38 s**, 4.9 GB peak, a 1.4 GB bundle, 389
splits of which 288 are non-covering, and no containment violation. The 50,000 build reported 317
splits and 247 non-covering; the 20,000 build 236 and 176. Non-covering splits are the algorithm's
and are what the corpus is for.

## The frame report, which is the thing to read

The view's `extent` is `auto`, which squares a box around the coordinates the run produced. Every
build says what that box does to the data, because an extent is four plausible-looking numbers
whatever the corpus holds:

```
view 's0': quantising against x [-20.27, 22.27], y [-18.93, 23.62]
        the data spans x [-16.43, 18.44], y [-18.51, 23.20] — 53718 x 64252 of the 65536 x 65536 cells
        20000 point(s) placed, none on the frame's edge
view 's0': 20000 point(s) landed in 19992 distinct cell(s) — 100.0% of them have a position of their own
```

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

## Asking it something

A grant is written in the corpus's own vocabulary, because the access terms are the category names
the points file carries: `{"terms": ["math.GT"]}` is a principal. The pair worth comparing is a set
of categories against one of them rather than two unrelated ones — two unrelated categories see two
disjoint sets of clusters, which shows nothing, where a subset relationship puts *the same clusters*
in front of both principals with a different count beside each.
