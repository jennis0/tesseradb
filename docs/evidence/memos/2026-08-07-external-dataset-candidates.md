# Candidate external datasets for the fixture gaps

**Status:** Evidence — survey, never normative. Web survey run 2026-08-07 (nine searches plus a
direct fetch of the OSM tile-log archive); every figure below is **as published by its source on
that date**, not verified by download, except items marked *prior knowledge*, which are weaker
still. Nothing here is adopted by being listed; adoption is per-dataset work with its own probe.

Commissioned from the measurement-design audience review. The synthetic corpus brackets *policy*
shape deliberately and well ([`probes/dataset.md`](../../../probes/dataset.md) §4.4) and brackets
*geometry* shape not at all — every replica repeats the base's topic geometry under transformation,
and dataset.md §7 already concedes the label configs "are synthetic policies chosen to bracket
distributional shapes, not to predict any deployment's". This memo records what real data exists
for the axes the fixtures cannot supply.

**This does not reopen measurement.md §11's "no new corpus".** That clause scopes the axes that
document added, all constructible within existing fixtures; the gaps here — geometry shape, real
hierarchy, churn profile, the workload model — are outside them.

---

## 1. Recommendations

Four adoptions, in order of value per unit of plumbing:

1. **A GBIF extract at 10⁹.** One dataset closes four gaps: real geographic density, a real
   deep hierarchy for policy generation, real timestamps for time-ordered ingest, and a wide,
   null-heavy attribute tail. Parquet-native at the source; the plumbing is a column-pruned,
   licence-filtered sample.
2. **OSM tile logs for the session arm.** Turns the assumed zoom distribution
   (measurement §6.1) into measured data for the price of parsing one file per day.
3. **Semantic Scholar SPECTER bulk** — the "same corpus flavour, real at ~200M" upgrade: real
   author Zipf (what `surnames` synthesises), real field-of-study labels, real years. The cost
   is a UMAP at 200M, which is not small.
4. **A real-ACL calibration set** (role-mining suite + Amazon UCI + Enron). Not fixtures — too
   small by orders of magnitude. A calibration probe measures their density, Zipf exponents and
   signature histograms and scores the synthetic generators against them, upgrading dataset.md
   §7's sentence to "bracket verified against every real matrix publicly available".

LAION is the reserve: the only true-10⁹ real-embedding option, if the replica caveat ever needs
killing at full scale. The building-footprint sets become current when tile-addressed serving
(roadmap #7) does.

---

## 2. Real geographic density

The gap: the corpus's only geometry is UMAP output — concentrated blobs, no empty ocean, no
collection bias. Real geographic data is a different density regime, and it skips UMAP entirely:
quantise lat/lon to the 2¹⁶ grid, Morton-rank, done — cheaper than the current geometry build.

| Dataset | Scale (as published) | What it exercises |
|---|---|---|
| **GBIF occurrence snapshot** | **3,499,090,951 records** (2026-06-01), parquet on AWS S3 in five regions; H3-partitioned cloud-optimised mirror on source.coop | Real global clumping — cities, roads, rivers, museums — plus taxonomy, publishers, dates, and the Darwin Core attribute tail (§5). Per-record CC0/CC-BY/CC-BY-NC: **filter to CC0+CC-BY before anything publishable** |
| **Google Open Buildings** | 1.8B detections over 58M km² (Africa, South/South-East Asia, Latin America); gzipped CSVs sharded by S2 level-4 cell: lat/lon, area, confidence, polygon WKT | Population-weighted density; confidence is a ready-made numeric filter operand |
| **VIDA combined Google+Microsoft(+OSM) buildings** | GeoParquet on source.coop | The union, already in the format the pipeline wants |
| **Overture Maps** | Monthly GeoParquet releases on S3/Azure; buildings in the billions, places with hierarchical categories; bbox-filtered CLI | The natural fixture source when #7 approaches — it is what MapLibre/QGIS users will actually point at the server. ODbL / CDLA-Permissive-2.0 |
| NYC TLC taxi *(prior knowledge)* | ~1.5B+ trips; precise lat/lon 2009–mid-2016 only, zone IDs after | The adversarial hotspot: Manhattan is replica 1's compression done by reality |

## 3. Real embedding geometry at scale

The gap: replicas repeat the base topology, so nothing above 2.42M is real embedding structure.

| Dataset | Scale (as published) | Notes |
|---|---|---|
| **Semantic Scholar Datasets API** | Bulk snapshots of the full graph — papers, authors, citations, venues, and **SPECTER embeddings** for a ~200M-paper corpus. ODC-BY | Best fit: scholarly like the current corpus, ~100× larger, with real labels. Needs our own projection at 200M — batched cuML or sample-fit + transform; budget it as real work |
| **LAION-5B** | 5.85B CLIP embeddings, parquet + npy; ClickHouse hosts a 100M-vector parquet subset (10 files × 10M rows) | True 10⁹-scale real embeddings. Label side is weak (language/NSFW/similarity only) — synthetic policies over real geometry. *Prior knowledge:* use the cleaned **Re-LAION-5B** re-release for provenance hygiene |
| **Cohere wikipedia-2023-11-embed-multilingual-v3** | ~250M passage embeddings on Hugging Face, streamable | Language × article is natural slice/compartment structure — a real multi-slice fixture candidate for the slices epic (#48) |

## 4. Real policy structure — and a negative result

**No large real enterprise permission corpus is public. NOT closable by search — do not plan
against one appearing.** The access-control ML survey literature confirms only a handful of small
real datasets exist. What does:

| Dataset | Shape (as published; figures vary slightly with papers' preprocessing) | Provenance |
|---|---|---|
| `healthcare` | 46 users × 46 permissions, 1,486 assignments, 70% density | US Veterans Administration |
| `domino` | 79 × 231, 730 assignments, 4% | A Lotus Domino server |
| `emea` | 35 × 3,046, 7,220 assignments, 6.8% | Enterprise, via HP Labs |
| `firewall1` / `firewall2` | 365 × 709, 31,951 (12.3%) / 325 × 590, 36,428 (19%) | Checkpoint firewall analyses |
| `americas` | ~10k users × ~3.5k permissions (small/large variants differ) | Enterprise, via HP Labs |
| **Amazon UCI access samples** (2011) | Grant/deny access logs, thousands of employees, with role/department attributes | Amazon; UCI repository (Kaggle 2013 challenge is a derivative — *prior knowledge*) |
| **Enron** *(prior knowledge)* | ~500k messages; recipient lists are genuine per-document reader sets over a real org | FERC release, CMU hosting |

The HP Labs originals are no longer hosted there; they circulate through role-mining tooling
(RMiner) and the papers' artefacts — pin whichever mirror is used, with checksums.

All of these are calibration data, not fixtures (§1 item 4). For hierarchy *at scale*, GBIF's
taxonomy (a real seven-plus-level tree with natural fan-out) and S2's fields of study and author
sets are real-shaped but **not access-control-born** — a grant on `Aves` is a plausible policy, not
an observed one. State that beside any figure they produce.

## 5. Churn streams and attribute tails

- **Wikipedia full revision history** — English alone runs past 10⁹ revisions (SNAP mirrors a
  processed form). Real diurnal shape, real bursts, and reverts/re-edits are the delete +
  re-ingest stream [decision 0047](../../decisions/0047-edit-is-delete-plus-reingest.md) implies —
  the fold's food, which nothing currently generates.
- **OSM changesets** — full history ships with the planet dumps; a North-America study alone spans
  13.4M changesets from 46,595 users. Editing bursts follow imports and events.
- **GDELT** *(prior knowledge)* — georeferenced event firehose on a 15-minute cadence; a steady
  high-rate profile.
- **Attribute tails**: GBIF's Darwin Core columns are the realistic case the fixture knob should
  imitate — mixed types, high-cardinality categoricals, and *heavy, structured nullness* — and S2
  (year, venue, fields of study, citation counts) is the scholarly equivalent. Both beat inventing
  distributions.

## 6. The workload model

**Confirmed by direct fetch:** [planet.openstreetmap.org/tile_logs/](https://planet.openstreetmap.org/tile_logs/)
publishes daily aggregated per-tile request counts (`tiles-YYYY-MM-DD.txt.xz`, ~31–62 MB/day
compressed) with hosts/countries/apps sidecar CSVs, reaching back to at least 2015. That grounds
the session arm's **zoom mix and spatial popularity skew** in measured data. It does **not** close
measurement §6.1's caveat — the logs are population-level, so think time and per-session
sequencing stay assumed; the caveat narrows rather than closes.

---

## 7. Practicalities

**The NFS mount is the acquisition, staging and archive tier** — raw snapshots (full GBIF is
>1 TB; a column-pruned 10⁹ extract is tens of GB and lives locally), S2 bulk, revision dumps, tile
logs, and **built fixtures as artefacts**: archived bundles — including aged ones, once a
generator exists — rsync in per campaign instead of rebuilding. Two disciplines keep the
measurements honest:

1. **Never serve or mmap a bundle from NFS.** Every fault becomes a network round-trip: the
   residency arm would measure NFS rather than page-cache reclaim, and `major_faults` loses its
   meaning. Copy in, then measure.
2. **The build figure names its source medium** (measurement §6 figure 8). The ingest-build arm's
   "reads are cache-warm after the first" observation does not hold over NFS; the published figure
   is from local source, and a network-source variant is a second figure, not a substitute.

Measure the mount's throughput from inside WSL2 once before relying on it for staging times —
mount routes vary from fine to 9p-disaster, and it decides whether staging is minutes or an hour.

## Links

GBIF: [AWS public data](https://github.com/gbif/occurrence/blob/master/aws-public-data.md) ·
[parquet snapshots](https://data-blog.gbif.org/post/apache-arrow-and-parquet/) ·
[H3 mirror](https://source.coop/cboettig/gbif).
Buildings: [Google Open Buildings](https://sites.research.google/gr/open-buildings/) ·
[VIDA combined](https://source.coop/vida/google-microsoft-open-buildings).
Overture: [getting data](https://docs.overturemaps.org/getting-data/) ·
[releases](https://github.com/OvertureMaps/data).
Embeddings: [S2 Datasets API](https://api.semanticscholar.org/api-docs/datasets) ·
[SPECTER2](https://allenai.org/blog/specter2-adapting-scientific-document-embeddings-to-multiple-fields-and-task-formats-c95686c06567) ·
[LAION-5B](https://laion.ai/blog/laion-5b/) ·
[ClickHouse LAION subset](https://clickhouse.com/docs/getting-started/example-datasets/laion-5b-dataset) ·
[Cohere Wikipedia](https://huggingface.co/datasets/CohereLabs/wikipedia-2023-11-embed-multilingual-v3).
ACL: [role-mining matrices (Constrained Role Mining, table 1)](https://arxiv.org/pdf/1203.3744) ·
[the role mining problem](https://dl.acm.org/doi/10.1145/1266840.1266870) ·
[ML-in-access-control survey](https://arxiv.org/pdf/2207.01739).
Churn/workload: [SNAP Wikipedia edit history](https://snap.stanford.edu/data/wiki-meta.html) ·
[OSM tile logs](https://planet.openstreetmap.org/tile_logs/).
