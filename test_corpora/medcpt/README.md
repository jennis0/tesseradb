# MedCPT / PubMed — the ladder's largest embedding rung

**35,920,666 PubMed articles**, each with a 768-dimensional MedCPT embedding the NCBI published
alongside the article text. Fifteen times the arXiv rung's rows, one view, and the layer the rung
exists for: the **MeSH descriptor DAG**, 30,954 concepts with 42,287 edges and a membership closed
upward through it.

It is a **demonstrator and a speed benchmark** (owner ruling, 2026-09-01: speed wins over
accuracy). Recall against an exact neighbour search is not measured, layout fidelity is not judged,
and no figure here is a claim about UMAP. Every figure *is* a claim about what this pipeline and
`tessera build` cost, and each names its medium.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# once: 163 GB off the share, resumable per chunk
~/venvs/projection/bin/python -m test_corpora.medcpt.stage

# the corpus
~/venvs/projection/bin/python -m test_corpora.medcpt.prepare --sample 1000000

cd "$TESSERA_LADDER/medcpt" && tessera check --payloads && tessera build
```

## One view, and why it is not called a topic map

MedCPT's article encoder was trained on 255 million query-article click pairs, for **retrieval**.
The publisher says so, and the acquisition README repeats it: the geometry is organised for search
relevance, which is not topical similarity. So the view is `knn`, titled **Literature map**, and
nothing here describes it as a map of topics.

`projection = "none"`, `extent = "auto"`: an embedding layout is not a map, and there is no
transform between these coordinates and any ground.

## The route: fit on what the card holds, place the rest

<!-- MEASUREMENTS -->

## The access column

`branches` — the MeSH top-level branch letters an article's resolved descriptors sit under, `A`
through `N` plus `V` and `Z`, sixteen published categories standing in for a compartment scheme the
source does not carry. It is the same synthetic-policy-over-real-data shape every rung uses.

**An article with no resolved descriptor carries the single term `unindexed`**, so the column is
never empty and `point_visibility`'s `default` never fires. The default is declared because the
field requires one, not because it is expected to be reached.

⊘ **`unindexed` is not a scatter.** MeSH indexing lags publication and the chunks are in PMID
order, so it is concentrated at the recent end of the corpus — 100% of chunk 0 is indexed against
37.5% of chunk 37 (`../../docs/ingest-campaign.md` §4.4). A principal granted every branch letter
but not `unindexed` sees the old literature and not the new one, and that is a property of the
source rather than of the policy.

⊘ **5.87% of descriptor mentions do not resolve against the 2025 MeSH vintage, and the miss is not
random** — 89 retired or renamed headings carry all of it, weighted towards the ancestry and
ethnicity terms the NLM revised in 2022–23. Ruled 2026-09-01: dropped, and said so. Every coverage
figure here carries the drop.

## Abstracts: an open owner ruling

⊘ **Not decided.** 36M × ~1 kB is ~30 GB of strings in an attribute pass that holds a text column
whole, and the streaming text column does not exist. `prepare.py --abstracts` takes them and the
default is off; the numbers below are the 1M sample built both ways, which is what the ruling wants.

## Measured

<!-- MEASURED -->
