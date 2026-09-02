# PaperSeek + OpenAlex — the ladder's largest rung, and the first bundle larger than the box

**102,117,343 OpenAlex works**, each with a 1024-dimensional Stella V5 embedding a third party
computed from its title and abstract and released on Hugging Face, joined to OpenAlex itself for a
four-level topic tree, a publication year, a work type, an open-access flag and — the rung's
compartment — **a licence**. 2.8× rung 3's rows at 1.33× the width, one view, and the abstracts
**on**.

It is a **demonstrator and a speed benchmark** (owner rulings 2026-09-01 and 2026-09-02: this
corpus tests Tessera's speed and memory, not the UMAP pipeline; layout quality matters only as far
as the demo looks good). Recall against an exact neighbour search is not measured and layout
fidelity is not judged.

**What the rung is for is one number and what happens either side of it: a bundle bigger than the
box's memory.** Nothing about the declaration is trimmed to make it fit — the abstracts are 118.9
GB of characters uncompressed and they are indexed as text, because a corpus trimmed to fit deletes
the finding.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# once: 235.6 GB off the share, resumable per chunk — the only pass over the publisher's bytes
~/venvs/projection/bin/python -m test_corpora.paperseek.stage

# the corpus; --sample 0 takes all 102,117,343, --drop-vectors frees the 209 GB memmap after
~/venvs/projection/bin/python -m test_corpora.paperseek.prepare --sample 0 --drop-vectors

cd "$TESSERA_LADDER/paperseek" && tessera check --payloads && tessera build --stage-timings
```

## Two tracks, one package

`stage.py`, `sources.py`, `routes.py`, `prepare.py`, `spread.py`, `drive.py`, `corpus.toml` and
this file are the **vectors** track. `openalex.py`, `extract.py` and `smoke.py` are the **OpenAlex**
track's: the one full scan of `works`, the topic tree, the per-work resolve and the tiered layer.
`prepare.py` imports `openalex.py` guarded, exactly as rung 3 imports `mesh.py` — a run without it
writes a corpus with no topic layer, no licence vocabulary and a `point_visibility` naming no
field, and the declaration beside the data says so in place of each of the three blocks.

## One view

Stella V5 1.5B embeds `Title: {title}\n[SEP] Abstract: {abstract}` as one general text encoding, so
unlike rung 3's retrieval-trained MedCPT the geometry is a reasonable stand-in for what a work is
about. The view is `knn`, titled **Scholarly map**. `projection = "none"` and `extent = "auto"`: an
embedding layout is not a map and there is no transform between these coordinates and any ground.

## The compartment is real, and it runs the other way

`licence` — the OpenAlex licence of a work's best open-access location. **This is the ladder's
first compartment that is a property of the row rather than a synthetic stand-in**: GeoNames and
Overture compartment on a country of convenience and MedCPT on the branch letters of an indexing
vocabulary, and each of those is a policy invented for the fixture. A licence is a rights fact
about the work.

**A work with no licence carries no access term and takes the view's declared `public` default**,
which is the opposite of rung 3's shape, where every article carried a term and the default could
never fire. So the principal ladder runs the other way: a principal holding no term at all already
sees the unlicensed majority, and each licence key adds a compartment to it.

⊘ **That is the honest shape of the source and it is not corrected.** An `unlicensed` sentinel term
would make the majority a compartment nobody sees without being granted it — a policy this corpus
does not carry, and one that would make every masked count on the map a statement about a fiction.

## The route — fit on what the card holds, place the rest

Rung 3's route, unchanged in shape and re-measured for this corpus's width. The matrix is
102,117,343 × 1024 float16 — **209 GB** — against rung 3's 55 GB, so the case for fitting on a
sample is the same case a third again over.

1. **Fit** UMAP on a uniform sample of **1,500,000** rows, through one CAGRA index over its own kNN
   graph.
2. **Place** every other row at the **similarity-weighted mean of its 15 fit-set neighbours'
   positions**, searched against an index over the same fit set. Every row goes through this path,
   fit rows included, and the fit rows are then overwritten with their own UMAP positions.

**`FIT_ROWS` moved from rung 3's 2,500,000 and the move was measured, not extrapolated.** At 1024
dimensions the fit set's raw block alone is 2.05 GB per million rows against 1.54 GB at 768, so
rung 3's size does not transfer. Measured on this box (RTX 3080, 10 GB, ~7.5 GB free — another
session held 2.7 GB of the card), 2,000,000 rows sampled, 2026-09-02:

| fit rows | CAGRA build | search | graph peak VRAM | self-first | UMAP | layout peak VRAM |
|---|---|---|---|---|---|---|
| 1,500,000 | 24.8 s | 73,171 /s | **4.49 GB** | 99.98% | 15.3 s | **2.20 GB** — 1,576 B/row |

⊘ **Only one fit size was tried, and that is the owner's ruling applied rather than an omission**:
speed over accuracy, take the first route that works. 1,500,000 fits with room to spare and a
larger fit set buys nothing this rung measures.

⊘ **The index is built twice — once for the graph, once for the placement** — because it cannot be
held across the layout. Rung 3 measured 5.46 GB and 2.99 GB separately against ~8.2 GB free; a
wider vector makes that worse, not better.

⊘ **The layout is not reproducible under a seed.** `random_state` is fixed and CAGRA's index build
takes none, so the graph UMAP is handed differs run to run — the arXiv rung's ⊘, unchanged.

## Nothing holds a text column whole

Rung 3 read its abstracts into Arrow and wrote `points.parquet` in one call. **118.9 GB will not fit
on a 47 GB box**, so this rung streams: `stream_staged` yields one source row group at a time, and
the OpenAlex resolve, the topic layer's member write and the `points.parquet` write all ride that
one pass. The clustering's membership is streamed for the same reason — `ArtifactSet.members` holds
one Python list entry per member row, and there are 10⁸ of them — which is why
`ArtifactSet.stream_members` grew an optional `rank`: a layer may only be written one way, and this
one has 10⁸ membership rows and a few hundred ranked generating-set rows in the same file.

## Measured

*(figures to follow — this section is written when the runs land)*

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv and MedCPT rungs; `requirements.txt` points at the arXiv rung's. `drive.py` is the one
exception and runs on the system `python3`: it needs `requests` and `pyarrow` and nothing the
rung's environment carries.
