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

## ⊘ Two licence claims on the share are wrong, and neither is this corpus's `licence` column

Both are about the *dataset's* rights, not the per-work compartment below, and both are recorded
here because a figure quoted from either would be quoted wrongly.

- **The PaperSeek acquisition README says the rung is "unencumbered end to end" on a CC0 reading of
  the dataset card.** The card's own licence section says something narrower: OpenAlex's data is
  CC0, and *"embeddings generated as part of the PaperSeek framework are released for research
  purposes"* — which is not CC0 and is not a licence with terms. The Hugging Face metadata block
  says `cc0-1.0`; the prose beside it does not. This is a measurement fixture that is not
  distributed, so nothing here turns on it, and nothing should be published from these vectors on
  the strength of the README's sentence.
- **The OpenAlex acquisition README named a top-level `license` column on `works`.** There is none,
  on this vintage or any partition sampled; the licence lives at `best_oa_location.license`
  (`../../probes/2026-09-02-rung-4-share-reads/` §2). Corrected on the share 2026-09-02 by the
  OpenAlex track.

## Two tracks, one package

`stage.py`, `sources.py`, `routes.py`, `prepare.py`, `spread.py`, `drive.py`, `corpus.toml` and
this file are the **vectors** track. `openalex.py`, `extract.py` and `smoke.py` are the **OpenAlex**
track's: the one full scan of `works`, the topic tree, the per-work resolve and the tiered layer.
`prepare.py` imports `openalex.py` guarded, exactly as rung 3 imports `mesh.py` — a run without it
writes a corpus with no topic layer, no licence vocabulary and a `point_visibility` naming no
field, and the declaration beside the data says so in place of each of the three blocks. The
eleventh licence key is the one part of the compartment this track owns: `unlicensed` is a property
of the scheme rather than of OpenAlex's list, so it is appended here and not asked of
`OpenAlex.licences()`.

## One view

Stella V5 1.5B embeds `Title: {title}\n[SEP] Abstract: {abstract}` as one general text encoding, so
unlike rung 3's retrieval-trained MedCPT the geometry is a reasonable stand-in for what a work is
about. The view is `knn`, titled **Scholarly map**. `projection = "none"` and `extent = "auto"`: an
embedding layout is not a map and there is no transform between these coordinates and any ground.

## The compartment is real, and every work carries a term

`licence` — the OpenAlex licence of a work's best open-access location. **This is the ladder's
first compartment that is a property of the row rather than a synthetic stand-in**: GeoNames and
Overture compartment on a country of convenience and MedCPT on the branch letters of an indexing
vocabulary, and each of those is a policy invented for the fixture. A licence is a rights fact
about the work.

**A work with no licence — an unmatched id included — carries `unlicensed`**, an eleventh key of
the same closed vocabulary, so the access column is never empty and `point_visibility`'s `default`
never fires. Rung 3's `unindexed` has exactly this shape and exists for the same reason.

⊘ **That is an owner ruling (2026-09-03), not a property of the data.** The corpus was first
written the other way — no term, and the view's `public` default — which is the more literal
reading of a work that names no licence. It was ruled out because **the campaign's principal ladder
starts at 1% of the corpus and cannot be composed under a 77% floor every principal holds for
free**: a `public` default puts three quarters of the map in front of a viewer with no grant at
all, and every masked count taken against it measures that floor rather than the compartment.

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

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The staging pass is the one **network-source** figure.

### Staging — one pass off the share

**2026-09-02/03**, SMB. **164.7 minutes** for all 53 chunks, 22.2 GB peak `VmHWM` — most of it the
page cache behind a 209 GB memmap being written — producing **254 GB** locally: 195 GiB of
`vectors.f16` and 59 GB of per-chunk parquet. ⊘ **Not comparable with rung 3's 60.5 minutes for 163
GB**: the OpenAlex track's own full scan of `works` shared the share for about half of this pass,
and the per-chunk rate moved from 149 s to 219 s and back as it came and went.

| | |
|---|---|
| rows | 102,117,343 — counted from the 53 footers |
| with a title | 101,873,782 (**99.8%**) |
| with an abstract | 102,028,375 (**99.9%**) |
| ids not spelled as the full URL | 0 |
| zero-norm vectors | 0 |

The abstract coverage is the one figure worth holding beside rung 3's: PaperSeek selected works
that *have* an English title and abstract, so this is 99.9% where MedCPT's PubMed is 68.9%. The
corpus is not a sample of the scholarly record and no coverage claim should be read off it — 102M
of OpenAlex's ~322M works, by a selection that is a property of the publisher's pipeline.

### The 1,000,000-row sample

`prepare.py --sample 1000000` **601 s** at **12.53 GB** peak `VmHWM`: route 222 s (CAGRA build
4.6 s over the fit set, graph search 20.2 s, UMAP 8.9 s, placement 5.2 s — the rest is the gather of
a scattered 1M-row sample off the 209 GB memmap), the resolve/layer/points pass 348 s, titles 21 s,
k-means 0.3 s. 25 k-means cells (422 … 85,717, median 46,720), 25 of 25 titled out of 43,837
candidate terms.

**The OpenAlex join, on this sample:** 968,824 of 1,000,000 ids matched (**96.9%**), 965,584 carry a
topic, 968,528 a year, 416,542 are open access, and **223,783 (22.4%) carry a real licence** — the
other 776,217 carry `unlicensed`.

`tessera build` **23.3 s** to a **744.3 MB** bundle over 12 terms; `verify --deep` clean in **0.33 s
at 52.5 MB**. The build's memory split, by the text-peak probe's method:

| | anonymous | file-backed | `VmHWM` |
|---|---|---|---|
| whole run high-water | **968 MB** | 1,744 MB | 2,398 MB |
| the stage it lands on | `text_index` | `attribute_tail` | `text_index` |

**Served on 8131, and the ladder is what the ruling asked for**: 0 / 137,868 / 1,000,000 visible at
zoom 0 across *no terms*, `cc-by` and all eleven keys. `match` on `abstract` moves the matched count
without moving the visible one — 36,695 of 1,000,000 for `network`, 0 for a token the corpus does
not carry.

⊘ **The item and artifact drill-downs return 404 and 422.** `drive.py` asks for handles 1…20, which
are not `tessera_id`s, so nothing resolves — the same shape the memory-cap probe recorded, and it
measures the route's cost rather than a drill-down. The 422 on the artifact route is rung 3's open
attached-layer finding, not this rung's.

### Layer spread at 1,000,000 — both layers draw

The box holding the middle 90% of an artifact's members, as a share of the map. k-means exactly over
all 25; the topic tree over a uniform sample of 150 artifacts per level, keeping those with ten
members or more:

| | median | p90 | max | under 5% |
|---|---|---|---|---|
| `clusters/kmeans` (25) | **0.75%** | 1.06% | 3.64% | 100% |
| `topics/openalex` (329 sampled) | **2.35%** | 6.96% | 28.2% | 78% |
| *by level:* domain (4) · field (26) · subfield (150) · topic (149) | 6.61% · 5.10% · 2.83% · **1.64%** | | | |

**The tree tightens with depth, which is the property the layer is declared for**: a domain is a
quarter of the map and a topic is 1.6% of it, so the levels a client is served at zoom 11–16 are the
ones that draw. Against the withdrawn taxonomies of rungs 1 and 2 (medians 13.7% and 9.6%) even the
domain level is compact. ⊘ These are the 1,000,000-row sample's figures; the whole corpus's are
below and are the ones that decide.

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv and MedCPT rungs; `requirements.txt` points at the arXiv rung's. `drive.py` is the one
exception and runs on the system `python3`: it needs `requests` and `pyarrow` and nothing the
rung's environment carries.
