# MedCPT / PubMed — the ladder's largest embedding rung

**35,920,666 PubMed articles**, each with a 768-dimensional MedCPT embedding the NCBI published
alongside the article text. Fifteen times the arXiv rung's rows, one view, and the layer the rung
exists for: the **MeSH descriptor DAG** — 30,954 concepts, 42,287 edges, a membership closed upward
through it, and 1.66×10⁹ member rows to show for it.

It is a **demonstrator and a speed benchmark** (owner rulings 2026-09-01 and 2026-09-02: this
corpus tests Tessera's speed and memory, not the UMAP pipeline; layout quality matters only as far
as the demo looks good). Recall against an exact neighbour search is not measured and layout
fidelity is not judged. Every figure below *is* a claim about what this pipeline and `tessera
build` cost, and each names its medium.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# once: 163 GB off the share, resumable per chunk — the only pass over the publisher's bytes
~/venvs/projection/bin/python -m test_corpora.medcpt.stage

# the corpus; --sample 0 takes all 35,920,666
~/venvs/projection/bin/python -m test_corpora.medcpt.prepare --sample 1000000

cd "$TESSERA_LADDER/medcpt" && tessera check --payloads && tessera build
```

## One view, and why it is not called a topic map

MedCPT's article encoder was trained on 255 million query-article click pairs, for **retrieval**.
The publisher says so and the acquisition README repeats it: the geometry is organised for search
relevance, which is not topical similarity. So the view is `knn`, titled **Literature map**, and
nothing here describes it as a map of topics.

`projection = "none"` and `extent = "auto"`: an embedding layout is not a map, and there is no
transform between these coordinates and any ground.

## The route — fit on what the card holds, place the rest

The arXiv rung puts the whole fp16 matrix on the card, builds one CAGRA index over it and hands
cuML's UMAP the graph. Here the matrix is 35,920,666 × 768 float16 — **55 GB** — which fits neither
a 10 GB card nor a 47 GB box.

**Measured first, on this box** (RTX 3080, 10 GB, ~8.2 GB free; vectors resident in host RAM;
2026-09-02):

| rows | CAGRA build | search | graph peak VRAM | self-first | UMAP | layout peak VRAM |
|---|---|---|---|---|---|---|
| 2,000,000 | 25.6 s | 77,019 /s | 4.68 GB | 99.15% | 34.0 s | 2.64 GB — **1,417 B/row** |
| 2,500,000 | 21.6 s | 87,674 /s | 5.46 GB | 98.95% | 34.7 s | 2.99 GB — **1,283 B/row** |

At 1,283 bytes a row the whole corpus is **~46 GB of device memory** for the layout alone — six
times the card, and past the 47 GB of host RAM that RMM's managed memory would have to
oversubscribe into. ⊘ **A managed-memory run was therefore not attempted**; the number says it
cannot fit, not that it was slow.

So the route is the brief's third option, taken directly:

1. **Fit** UMAP on a uniform sample of **2,500,000** rows, through one CAGRA index over its own kNN
   graph. 2,500,000 is the largest size *measured* rather than the largest extrapolated: the
   binding constraint is the index (5.46 GB) and not the layout (2.99 GB), and an OOM two thirds of
   the way through a 36M run costs an hour.
2. **Place** every other row at the **similarity-weighted mean of its 15 fit-set neighbours'
   positions**, searched against an index over the same fit set. Every row goes through this path,
   fit rows included, and the fit rows are then overwritten with their own UMAP positions: it costs
   7% more searches and buys contiguous reads off a 55 GB memmap where skipping them would make
   every batch a gather.

⊘ **The index is built twice — once for the graph, once for the placement**, 18 s each at 2,500,000
rows, because it cannot be held across the layout: 5.46 GB for the index and 2.99 GB for the layout
is 8.45 GB against ~8.2 GB free. Hoisting it would need a larger card or a smaller fit set.

**Sharded CAGRA over all 36M is retained in `knn_graph` and is not used.** It would be fifteen
indexes × 36M queries where this is one index and one pass. ⊘ Nothing measured it at scale — do not
quote it.

⊘ **The layout is not reproducible under a seed.** `random_state` is fixed and CAGRA's index build
takes none, so the graph UMAP is handed differs run to run — the arXiv rung's ⊘, unchanged.

## The access column

`branches` — the MeSH top-level branch letters an article's resolved descriptors sit under, `A`–`N`
plus `V` and `Z`, sixteen published categories standing in for a compartment scheme the source does
not carry. The same synthetic-policy-over-real-data shape every rung of the ladder uses.

**An article with no resolved descriptor carries the single term `unindexed`**, so the column is
never empty and `point_visibility`'s `default` never fires. The default is declared because the
field requires one, not because it is expected to be reached.

⊘ **`unindexed` is not a scatter.** MeSH indexing lags publication and the chunks are in PMID
order, so it is concentrated at the recent end of the corpus — 100% of chunk 0 is indexed against
37.5% of chunk 37. A principal granted every branch letter but not `unindexed` sees the old
literature and not the new, and that is a property of the source rather than of the policy.

⊘ **Unresolved descriptor mentions are dropped, and the drop is not random.** Measured over the
whole corpus: 297,844,931 mentions resolved, **19,910,463 (6.3%) not**, across 2,875 distinct
headings the NLM has retired or renamed since — weighted towards the ancestry and ethnicity terms
revised in 2022–23. Ruled 2026-09-01: dropped, and said so. Every coverage figure here carries it.

## Abstracts: an open owner ruling

⊘ **Not decided.** 36M × ~1 kB is ~30 GB of strings in an attribute pass that holds a text column
whole, and the streaming text column does not exist. `--abstracts` takes them; the default is off.
The 1,000,000-row sample was built **both ways** so the ruling can be made from numbers — local
NVMe, box otherwise idle, 2026-09-02:

| | `--abstracts` off | on | ×36 (linear, **modelled**) |
|---|---|---|---|
| `points.parquet` | 118.6 MB | 634.6 MB | 4.3 GB → 22.8 GB |
| prepare's *write points* step | 0.7 s | 115.6 s | — |
| prepare peak RSS | 15.6 GB | 16.2 GB | — |
| `tessera build` wall | 19.7 s | 34.3 s | — |
| **`tessera build` peak RSS** | **716 MB** | **2,246 MB** | 25.7 GB → **80.7 GB** |
| bundle on disk | 333 MB | 799 MB | 12.0 GB → 28.7 GB |

Abstract coverage is 689,132 of 1,000,000 (68.9%), which is also the whole-corpus figure (68.9%).
⊘ **The two prepare runs were not equally loaded** — the `off` run shared the box with a demo server
and a `verify`, which cost its MeSH step 222 s against the `on` run's 45 s — so read the *prepare*
rows as indicative. The `tessera build` and bundle rows are the ones the ruling turns on and both
builds ran alone.
The last column is a **linear extrapolation and not a measurement**: the build's peak is known not
to be bounded by `--memory-budget` (the campaign's W2), so the 80.7 GB is what to expect to meet
rather than a prediction of a graceful refusal on a 47 GB box.

## Measured

### Staging — one pass off the share

**2026-09-02**, SMB at ~67 MB/s (a **network-source** figure, not comparable with the local-NVMe
ones below): **60.5 minutes** for all 38 chunks, 15.0 GB peak RSS, writing 67 GB locally —
55.2 GB of `vectors.f16` and ~12 GB of per-chunk parquet.

| | |
|---|---|
| rows | 35,920,666 — counted from the 38 `.npy` headers |
| with a MeSH field | **84.9%** |
| with an abstract | **68.9%** |
| unparseable dates | 27,957 (0.078%) — written null and counted, never a refusal |
| zero-norm vectors | 0 |

### The 1,000,000-row sample

Local NVMe, RTX 3080. `prepare.py` **349 s** at **15.61 GB** peak RSS: route 73 s, staged columns
43 s, the MeSH resolve/closure/member write 222 s over 46,178,538 closed pairs, k-means 0.5 s,
titles 9 s. ⊘ **That MeSH figure is contended** — a demo server and a `verify` were running beside
it; the same step on an idle box in the `--abstracts` run below was 45 s over the same rows. 43
k-means cells (18 … 51,243 members, median 27,693), 43 of 43 with a distinctive title out of 39,906
candidate terms.

`tessera build` **19.7 s** to a **333 MB** bundle at **716 MB** peak RSS; `verify --deep` clean at
1,000,000 rows and 4,601,362 pairs; **no containment violation** over 29,229 descriptors and 40,075
edges, 9,831 splits of which 9,362 are non-covering.

Served through `run_demo.sh` on its own deployment, the principals ladder is real: 136 / 136 /
167,479 / 706,252 / 1,000,000 visible across narrow, sparse, medium, heavy and full.

### The whole corpus — 35,920,666 rows

Local NVMe, RTX 3080, box otherwise idle, 2026-09-02. `prepare.py --sample 0` **17 m 27 s** at
**43.3 GB peak RSS** — on a 47 GB box, which is the headroom this rung has and not a comfortable
one.

| step | | |
|---|---|---|
| route `knn` | **480 s** | gather the 2.5M fit set off the memmap 40 s · CAGRA build 18.1 s · graph search 22.1 s (111,872 q/s) · UMAP 36.6 s · **place 35,920,666 rows 361 s** |
| staged columns | 48 s | 38 chunk parquets, base columns only |
| MeSH | **421 s** | resolve, close and stream, a million rows at a time |
| k-means | 22 s | cuML over the 36M × 2 layout, k = 256 |
| titles | 25 s | 89,747 candidate terms over a 4,000,000-title sample; 253 of 256 cells titled |
| write points | 25 s | 4.02 GB of parquet |

99.07% of the fit set's rows came back with themselves first; the other 0.93% were repaired.
256 k-means cells hold 2 … 393,741 articles (median 167,041).

**MeSH, over the whole corpus:**

| | |
|---|---|
| articles with a resolved descriptor | 30,504,767 (**84.9%**) |
| resolved mentions | 297,844,931 · major-topic 103,662,402 |
| unresolved mentions | 19,910,463 (**6.3%**) over 2,875 distinct retired headings — dropped, ⊘ non-randomly |
| **closed member rows** | **1,658,437,807** — 46.2 an article, 54.4 an *indexed* article |
| descriptors with members | 30,217 of 30,954 · 41,321 edges · 9,095 with more than one parent · 107 roots · at most 6 |
| `unindexed` | 5,415,899 articles (15.1%) |
| (article, branch) labels | 165,272,740 over 17 terms |

**`tessera build`: 12 m 10 s, 16.03 GB peak RSS, an 11.15 GB bundle** over 35,920,666 items,
18 terms and 165,272,740 pairs. `verify --deep` clean in 5.1 s at 1.15 GB: 1 partition, 1 view,
1 segment, 35,920,666 rows. The build's own report, verbatim:

```
view 'knn': quantising against x [-17.894932670593263, 20.183564109802248], y [-20.086218280792238, 17.992278499603273]
        the data spans x [-17.52161407470703, 19.810245513916016], y [-17.711769104003906, 15.617829322814941] — 64252 x 57364 of the 65536 x 65536 cells
        35920666 point(s) placed, none on the frame's edge
attribute 'published': 35,892,709 of 35,920,666 entities have a value
attribute 'title': 35,887,816 of 35,920,666 entities have a value
attribute 'mesh_major': 30,351,241 of 35,920,666 entities have a value
attribute 'pmid': 35,920,666 of 35,920,666 entities have a value
artifact layouts, chosen from the bundle's own row space (130447 ms):
  clusters/kmeans level 0 [knn]: 256 artifact(s) with rows, 0.180 everywhere, 7.3 blocks/artifact, disjoint — served rows
  mesh/descriptors level 0 [knn]: 30217 artifact(s) with rows, 0.984 everywhere, 241.4 blocks/artifact, overlapping — served list
view 'knn': 35920666 point(s) landed in 32550278 distinct cell(s) — 90.6% of them have a position of their own
built .../bundle (v00000): 35920666 items, 18 terms, 165272740 pairs, 11150611895 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
```

Served through `run_demo.sh` on its own deployment, the principals ladder is 4,910 / 4,910 /
6,024,843 / 25,357,425 / 35,920,666 visible.

⊘ **The bundle's manifest was corrected by hand.** The run wrote
`umap.graph = "cagra fp16, sharded"` and no fit size — the name of a path `knn_graph` can take and
this rung does not. `prepare.py` writes the route it ran from `65796880` onward; the
`$TESSERA_LADDER/medcpt/manifest.json` beside the built bundle was patched rather than regenerated,
and says so in a `corrected_by_hand` field.

### The rung's scaling finding

**1.66×10⁹ closed membership entries against rung 2's 5.07×10⁸ — 3.27×**, and
`dag-hierarchies.md` §8 predicted 3.4× from chunk 18 alone. It fits: 2.75 GB of member parquet into
an 11.15 GB bundle, built in twelve minutes at 16 GB peak. **Neither of the campaign's first two
walls fired** — no artifact is large enough to meet W1's 5×10⁷-member Roaring round trip, a closed
MeSH root being bounded by the 3.05×10⁷ indexed articles, and W2's OOM did not happen at 16 GB on a
47 GB box. So the fallback of design §8 — explicit assignments with the containment report beside
every figure — is not needed and was not taken.

### Layer spread — both layers draw

The box holding the middle 90% of an artifact's members, as a share of the map (the arXiv rung's
measure). k-means exactly, over all 256; MeSH over a uniform sample of 150 descriptors, 148 of
which hold ten members or more (median 2,691):

| | median | p90 | max | under 5% |
|---|---|---|---|---|
| `clusters/kmeans` | **0.04%** | 0.15% | 0.83% | 100% |
| `mesh/descriptors` | **1.4%** | 5.1% | 7.9% | 88% |

Against the withdrawn taxonomies of rungs 1 and 2 (medians 13.7% and 9.6%) both are compact, and
neither is withdrawn here. ⊘ **That is not the same statement as the build's `everywhere` fraction**,
which is 0.984 for the DAG: a box covering 1.4% of the map is still wider than the tile-index node
at the depth the level is served from, so 98.4% of the descriptors are served as a list rather than
bounded by a node. The two numbers measure different things and both are above.

### ⊘ 45 member rows landed under the wrong article, and the build named every one

`containment.json` on the first whole-corpus build names **56 edges whose child holds a member its
parent does not — 45 rows of 1,658,437,807**, which is 3×10⁻⁸. Chased rather than waved through,
and what it is *not* is as measured as what it is.

**The shape.** Every affected row lies in the 35-wide window of **consecutive** entities
18,662,757 … 18,662,791, and the error is structured: each of those articles lost its
**highest-id — alphabetically last — descriptors to the next article along**, with the row totals
preserved. It is a set of row boundaries displaced by a few positions, not values overwritten at
random.

**What is sound.** The closure carries the parent for all 56 edges. `weather` holds exactly the
18,845 members a recomputation gives. The `branches` column is correct over the same rows — it is
computed from the explicit descriptors and not from the closure, which localises the fault to
`closure` → `_list` → `write_layer` and nothing before it. The window sits on no chunk, `MESH_SLICE`
or `BATCH` boundary.

**No code path accounts for it.** Five candidates, each excluded:

| | |
|---|---|
| the composite key's bit width | `_clo_flat` holds ids 0 … 30,953 against a 15-bit field's 32,767, so the id cannot carry into the row field |
| an int32 key | the key is int64; a truncation would first bite at within-batch row 65,536, and the affected rows are 62,757 … 62,791 — a near miss, and a miss |
| an unstable or partial sort | the sort is over the composite key, which is injective on `(row, id)`, so there are no ties to reorder |
| an off-by-one at a row boundary | the right shape, and recomputing the slice agrees exactly with an independent set-union reference over the window and 150 rows of margin, twice, bit for bit |
| an int32 in the offsets | real, and 37× away: the largest slice expanded to 5.8×10⁷ entries and the offset at the affected row was 3.3×10⁷ against 2.147×10⁹ |

**A second whole-corpus run does not reproduce it.** Same code, same staged input: the second run
differs from the first in exactly those 45 rows, and its window matches an independent recompute
(49,279 rows, none wrong, none missing). The two runs also disagree by **three rows** in the total
MeSH membership they wrote — 1,658,437,807 against 1,658,437,804 — over an input and a code path
that are deterministic in process, which is a second symptom of the same kind rather than a
consequence of the first.

⊘ **Attributed to a memory fault on this host, and that is not proven.** It is the shape of one — a
run of an int32 offsets buffer displaced while the values beside it are untouched, in a run peaking
at 43.3 GB on a 47 GB box — and this machine has a standing memory-fault suspicion from three
corruption-class symptoms on 2026-08-22/23. What rules the alternative *in* is that a logic error
would move counts or shift a whole slice, and would recur. What has not been done is a memtest, so
the attribution stands as the best fit and not as a finding. ⊘ The second run's own `tessera build`
then **died of `SIGSEGV`** after 3 m 12 s at 5.3 GB (kernel: `error 6`, a write to a non-present
page) where the first completed in 12 m 10 s at 16.03 GB — a third symptom, and the reason the
memtest is now the thing to do before any of this is called a bug.

**Two things caught it, and one of them is new.** `tessera build`'s containment report named all 45
rows individually, by parent and child, without being asked — after 1.66×10⁹ rows had been written.
`prepare.py` now refuses per slice, before a single member row reaches the file, on the property
that cannot fail on sound data: a row's closure contains the descriptors it was closed over. That
check costs 19 s over the whole corpus against the MeSH step's 421 s. ⊘ Neither whole-corpus run
above was produced with it — it was written after both.

## Two things the scale broke, and what they cost

**`ChunkedArray.take` concatenates the whole column before it takes anything.** 35,920,666 titles
are ~3 GB of characters, past the 2 GiB an Arrow `string` array's 32-bit offsets can address, so
drawing a 4,000,000-row sample of them failed with `offset overflow while concatenating arrays` —
on the column, with nothing to do with the sample's size. `take_strings` walks the chunks;
`mesh_major` and the access column are left chunked for the same reason. It surfaced only at full
scale, after the route and the MeSH pass had both completed.

**The MeSH closure cannot be held.** 1.66×10⁹ pairs is 14 GB of `int64` before anything is written,
so `prepare.py` resolves, closes and writes a million rows at a time and only the access column and
the joined major-topic names survive the loop; `mesh.write_layer` streams its member rows straight
to parquet and re-declares its artifacts on each call.

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv rung; `requirements.txt` points at that rung's. Nothing here needs a package it does not
already have.
