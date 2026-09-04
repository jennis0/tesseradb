# The ingest campaign — full datasets, and the three walls they will hit

**Status:** Working memo — a plan, not a design. Nothing here decides an invariant. **Owner
direction, 2026-08-27: serve whole datasets and whole artifact sets. Where Tessera cannot, that is
a defect to fix, not a scale to design around.** This memo is written on that premise: no rung is
sampled, and every wall is named with the work that removes it.

**Reads with:** 2026-08-26-dataset-ladder.md (what was acquired
and why), [`2026-08-22-artifact-scale-campaign.md`](2026-08-22-artifact-scale-campaign.md) (the
three walls, measured), ../../artifact-delivery.md (the stage this
work belongs to), [`../../design/configuration.md`](../../design/configuration.md) (the surface each
rung is declared in), and [`../../../probes/dataset.md`](../../../probes/dataset.md) (the corpus
this ladder joins).

---

## 1. What already works, so the walls are the news

**The point layer is proven at 10⁹.** `scripts/build_full.sh` builds the full
`data/scaled/geometry.parquet` with `categories-subclass` — 10⁹ points, 1.72×10⁹ pairs — into a
**~45 GB bundle**, and that bundle is the p99 measurement fixture. A billion points with a category
vocabulary is not an open question.

What has never been done is **a billion points carrying artifacts**, and the artifact scale
campaign of 2026-08-22 says exactly where it stops.

## 2. The three walls, measured

| # | Wall | Where it bites | Evidence |
|---|---|---|---|
| **W1** | **A 5×10⁷-member enumerated membership fails its own Roaring round trip.** `deserialise_members` returns `None` at the site whose comment reads *"Unreachable: the memberships were serialised from bitmaps two calls ago"* — a `Portable` bitmap failing to deserialise **in the process that wrote it**, with 21 GB of headroom. The same layer at 10⁷ builds and censuses exactly | Any whole-corpus root cluster on any rung above 5×10⁷ | campaign finding 3 |
| **W2** | **`tessera build`'s peak RSS is not bounded by `--memory-budget`.** OOM-killed at 47.3 GB auto-derived, 47.5 GB with `--memory-budget 12g`, 47.55 GB with the budget *and* the two largest member sources dropped. Three runs, one number: the peak is the machine | Every artifact-carrying build above ~5×10⁷ points | campaign finding 4 |
| **W3** | **The build reads every input while writing the bundle**, so the transient requirement is inputs + bundle. At the artifact campaign's 10⁷-measured ratios that is **267 GB at 10⁹** against 147 GB free | GBIF at 3.5×10⁹ and Overture buildings at 2.53×10⁹ | campaign "what was not run" |

**W1 and W2 are defects.** W1 is fail-closed and localised — a serialisation round trip that works
at 10⁷ and not at 5×10⁷, in one process, which is a bug with a bisection rather than a scaling law.
W2 is a flag that exists precisely to prevent this failure and does not; a build that is *killed*
rather than *refused* is the case the pre-flight was built for. Both are marked ⊘ in the campaign
and neither has an issue against it.

**W3 is partly physics and partly the same defect wearing different clothes.** A build that streams
its inputs rather than holding them does not need inputs + bundle simultaneously. The disk half is
real and is answered in §4.

## 3. What each rung costs at full scale

Sizing ratios, all measured on this repository: **45 B/point** for a bundle with a category
vocabulary (10⁹ → 45 GB, `build_full.sh`); **121 B/point** for an artifact-carrying bundle and
**146 B/point** for its inputs (both from the artifact campaign's 10⁷ tier); **10.4 B/point** for a
geometry-only points file (`data/scaled/geometry.parquet`); **107 B/point** for a title-indexed
bundle and **580–950 B/point** with abstracts (the demo bundles).

| Rung | Points | Points file | Bundle (categories) | Transient (inputs + bundle) | Verdict on 170 GB |
|---|---|---|---|---|---|
| GeoNames | 1.2×10⁷ | 0.5 GB | 1.4 GB titles+names | ~4 GB | **fits, trivially** |
| MedCPT / PubMed | 3.6×10⁷ | 1.5 GB | 3.9 GB titles | ~12 GB | **fits** |
| Overture places | 7.4×10⁷ | 3.3 GB | 8.1 GB | ~20 GB | **fits** |
| PaperSeek + OpenAlex | 1.02×10⁸ | 4.6 GB | 11 GB titles · **~60 GB with abstracts** | ~30 / ~110 GB | **fits; abstracts are the tight one** |
| TreeOfLife | 2.33×10⁸ | 10 GB | 10.5 GB | ~35 GB | **fits** |
| OpenAlex works | 3.22×10⁸ | 14 GB | 14.5 GB | ~45 GB | **fits** |
| Overture addresses | 4.73×10⁸ | 21 GB | 21 GB | ~60 GB | **fits** |
| Overture buildings | 2.53×10⁹ | 100 GB | **114 GB** | **~214 GB** | **needs §4** |
| GBIF | 3.50×10⁹ | 140 GB | **157 GB** | **~300 GB** | **needs §4** |

**Seven of nine rungs fit on the disk we have.** Only the two true 10⁹ rungs do not, and they fail
on W3 rather than on anything conceptual.

## 4. The disk, and why the answer is a second ext4 volume rather than the share

`/` is 393 GB with **126 GB free**; clearing `data/` (41 GB, §5) and `clients/ts/.dev` (4 GB)
reaches ~170 GB, and `target/debug` (55 GB) is a further reserve at the cost of one rebuild. That is
enough for seven rungs and not for two.

**The share cannot take the overflow** — never serve or mmap a bundle from it (67 MB/s, a page
fault is a network round trip). Nor can `/mnt/d` or `/mnt/e`: they are **9p** filesystems, and a
build or a serve over 9p measures 9p.

**Create a second ext4 volume**, as a VHDX attached to WSL with `wsl --mount --vhd`. That gives a
real block device with real `mmap` and real page-cache behaviour — the properties every measurement
in this corpus depends on. A 400 GB volume clears both 10⁹ rungs with room for the transient.

⚠ **Corrected 2026-08-29: not on D:, and the drive this memo named cannot carry it.** The media
were read after rung 2 and they are not interchangeable. D: is a **WDC WD20EZRX, a spinning disk**,
with 744 GB free; C: is a Samsung 980 PRO NVMe with 29 GB free; E: is a WD_BLACK SN850X NVMe with
115 GB free. A bundle served from a VHDX on D: would answer every page fault with a seek, which is
the same class of error as serving from the share and for the same reason — the medium, not the
filesystem. **The capacity is on the HDD and the speed is on E:, and no volume on this box has
both**; 115 GB does not reach a 10⁹ rung. This is an open constraint on the ladder rather than a
choice this memo can make, and it is the owner's.

⊘ **Still unverified:** that `wsl --mount --vhd` on this Windows build attaches a VHDX at all, and
what the resulting device sustains. Measure it exactly as the share was measured before any figure
is taken against it, and record the medium in every figure after.

## 5. Cleanup — what may go, what may not

**The gate does not read `data/`.** The only two tests naming paths under it —
`crates/tessera-engine/tests/viewport.rs::latency_sanity_at_2_4m_p99_under_50ms` and
`crates/tessera-build/tests/build_equivalence.rs::reference_build_at_scale` — are both `#[ignore]`d.
All six gate commands stay green with `data/` empty.

**Four files may never be deleted before the NAS copy is verified**, because nothing regenerates
them:

- **`data/geometry.parquet` + `.sha256`** — cuML's `nn_descent` is not bit-reproducible even under a
  fixed `random_state`, which is why this artifact is *hashed rather than seeded*
  (`probes/dataset.md` §3). Deleting it does not cost a rebuild; it costs **a different corpus
  wearing the same name**, and with it every figure in `probes/results.md`, the tile-occupancy table
  at `dataset.md` §4.3, and the 10⁹ replicas that are affine transforms of *these* points.
- **`data/corpus.parquet`** and **`data/embed_paper_ids.parquet`** — regenerable in principle from
  the Kaggle snapshot v296, which **is not on this disk**.
- **`data/demo/prose.parquet`** — same source, same absence.

Everything else under `data/` is scripted output: `scaled/` (31 GB) regenerates in minutes from
`build_scaled_corpus.py`, `demo/points-*` from `build_demo_datasets.py`, `pairs/` from
`gen_pairs.py`, `filter-lifecycle/` and `bench-fixtures/` from their own scripts. **Copy all of it
to the NAS, verify, then delete all but the four.**

⊘ **A trap for any pipeline forked from `build_geometry.py`:** the shipped `geometry.parquet`'s
`morton`/`gx`/`gy`/`row_id` columns are a **transpose** — the x/y axes were swapped until
2026-08-02 and the file was deliberately not regenerated. Take `x`/`y` and re-quantise with the
current code; do not carry the file's derived columns forward.

## 6. Projection — the graph is the product, and PCA-first was never the right shape

**Correcting this memo's own first draft, and the pipeline it inherited.** `build_geometry.py` does
exact covariance PCA to 64 dimensions and then UMAP, and its module docstring says why: *"VRAM holds
neither 2.4M x 1024 nor the UMAP knn graph over it"*. That is a **workaround for a 10 GB card**, and
this memo first restated it as though it were method. It is not, and for embeddings specifically it
is the wrong trade:

- **The kNN graph is the entire input to the layout.** Everything UMAP does after neighbour-finding
  operates on that graph. Reducing dimension first makes the expensive step cheap *by changing which
  neighbours it finds*, which is the one part of the pipeline that should not be approximated.
- **PCA's premise does not hold for encoder outputs.** It keeps high-variance directions, but in
  sentence and image embeddings the dominant components tend to carry magnitude, frequency and
  length artefacts rather than semantics — the isotropy and "all-but-the-top" results point the
  other way, that removing dominant directions can improve discriminability. ⊘ Literature, not
  measured here. The arXiv build retained 82.2% of variance at 64 dimensions; nothing establishes
  that the discarded 18% was the uninformative part.
- **It silently changes the metric.** The corpus vectors are L2-normalised BGE outputs where cosine
  is the meaningful distance. PCA centres and reprojects, so the geometry UMAP sees is not the
  geometry the encoder produced.

### 6.1 The shape that replaces it

**Build the kNN graph in full dimension with a purpose-built ANN index, then hand UMAP a
`precomputed_knn` and let it do only the layout.** `umap-learn` and cuML both accept one. This
removes the reason PCA was there, and it also dissolves the memory argument: the vectors are read in
batches and never all resident — **only the graph is**, at `n × k × 8` bytes for indices and
distances.

| Rung | Vectors, fp32 | **Graph at k=15** |
|---|---|---|
| arXiv 2.42×10⁶ × 1024 | 9.9 GB | **0.3 GB** |
| MedCPT 3.6×10⁷ × 768 | 111 GB | **4.3 GB** |
| PaperSeek 1.02×10⁸ × 1024 | 418 GB | **12 GB** |
| TreeOfLife 2.33×10⁸ × 768 | 358 GB | **28 GB** |

Every graph fits in 47 GB of RAM. The vectors never need to.

### 6.2 The experiment that settles it, on data already on disk

`data/arxiv_papers_embeds.parquet` is 2.42M × 1024 and local, and the map built from it is hashed
and characterised. **Build it both ways and compare**: full-dimension cosine kNN into UMAP, against
the shipped PCA-64 route. Judge on the **tile-occupancy table at `probes/dataset.md` §4.3** — not on
how the pictures look — because tile occupancy under Morton order is the property every measurement
in this corpus rests on. Report Morton autocorrelation and the mask-scatter ratios beside it.

Two outcomes, both useful: the routes agree, and PCA-64 is a sound cheap path to reuse at every
rung; or they do not, and **the shipped geometry has been carrying a distortion since Phase 0** —
which is worth knowing before seven more corpora inherit the same pipeline.

### 6.3 Tooling, and a hardware caveat

`notebooks/.venv` has `umap-learn` 0.5.12, `pynndescent` 0.6.0 and torch 2.13+cu130, and **no cuML,
cuVS, faiss or cupy** — the cuML used in Phase 0 lived in a scratchpad environment that no longer
exists. A dedicated environment with `cuml-cu12` and `cuvs-cu12` is being built at
`~/venvs/projection`. The CPU route (`pynndescent` in full dimension, which is what `umap-learn`
does by default) is the fallback and is the comparison point for whether the GPU route is merely
faster or actually necessary.

⊘ **The GPU is currently unavailable.** At the time of writing 9.2 GB of the 3080's 10 GB is held by
a Windows-side process — WSL sees only `Xwayland` — at 52% utilisation and 78 °C. Nothing GPU-bound
in this campaign can be scheduled until that is freed, and the projection experiment should not be
attempted against 1 GB of headroom.

### 6.4 The one property that is not negotiable

Decision 0091 makes build and ingest the same operation, so rows arriving after a build must land in
the frame the build established. **A projection that must be re-fitted to place a new point is not
usable for ingest at all** — a re-fit moves every existing point and invalidates every stored Morton
code and every artifact extent. Whatever route survives §6.2 must end in a **frozen transform**, and
each rung's extent must be declared with headroom rather than fitted to what happens to be present
(`configuration.md` §1: `auto` sees only the data at build, and the first out-of-range ingest
clamps).

**Every projection is hashed and archived on the NAS**, exactly as `geometry.parquet` is and for the
same reason: it cannot be reproduced bit-for-bit, so the artifact *is* the record.

## 7. The order, and what each rung is for

One rung at a time, torn down between — not because disk forces it (seven fit) but because a rung
that overlaps another cannot have its build wall time, peak RSS or ingest rate attributed, and those
numbers are half of what this campaign is for.

| # | Rung | Why here | What it is the only source of |
|---|---|---|---|
| **0** | Re-run the artifact campaign's 5×10⁷ tier | Confirm W1 and W2 reproduce, and whether the post-2026-08-23 pre-flight now **refuses** instead of being killed | — |
| **1** | **GeoNames** 1.2×10⁷ | Fits everywhere; no projection at all; every surface at once | Two orthogonal hierarchies over one point set (feature class→code, and admin1–4); daily `modifications`/`deletes` files — **the cheapest real churn stream in the ladder** |
| **2** | **Overture places + divisions** 7.4×10⁷ | First rung past the 5×10⁷ wall — **W1 and W2 are expected here, and that is the point** | `division_area`: 1.07M polygons, 12 subtypes, explicit `hierarchies[]` — the only tiered boundary layer |
| **3** | **MedCPT / PubMed** 3.6×10⁷ | The projection experiment's first real test (§6) | MeSH: 30,594 descriptors, 64,687 tree numbers, depth 13, **a polyhierarchy** — the case `configuration.md`'s two-parents refusal has never met |
| **4** | **PaperSeek + OpenAlex** 1.02×10⁸ | Prose at scale; 1024-d vectors with title and abstract **inline** — no join needed for text | Per-work licence and OA status as a real compartment; the 4-level topic tree; the honest 40×-arXiv comparison |
| **5** | **TreeOfLife** 2.33×10⁸ | Largest rung that fits without §4 | The GBIF join — one entity in two geometries. ~~Also: **ingest the same rows in taxonomy order and shuffled**, everything else held, which measures entity-space contiguity directly~~ — **withdrawn, owner ruling 2026-09-03**: allocation orders entity ids within a signature group by Morton code and not by arrival ([`../../design/annotation-representation.md`](../../design/annotation-representation.md) §2.2), so input order changes nothing and the two arms would differ only by the cost of shuffling. The rung is built once |
| **6** | **GBIF** 3.5×10⁹ | Needs §4's volume; the real top rung | Real collection-bias density; per-record CC0/BY/NC; `eventdate` with diurnal and seasonal structure; monthly snapshots as a real add-and-withdraw stream |
| **7** | **Overture buildings** 2.53×10⁹ | Needs §4's volume | 10⁹ geometry with almost no labels — the pure point-layer scale test |

### 7.1 What each rung needs that does not exist yet

Two capabilities are being designed alongside this campaign —
[`../../design/projections.md`](../../design/projections.md) and
[`../../design/polygon-membership.md`](../../design/polygon-membership.md) — and the ladder is what
forces each. **Blocking** means the rung cannot be built correctly without it; **degrades** means it
can be built and something is lost or worked around.

| Needed | First rung | Blocking? |
|---|---|---|
| **A y-south frame definition** | **1, GeoNames** | **Blocking.** Any geographic corpus built before this is decided is vertically mirrored against every basemap, and the extent cannot be inverted to repair it (§9). The first geographic build must not happen until the frame's y direction is settled |
| **A declared projection, and `/v1/meta` serving it** | 1, GeoNames | Degrades. A rung builds without it by projecting outside and stating an extent — which is what §9 does — but no client can align a basemap or invert a position, so the map has no ground under it |
| **A clip counter distinct from the clamp counter** | 1, GeoNames · matters at **6, GBIF** | Degrades at rung 1, misleads at rung 6. Clipped points sit exactly at the frame maximum where the clamp report reads zero by contract; GBIF has 69,486 records beyond ±85.0511° |
| **`f64` on the coordinate path** | none as scoped | Not blocking — every rung here is global, where `f32` wastes residual bits only. **Blocking for any regional corpus or any 2^k-aligned sub-square**, where past roughly zoom offset 8 it corrupts the cell itself, silently |
| **Polygon membership** | **2, Overture divisions** | Degrades. The interim is the offline point-in-polygon join of §9.2, which is exact and unblocks the rung; what it costs is a column per level and a re-ingest rather than an edit when a boundary is revised |
| **W1 — the Roaring round trip at 5×10⁷ members** | **2, Overture places** | **Blocking** for a whole-corpus root cluster at 7.4×10⁷, and expected there (§8) |
| **W2 — build peak RSS bounded by its budget** | **2, Overture places** | **Blocking** above ~5×10⁷ points carrying artifacts, and expected there (§8) |
| **A ruling on MeSH's polyhierarchy** | **3, MedCPT** | **Blocking** for that rung's artifact layer (§8) |
| **The streaming text column** | **4, PaperSeek** | Degrades. Titles build; abstracts at 1.02×10⁸ do not (§8) |
| **A second local volume** | **6, GBIF · 7, buildings** | **Blocking** — the transient exceeds the disk (§4) |

**A rung is done when** its declaration passes `tessera check --payloads`; the bundle exists with
the build's own frame report recorded verbatim; **decision 0091's own test passes on real data** —
the same rows reached by build and by ingest, the two databases identical to a client for at least
three principals over at least six viewports; a masked-count census is exact against an oracle built
from the source files; one full write cycle completes (suppress a real compartment → delete a slice
→ re-ingest → fold → re-census, counts right at every step); and there is a results row carrying
build wall time, peak RSS, bundle bytes, ingest rows/s, p99 at three zooms **and a screenshot** — a
rung whose numbers are good and whose map is a blob has failed.

## 8. What this campaign is expected to break

Stated in advance so that hitting them is a result rather than a surprise. Each is a Tessera defect
to fix, per the owner direction this memo is written on.

1. **W1 at rung 2** — a whole-corpus root cluster over 7.4×10⁷ places will exercise the membership
   that failed at 5×10⁷. Expect the refusal; bisect the Roaring round trip.
2. **W2 at rung 2** — expect the OOM, and check first whether the pre-flight now refuses. If it
   still kills, `--memory-budget` is the fix, not more RAM.
3. **MeSH's polyhierarchy at rung 3** — a descriptor with several tree numbers names a child under
   two parents, which the configuration surface refuses outright. Either the refusal is correct and
   MeSH needs a chosen-and-recorded primary tree number per descriptor, or the refusal is too
   strict. That is an owner ruling, and rung 3 is what forces it.
4. **Abstracts at rung 4** — the attribute pass holds a text column whole; 1.02×10⁸ abstracts at
   442 B/row is ~45 GB of strings. This is the case that forces the streaming text column that
   `build_demo_datasets.py` already names.
5. **Small-file column projection over SMB** — GBIF is 8,375 parquet files and OpenAlex's `works` is
   Hive-partitioned across thousands of parts. The 67 MB/s figure is *sequential*; a footer read
   plus per-column range reads over SMB may be nothing like it. ⊘ Measure on 20 files before
   committing a plan to the projected figure; the fallback is to copy whole files and prune locally
   with a bounded working set.
6. **The extent decision at rung 1**, which is the operator's and is inherited by every geographic
   rung after: `auto` squares the box and spends half the grid on empty latitude; a per-axis extent
   stretches the map 2:1; a Web Mercator *y* looks correct and makes every stored position depend on
   a transform nobody declared.

---

## 9. Per rung: preprocessing, declaration, what is rendered and indexed, and the artifacts

**Three properties of the current surface shape every rung below, and each is a constraint rather
than a preference.**

**(a) Spatial membership is bounding-box only — and this is a required feature, not a constraint to
design around.** The requirements are stated in
[`../../design/polygon-membership.md`](../../design/polygon-membership.md), which this campaign
depends on: every geographic rung below uses the offline-join route in the interim, and each is a
consumer of that capability when it exists.

**How the interim works.** `[layer.shape]` takes `{ kind = "bbox", depth }`
and **a polygon or a radius is refused** — an approximate cover would be a membership *wider* than
the declaration. So an administrative boundary layer cannot be declared by handing the server
polygons. Two honest routes, and the second is the recommendation everywhere below: cover each
boundary by its bbox and accept that a ward's membership is its rectangle; or **do the
point-in-polygon offline** and declare `membership = { attribute = "<code>" }`, whose artifacts are
the distinct values of an indexed column. The second is exact, costs one join at preprocessing, and
turns every boundary set in the ladder into an ordinary category column. `depth` is not a tuning
knob either way — the same box at depth 4 and depth 8 holds different points, so it *is* the
membership.

**(b) `[layer.members]` accepts a list per point, and that is how a taxonomy is declared.** One row
per point naming every artifact it belongs to; under `tiered` the entries are one per declared level
and consecutive entries are the containment edges, under `nested` the list is a lineage. So a
seven-rank taxonomy is a single member file with a seven-element list per row — not seven files, and
not a join. A null key or `-1` means *in no artifact* and is counted rather than refused, which is
what a clusterer's noise points need.

**(c) Web Mercator is the projection for every geographic rung, and the reason is structural.**
`client-interaction.md` §12: a Web Mercator projection into the quantised bounds makes an XYZ slippy
tile **identically a Morton prefix**, so the engine already speaks the tile addressing that
MapLibre, OpenLayers and QGIS want. That settles §8's open extent question for the geographic side:
project longitude and latitude to metres and clip latitude to ±85.0511°. ⊘ **The extent this memo
first wrote — `{ min = -20037508.34, max = 20037508.34 }` — is wrong, and wrong in a way nothing
would report**: `clients/ts/core/src/coords.ts` pins, against deck.gl's own `Tileset2D`, that tile y
and cell y increase together, so cell y = 0 must be **north**, while EPSG:3857's northing increases
northward. Every corpus built on that extent is vertically mirrored against any basemap, and
`Bounds::validate` requires `y_max > y_min` so it cannot be repaired by inverting the extent. The
frame must be defined y-south. See [`../../design/projections.md`](../../design/projections.md). `auto` would square a box around the data — a
different frame per dataset, so two corpora could not share a tile address — and a per-axis extent
would stretch the map and break the prefix property. The cost is Mercator's own area distortion,
which is a rendering fact to state beside any density figure, not a defect.

### 9.1 GeoNames — 1.2×10⁷

**Preprocessing** — one DuckDB pass, no GPU, minutes:

1. `allCountries.txt` is tab-separated with no header; read it with the 19 column names from
   `geonames-official-readme.txt` and explicit types.
2. Project `(longitude, latitude)` to Web Mercator metres, clipping latitude to ±85.0511°.
3. Assign `entity_id` densely from 0 in **`(modification_date, geonameid)` order**, so that replaying
   the file in id order *is* the time-ordered ingest of §7 rather than a second sort.
4. Write `points.parquet` carrying identity, `x`, `y` and every rendered column.
5. Write two vocabulary files (`feature_class` 9 keys, `feature_code` 645) from `featureCodes_en.txt`,
   and `country` (~250) from `countryInfo.txt`, each `key,code,title`.
6. Write `members-feature.parquet`: one row per point, `key` a **two-element list**
   `[feature_class, feature_code]` — the tiered taxonomy in one file, per (b).
7. Write `members-admin.parquet`: one row per point, `key` a **five-element list**
   `[country, admin1, admin2, admin3, admin4]`, nulls where the source has none.
8. `hierarchy.txt` is `parentId, childId, type` **between features** — a containment tree of places
   rather than of categories. Write it as `members-places.parquet` under a `nested` layer, each
   artifact a place keyed by its geonameid and its lineage the entry list.

**What is rendered and what is indexed.** `render` puts a fixed-width code in every row of
`columns.arrow` so a mark can be coloured by it; `index` builds the entity-space postings so the
same predicate is answered without scanning the request's rows. Declaring both is what lets decision
0068 route on cost, and it is right for any column that is both a colour and a filter.

| Column | Type | render | index | Why |
|---|---|---|---|---|
| `feature_class` | `category` u8, 9 values | ✓ | ✓ | the default colouring, and a coarse filter |
| `feature_code` | `category` u16, 645 | ✓ | ✓ | the fine colouring; 645 values is a real vocabulary listing test |
| `country` | `category` u16 | ✓ | ✓ | colour, filter, and the synthetic compartment |
| `admin1` | `category` u16 | ✗ | ✓ | filtered, never coloured — the case for index-without-render |
| `population` | `u32` | ✓ | ✗ | a violently skewed numeric range filter; Tokyo fits u32 |
| `elevation` | `i16` | ✓ | ✗ | a second numeric, with real nulls |
| `modification_date` | `timestamp_us` | ✓ | ✗ | the date picker, and the ingest order |
| `name` | `text` | ✗ | ✓ | prose search; 12M short names is affordable whole |
| `timezone` | `category` u16 | ✗ | ✓ | a third categorical for filter-combination tests |

**The access side is synthetic and must be labelled as such.** GeoNames carries no rights or
audience field. Declare `point_visibility = { field = "country", default = "public" }` so every point
carries its country as its term — a real column standing in for a compartment. Every figure it
produces says *synthetic policy over real data*.

```toml
[sources]
points         = "points.parquet"
feature_class  = "vocab-feature-class.parquet"
feature_code   = "vocab-feature-code.parquet"
country        = "vocab-country.parquet"
members_feature = "members-feature.parquet"
members_admin   = "members-admin.parquet"
members_places  = "members-places.parquet"

[defaults]
source = "points"

[[view]]
name             = "world"
title            = "GeoNames"
extent           = { min = -20037508.34, max = 20037508.34 }   # Web Mercator, square: a tile is a Morton prefix
point_visibility = { field = "country", default = "public" }

[[vocabulary]]
name = "feature_class"
width = "u8"     value_set = "closed"  visibility = "public"  source = "feature_class"
# … feature_code (u16), country (u16) likewise; all three are published taxonomy, so `public` is safe

[[attribute]]
name = "feature_class"  type = "category"  vocabulary = "feature_class"  render = true  index = true
# … the rest of the table above

[[layer]]
name       = "features/taxonomy"
title      = "Feature taxonomy"
views      = ["world"]
membership = "enumerated"
value_set  = "open"                    # the roster is what the points name
hierarchy  = { kind = "tiered", prune_children = true }
visibility = "public"
artifact_visibility        = { default = "inherited" }
require_member_visibility  = { count = 1 }
[[layer.levels]]  level = 0  title = "Class"
[[layer.levels]]  level = 1  title = "Code"
[layer.members]
source = "members_feature"

[[layer]]
name       = "admin/hierarchy"
views      = ["world"]
membership = "enumerated"
value_set  = "open"
hierarchy  = { kind = "tiered", prune_children = true }
visibility = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { count = 1 }
[[layer.levels]]  level = 0  title = "Country"
[[layer.levels]]  level = 1  title = "Admin 1"
[[layer.levels]]  level = 2  title = "Admin 2"
[[layer.levels]]  level = 3  title = "Admin 3"
[[layer.levels]]  level = 4  title = "Admin 4"
[layer.members]
source = "members_admin"
```

**Why this rung earns its place**: two independent hierarchies over one point set — a category tree
and an administrative tree — plus a third, `places/containment`, that is a `nested` lineage between
the features themselves. Three artifact layers, one corpus, no projection, and it fits in 4 GB.

### 9.2 Overture places + divisions — 7.4×10⁷

**Preprocessing** — DuckDB with the `spatial` extension, over the staged GeoParquet:

1. `ST_X`/`ST_Y` on the `geometry` column for point coordinates; project to Web Mercator as above.
2. **The division join is the whole trick, and it happens here rather than in the server.** Load
   `theme=divisions/type=division_area` (1.07M polygons), and for each place compute the
   `division_id` of every containing division at each of the twelve subtypes — one
   `ST_Within` join, bounded by a bbox pre-filter. Write the result as twelve nullable columns.
   Per (a) this turns the boundary layer into `membership = { attribute = "division_locality" }`
   and friends: **exact containment, evaluated once, and no polygon ever crosses the wire as a
   membership**. The polygons remain available as *content* for drawing.
3. `taxonomy` on a place is a path; write it as a list column, deepest-first truncated to six, for a
   `tiered` member source.
4. Order by `update_time` for the ingest replay.

| Column | Type | render | index |
|---|---|---|---|
| `basic_category` | `category` u16 (~300) | ✓ | ✓ |
| `confidence` | `f32` | ✓ | ✗ |
| `country` | `category` u16 | ✓ | ✓ |
| `source_dataset` | `category` u8 | ✓ | ✓ |
| `division_locality`, `division_county`, `division_region` | `category` u32 | ✗ | ✓ |
| `name` | `text` | ✗ | ✓ |
| `update_time` | `timestamp_us` | ✓ | ✗ |

Layers: `places/taxonomy` (`tiered`, six levels, from the path list); **`boundaries/divisions`**
(one layer per level or one `tiered` layer over the joined columns, `membership = { attribute = … }`,
which declares no content and no hierarchy but `flat` — so the twelve-level tree is expressed as
twelve attribute layers or as one tiered enumerated layer built from the same join, and **which of
those the surface prefers is the first thing to test at this rung**); and `programmes/source`
(`{ attribute = "source_dataset" }`) as the tagged-programme case.

**This is the rung where W1 and W2 are expected**, because a whole-corpus root cluster over 7.4×10⁷
members is exactly the membership that failed its Roaring round trip at 5×10⁷.

### 9.3 MedCPT / PubMed — 3.6×10⁷

**Preprocessing**: read the 38 `.npy` chunks and their `pmids_chunk_N.json` in order; build the kNN
graph in **full 768 dimensions** with cosine (§6); UMAP layout from the precomputed graph; quantise
to the view's extent — here `auto` with a margin, since an embedding projection has no natural frame
and nothing else shares its tile space. **MeSH is not staged**: acquiring the 51.8 GB PubMed baseline
and extracting `(pmid, descriptor, tree_numbers)` is a prerequisite, not a step.

Rendered: `year` (`u16`), `journal` (`category` u16), `publication_type` (`category` u8). Indexed:
all three, plus `title` (`text`) — **abstracts are deferred at this rung**, 36M × ~1 kB being past
what the attribute pass holds. Artifacts: `mesh/tree` as `tiered`, one level per MeSH depth, from a
list-per-point member file.

⊘ **The polyhierarchy meets a refusal here.** A descriptor carries several tree numbers, so a
lineage list names a child under two parents, which the surface refuses. Either MeSH gets a
*chosen and recorded* primary tree number per descriptor — a preprocessing decision, written down as
data — or the refusal is too strict. Rung 3 forces that ruling.

### 9.4 PaperSeek + OpenAlex — 1.02×10⁸

**Preprocessing**: the 53 chunk parquets carry `id`, `title`, `abstract` and a 1024-d `embedding`
**inline**, so no prose join is needed. Build the graph in full dimension, lay out, quantise. Join
OpenAlex by work id for `primary_topic`, the four-level topic lineage, `license`, `is_oa` and the
institution list — the topic dimension tables are small top-level files, so the roster of 4 domains,
26 fields, 252 subfields and 4,516 topics is a direct read rather than a scan of the 300 GB `works`
partition.

**This is the first rung with a real compartment**: `point_visibility = { field = "license", default = "public" }`,
CC-BY on ~29.2M works against everything else. Rendered: `publication_year`, `primary_topic`,
`is_oa`, `type`. Indexed: those, plus `title` and — **the forcing case** — `abstract`, at 442 B/row
× 1.02×10⁸ ≈ 45 GB of strings.

Layers: `topics/openalex` (`tiered`, four levels), `clusters/hdbscan` (`nested`, `value_set = "open"`,
lineage list per point, noise as null), `topics/labels` (`flat`, `depends_on` the clustering,
c-TF-IDF text with generating sets — the disclosure-controlled label case).

### 9.5 TreeOfLife — 2.33×10⁸ · 9.6 GBIF — 3.5×10⁹ · 9.7 Overture buildings — 2.53×10⁹

**TreeOfLife**: taxonomy-sorted parquet, so read the ~29 B/row label columns whole (6.8 GB) and
stream the vectors for the graph. `taxonomy/tree` as `tiered` with seven levels;
`publishers/source` as `{ attribute = "publisher" }`. ⊘ **Withdrawn, owner ruling 2026-09-03**: the plan's "ingest the same
rows twice, in file order and shuffled" measures nothing, allocation having made entity order within a
signature group the Morton code's rather than the input's
([`../../design/annotation-representation.md`](../../design/annotation-representation.md) §2.2:
`(signature, morton_code, source_id)`).

**GBIF**: an 18-column projection of 104 B/row down to ~25 B/row. `point_visibility = { field = "license", default = "public" }`
is **the real per-record compartment in the ladder**. Order by `eventdate`. Rendered: the seven ranks
as categories, `basisofrecord`, `year`, `coordinateuncertaintyinmeters`. Indexed: those plus
`locality` as text and `issue` — ⊘ which is multi-valued, and `multi` is refused at parse today, so
`issue` needs either one boolean per flag or a second member layer. Layers: `taxonomy/gbif`
(`tiered`, seven levels), `datasets/publisher` (`{ attribute = "datasetkey" }`).

**Overture buildings**: centroids only, `height`, `num_floors`, `roof_shape`, `source_dataset` — the
pure point-layer scale test with almost no artifact side, which is the point of having it.
