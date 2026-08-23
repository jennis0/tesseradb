# The Tessera test corpus

**One 10⁹-point corpus whose prefixes are smaller corpora**, plus seven
label sets over it. Built 2026-07-27. This document describes what the
data is, how it was made, and how to use it without drawing wrong
conclusions. Measurements taken against it are in `results.md`.

---

## 1. Provenance

Two real sources, joined:

| | |
|---|---|
| **Geometry / text** | [AliMaatouk/arXiv-Topics-Embeddings](https://huggingface.co/datasets/AliMaatouk/arXiv-Topics-Embeddings) — 2,422,486 arXiv papers, one BGE-large-en-v1.5 topic embedding each (4.4 GB parquet) |
| **Metadata** | Kaggle `Cornell-University/arxiv` snapshot v296 — 3,113,330 records: id, categories, authors, versions (5.1 GB JSON, anonymous `kagglehub` download works) |

The Kaggle snapshot was chosen over the ML-only alternative because that
filter collapses the category skew the design depends on.

**Join probe verdict: clean.** All 2,422,486 embedding IDs match a
snapshot `id` verbatim — no normalisation needed in either direction.
Both sources use the bare-archive form for old-style IDs
(`math/9906001`); the feared `math.GT/…` subject-class mismatch does not
exist, and every ID in both sources parses as exactly one of
new-4-digit, new-5-digit or old-style, with zero residue.

Four caveats that bound what "real corpus" means here:

1. The snapshot has **78 duplicated `id`s** (88 surplus rows, nearly all
   `math-ph/…`); deduplicated on latest `update_date` in the build.
2. **Embedding coverage stops mid-September 2024** (2024-08 complete,
   2024-09 41% missing, 2024-10 onward absent). The corpus is the
   *joined set*, not "arXiv".
3. A background **3–6% of snapshot papers per year lack an embedding**
   even before the cutoff — the upstream topics dataset's own coverage,
   not missing abstracts.
4. `versions[0].created` is neither unique (40,078 rows share a
   timestamp) nor always sane (34 pre-1991 backdated rows, earliest
   1986), hence the `(created, id)` tiebreak below.

## 2. The base corpus — `data/corpus.parquet`

117 MB, 2,422,486 rows. Built once by `build_corpus.py`; **nothing
downstream reads the raw sources**.

| Column | Meaning |
|---|---|
| `entity_id` | u32, dense, 0-based — assigned in `(v1_created, id)` order. The permanent entity space (I9: append-only, never reused) |
| `id` | arXiv id |
| `categories` | space-separated category string as supplied |
| `surnames` | author surnames from `authors_parsed` (max 2,832 on one paper) |
| `v1_created` | parsed `versions[0].created` |

Deterministic from (snapshot version, embeddings snapshot) alone — no
seed. Dedup, join and entity-ID assignment all happen here, once.

## 3. Geometry — `data/geometry.parquet` (+ `.sha256`)

64 MB. **The one hashed artifact**, sha256 `4a59a9b8…`.

Pipeline: L2-normalise the BGE embeddings (they arrive unnormalised at
‖x‖≈49.5, and the model is cosine-conventional) → exact covariance PCA
to 64 components (82.2% variance) → cuML UMAP on the GPU (~70 s on a
3080) → quantise to the 2¹⁶×2¹⁶ grid → Morton codes → row rank with
entity ID as the intra-cell tiebreak.

PCA is not a preference: 2.42M × 1024 float32 is 9.9 GB, which fills a
10 GB card before UMAP allocates its k-NN graph. cuML's `nn_descent` is
**not bit-reproducible even under a fixed `random_state`** — which is
exactly why geometry is hashed rather than seeded. Build once, record
the hash, reuse the artifact.

## 4. The scaled corpus — `data/scaled/`

### 4.1 One artifact, four corpora

Entity IDs are append-only, so **a prefix of entity space is a whole,
coherent corpus** — the relationship the design already gives ingest
(§5.1, I9). Select a scale by filtering `entity_id < limit`:

| Scale | Rows | What it is |
|---|---|---|
| 250,000 | 250,000 | prefix of the real arXiv data (oldest by `v1_created`) |
| 2,422,486 | 2,422,486 | **the real arXiv data entire** — replica 0, Morton codes bit-identical to the hashed artifact (verified) |
| 250,000,000 | 250,000,000 | + transformed replicas |
| 1,000,000,000 | 1,000,000,000 | + transformed replicas — the design's target |

`scales.json` records the limits and row counts.

### 4.2 Row space per scale, at no storage cost

`geometry.parquet` (6.6 GB) is sorted by `(morton, entity_id)`. Filtering
preserves relative order, so the rows surviving a prefix filter are
**already** in that sub-corpus's Morton rank order — its `row_id` is the
running position:

```python
g = pq.read_table("data/scaled/geometry.parquet", columns=["entity_id", "morton"])
keep = g["entity_id"].to_numpy() < limit    # the scale's rows
row_id = np.arange(keep.sum())              # its Morton ranks
```

Verified against an independent re-sort at 250k and 2.4M. The stored
`row_id` column is the *full-corpus* rank; smaller scales derive theirs
as above. This is §5.1's per-view row assignment with scale standing in
for view.

Columns: `entity_id`, `morton`, `row_id`, `priority` (u16, splitmix hash
of entity ID — mask-independent per §7.2).

### 4.3 Replicas are transforms, not copies

Naive tiling would make the corpus **easier** than the base: identical
copies give every term N identical posting blocks and every tile the
same occupancy, so mask structure and spatial structure both go
degenerate. Each replica is instead an affine transform (rotate,
reflect, log-uniform scale 0.02–0.55, translate) plus jitter, with 45%
placed on shared hubs so some regions overlap heavily while others stay
isolated.

Replicas 0–4 pin deliberate edge cases:

| # | What | Exercises |
|---|---|---|
| 0 | identity, real coordinates | the hashed artifact unchanged |
| 1 | extreme compression (s=3e-4) | Morton collisions, hot tiles, intra-leaf priority tiebreak under load |
| 2 | pinned to grid corner (0,0) | quantisation clamp low |
| 3 | pinned to corner (max,max) | quantisation clamp high |
| 4 | degenerate line (y collapsed) | a tile one cell tall |

Resulting structure — occupancy spans **four to five orders of magnitude
at every depth**, which is what exercises the direct-evaluation /
candidate-list crossover across its whole range:

| Depth | Tiles | Occupied | Median | p99 | Max |
|---|---|---|---|---|---|
| 4 | 256 | 256 | 2,944,013 | 18,088,140 | 24,416,318 |
| 6 | 4,096 | 4,096 | 123,587 | 1,948,670 | 7,787,046 |
| 8 | 65,536 | 65,481 | 6,660 | 138,743 | 4,584,942 |
| 10 | 1,048,576 | 1,044,000 | 393 | 9,135 | 4,034,398 |
| 12 | 16,777,216 | 16,279,220 | 25 | 561 | 3,910,432 |

64.7% of points occupy a distinct Morton cell; one cell holds ~3.88M
(the compression replica). u32 row IDs are 23.3% consumed, and at 0.23
points per grid cell the arithmetic matches §5.2's own 10⁹ figures.

### 4.4 Label sets — `data/scaled/pairs/`

Each is an exploded `(entity_id, term_id)` relation. Geometry is
label-independent, so all configs share one `geometry.parquet`.

| Config | Size | Pairs | Terms | Structure | Stresses |
|---|---|---|---|---|---|
| `categories-subclass` | 1.64 GB | 1.72B | 47,968 | 60 global + 413×116 local | realistic baseline, mixed diffuse/clustered |
| `categories-archive` | 0.85 GB | 1.38B | 15,694 | all replica-local | maximal entity-space contiguity |
| `hash-flat` | 2.39 GB | 1.00B | 10,000 | all global | orthogonal control, maximal scatter |
| `surnames` | 9.80 GB | 4.37B | **116,946,544** | 121k global + 413×282,870 local | dictionary scale, near-unique signatures, real Zipf |
| `hiterms` | 1.76 GB | 1.30B | 1,000,000 | independent draws | per-item breadth (10–1000 terms/item) |
| `hiterms-ov0.5` | 2.53 GB | 1.38B | 1,000,000 | 50% from profile band | as above + moderate signature groups |
| `hiterms-ov0.9` | 2.71 GB | 1.25B | 1,000,000 | 90% from profile band | as above + strong signature groups |

**Global vs replica-local** is the key structural knob. A *global* term
spans every replica: huge, spatially diffuse postings scattered across
the whole entity space. A *replica-local* term lives in one replica's
contiguous entity block: smaller postings that encode as runs. This
choice is worth up to ~130× on mask-build cost (see `results.md`), so it
is not cosmetic.

Each config has a sidecar `.json` manifest with its parameters.

### 4.5 Two knobs the files themselves do not encode

**The surnames fold** varies dictionary scale while holding pairs,
coverage and spatial footprint exactly constant — F consecutive replicas
share one local vocabulary. Applied at read time as arithmetic on
`term_id`:

| Fold F | Local blocks | Distinct terms |
|---|---|---|
| 1 | 413 | 116,902,007 |
| 2 | 207 | 58,675,736 |
| 3 | 138 | 39,157,568 |
| 12 | 35 | 10,021,752 |

```python
loc = tid >= n_g                      # n_g, n_l from surnames.json
idx = tid[loc] - n_g
tid[loc] = n_g + (idx // n_l // F) * n_l + (idx % n_l)
```

**The hiterms entity cap.** At 10–1000 terms/item the pair relation is
the binding cost — 10⁹ items would be ~2×10¹¹ pairs (~1.7 TB) — so these
configs cover `entity_id < 10,000,000` only. The 250k and 2.4M scales
are complete; **250M and 1B are not reached**. The manifest records the
limit; a probe must respect it or it will silently measure a corpus with
a hole in it.

## 5. How to use it properly

1. **Pick a scale by filtering `entity_id < limit`** — never by taking
   the first N rows of `geometry.parquet`, which is in Morton order, not
   entity order.
2. **Derive `row_id` per scale** as the running position after the
   filter (§4.2). The stored `row_id` is full-corpus rank and is wrong
   for any smaller scale.
3. **Check the config's entity cap** before choosing a scale — `hiterms`
   stops at 10⁷.
4. **Pairs are sorted by `(term_id, entity_id)` within row groups** with
   `DELTA_BINARY_PACKED` encoding: ~3.8× smaller and ~3× faster to read
   than unsorted. Streaming consumers can exploit the order and skip a
   per-batch sort.
5. **Do not hold a bitmap per term for `surnames`** — 117M terms is
   ~29 GB of Python and container overhead before storing a posting.
   Follow §6.2's shape: sorted int32 arrays (CSR), or materialise only
   the granted terms, which is what the serving path does anyway.

## 6. Regenerating

Deterministic given the sources and seeds; geometry excepted (see §3).

```bash
# base corpus
python probes/build_corpus.py <snapshot.json> data/embed_paper_ids.parquet data/corpus.parquet
# geometry (GPU; hash will differ — record the new one)
python probes/build_geometry.py data/corpus.parquet data/arxiv_papers_embeds.parquet data/geometry.parquet
# base-scale label sets
python probes/gen_pairs.py data/corpus.parquet data/pairs --config categories --level subclass
#   …also: --level archive | --config surnames | --config hash … | --config noise --epsilon E
# the 10^9 corpus (geometry once, then pairs per config)
python probes/build_scaled_corpus.py data/geometry.parquet <any-pairs> data/scaled --skip-pairs
python probes/build_scaled_corpus.py data/geometry.parquet data/pairs/surnames.pairs.parquet \
       data/scaled --pairs-only surnames --global-frac 0.3
# high terms-per-item
python probes/gen_hiterms.py data/scaled --max-entity 10000000 --vocab 1000000 \
       --profiles 1000 --overlap 0.9
```

Build cost: geometry ~2.5 min, each label config 30 s to 3.5 min,
`hiterms` ~10 min per variant. Whole corpus ~27 GB.

## 7. What this data can and cannot tell you

**Can**: stress the machinery — Morton sort at 10⁹, tile tables, range
arithmetic, u32 limits, hot cells, mask build across vocabulary and
posting shapes, dictionary scale to 10⁸ terms, per-item breadth to 1000.

**Cannot**: say anything new about how *real* access-control labels
cluster. Replicas repeat the base's topic geometry under transformation,
so spatial-autocorrelation conclusions belong to the 2.4M real prefix
(and, ultimately, to the real-label rerun). The label configs are
synthetic policies chosen to bracket distributional shapes, not to
predict any deployment's.

Timings taken on this data are WSL2, single-threaded Python driving
CRoaring: **treat ratios as evidence and absolutes as indicative**.
