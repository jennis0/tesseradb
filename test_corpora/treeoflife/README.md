# TreeOfLife-200M — the ladder's largest rung, and its first with two geometries over one entity space

**233,055,986 specimen photographs**, each with a 768-dimensional BioCLIP-2 embedding a third party
computed and released on Hugging Face, carrying seven taxonomic ranks, a scientific name, a common
name and — the rung's compartment — **a publishing institution**. 2.3× rung 4's rows at three
quarters of the width, **two views of one entity space**, and the ladder's first seven-level tiered
layer.

It is a **demonstrator and a speed benchmark** (owner rulings 2026-09-01 and 2026-09-02: this
corpus tests Tessera's speed and memory, not the UMAP pipeline; layout quality matters only as far
as the demo looks good). Recall against an exact neighbour search is not measured and layout
fidelity is not judged.

**Three things make this rung different from the four below it.**

1. **Two views over one entity space.** `bioclip` is the embedding layout and holds every row;
   `geo` is Web Mercator over the GBIF-joined rows and holds 75.90% of them. The arXiv rung had two
   views of one embedding and the geographic rungs had one geography each; nothing on the ladder
   has had an embedding *and* a geography over the same entities, or a view holding a subset of the
   corpus.
2. **A seven-level tiered layer over every row**, which is the largest tiered membership the ladder
   has carried and is drawn on both views.
3. **No prose.** The search surface is names — an indexed `scientific_name` keyword and an indexed
   `common_name` text column. Rung 4 measured what 118.9 GB of abstracts do to a build; this rung
   measures what 2.33×10⁸ rows do to one without them.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# the join track's two passes over the share — see README-join.md
~/venvs/projection/bin/python -m test_corpora.treeoflife.stage --scan --combine
~/venvs/projection/bin/python -m test_corpora.treeoflife.gbif_join

# the vectors track: the fit sample, then the corpus
~/venvs/projection/bin/python -m test_corpora.treeoflife.stage --fit
~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --layout-only
~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --reuse-layout

cd "$TESSERA_LADDER/treeoflife" && tessera check --payloads && tessera build --stage-timings
```

## Two tracks, one package

`sources.py`, `stage.py`, `gbif_join.py` and [`README-join.md`](README-join.md) are the **join**
track's: the one pass over the 666 source files for every column but `emb`, and the scan of GBIF's
8,369 occurrence parts that gives the `geo` view its coordinates. `routes.py`, `prepare.py`,
`spread.py`, `drive.py`, `corpus.toml` and this file are the **vectors** track's. The interface
between them is `staging/metadata.parquet` and `staging/gbif-coordinates.parquet`, and
`entity_id` is the global row index in file order on both sides.

## The vectors are never staged whole, and that is a disk fact

346 GB of `float16` against **217 GB free** on this box. Rung 4 staged its 209 GB matrix and placed
against a local memmap; here the layout is fitted on a sample staged locally and every row is
placed in a **second pass that reads the share and keeps nothing but a position**.

**The fit sample is uniform over all 666 files, because the corpus is sorted by taxonomy.** A
prefix of the files is a prefix of the tree of life. Each file contributes its equal share, drawn
from **one of its seven row groups**, the group index rotating with the file (`i % 7`).

⊘ **That is one seventh of a pass rather than a whole one, and it is a deliberate trade.** A truly
uniform draw within a file would touch nearly every data page of that file's `emb` column — the
column is 73 MB a row group and a scattered 3,754-row take decodes almost all of it — so a whole
pass would cost hours to sample what a rotating single group samples in half of one. What is given
up is intra-file uniformity, over a file spanning a very narrow taxonomic range.

## The compartment is real, and every row carries a term

`publisher` — the institution that published the occurrence record. **Owner ruling, 2026-09-03:** a
row with no publisher carries `unpublished`, a key of the same closed vocabulary, rather than no
term and the view's `public` default. So the access column is never empty, `point_visibility`'s
`default` never fires, and **a principal holding no term sees nothing**. Written this way because
the campaign's principal ladder starts at 1% of the corpus and cannot be composed under a floor
every principal holds for free. Rung 4's `unlicensed` and rung 3's `unindexed` have the same shape
and exist for the same reason.

⊘ The interface document fixed on 2026-09-03 said `default = "public"`; the ruling later that day
replaced it, and the declaration follows the ruling.

## The taxonomy layer's keys

A member row is one specimen with a seven-entry list whose positions are the declared levels —
GeoNames' `members-admin.parquet` form. Level *j*'s key is the first *j* + 1 ranks joined with `|`,
so a species key names its whole lineage: `species` in this source is the epithet (`lotor`, not
`Procyon lotor`), and a bare epithet is not unique across the tree.

**A hole in the chain becomes an artifact, not a null.** A row with a family and no order carries
an explicit `NOT_RECORDED` key at the order level, because `parent_edges` is `windows(2)` and does
not read past a gap; a null there would state a containment no row makes. A level is filled only
where something *below* it is present, so a chain that simply ends keeps its remaining levels null.
Both markers are checked against the corpus's own values and a collision is a refusal: a
placeholder that merged with a real clade would move specimens between artifacts with no error.

## Ruled 2026-09-03 — no contiguity experiment

The plan's "ingest in taxonomy order and shuffled" is withdrawn.
[Decision 0073](../../docs/decisions/0073-entity-ties-are-ordered-by-morton-code.md) orders entity
ids within a signature group by Morton code, so input order changes nothing and the two arms would
differ only by the cost of shuffling the input. The rung is built once, with `publisher` as the
compartment.

## Measured

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The two staging passes are the **network-source** figures.

<!-- measured -->

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv, MedCPT and PaperSeek rungs; `requirements.txt` points at the arXiv rung's. `drive.py` is
the one exception and runs on the system `python3`: it needs `requests` and `pyarrow` and nothing
the rung's environment carries.
