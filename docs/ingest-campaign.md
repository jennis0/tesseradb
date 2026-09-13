# The ingest campaign — status

**Status:** Working status record, never normative. **This is a status document and is expected to
be edited in place** as rungs land; it is not a dated memo. The plan it executes is
[`evidence/memos/2026-08-27-ingest-campaign-plan.md`](evidence/memos/2026-08-27-ingest-campaign-plan.md),
which is a *plan* and has already been departed from in several places — where the two disagree,
this document records what was actually done and why.

⊘ **This tracker has no pointer in `CLAUDE.md`.** It follows the convention
artifact-delivery.md and client-delivery.md use,
both of which are named there by owner direction. Whether the campaign is tracked here or on issues
is the owner's to settle.

**Last updated:** 2026-09-02.

---

## 1. Where the campaign is

| # | Rung | Points | State |
|---|---|---|---|
| — | arXiv | 2,422,486 | **Have, and now on the campaign's convention.** The pipeline that produces it was a notebook outside `test_corpora/`; ported to [`../test_corpora/arxiv/`](../test_corpora/arxiv/README.md) on 2026-08-28 as `prepare.py` plus an optional `toponymy.py`, and the notebook deleted. It is the ladder's only embedding corpus and the only one whose source is derived rather than staged. **Reworked 2026-09-01**: two views (`knn`, `pca64`) on cuML, both clusterings on both, clusters titled by their own text, taxonomy layer withdrawn (§4.5) |
| 0 | Re-run the 5×10⁷ artifact tier | — | **Deferred, deliberately.** It confirms W1 and W2, which bite at rung 2 and not at rung 1, and it costs a ~45 GB build. Take it before rung 2, not before rung 1 |
| **1** | **GeoNames** | **13,463,857** | **Built, verified and served**, and rebuilt 2026-08-30 on a declared `web_mercator` projection. Not done against §7.1's bar — see §2 |
| **2** | **Overture places + divisions** | **7.4×10⁷** | **Built and verified**, and rebuilt 2026-08-30 on a declared projection with its boundary polygons in longitude and latitude — see §3 |
| **3** | **MedCPT / PubMed** | **35,920,666** | **Built, verified and served** 2026-09-02 — see §4.6. The ladder's largest embedding rung and its first `dag` layer: MeSH's 30,217 descriptors with members over 41,321 edges, membership closed upward to **1.66×10⁹ entries** (3.27× rung 2's spill), an 11.15 GB bundle in 12 m 10 s at 16.03 GB peak, `verify --deep` clean. ⊘ Three non-reproducing host faults over two runs, §4.6 |
| **4** | **PaperSeek + OpenAlex** | **102,117,343** | **Staged and prepared whole; built, verified and served at a 10⁷ prefix; ⊘ stalled at 10⁸** 2026-09-03 — see §4a. The corpus exists: 254 GB staged in one 164.7-minute pass, laid out and joined to OpenAlex in 43.8 minutes at 18.4 GB, 52.2 GB of `points.parquet`, 394,325,928 topic member rows, and the ladder's first compartment that is a property of the row. **`tessera build` reached the abstract text index and stalled there** — not refused, not killed, 93% system time against a 128 GiB mapped arena on a 47 GB box. Both stalls that produced are fixed, and **the whole corpus now builds: 2 h 56 m to a 70.78 GB bundle, `verify --deep` clean, served under a 24 GiB cap with `oom_kill` 0** (2026-09-04, §4b), and **1 h 09 m for the same bundle byte for byte** once the prose stopped being held in entity order at all (§4c). The rung's finding is that negative and its resolution |
| **5** | **TreeOfLife-200M** | **233,055,986** | **Built, verified and served** 2026-09-04 — see §4b. The ladder's largest rung and its first with **two geometries over one entity space**: `bioclip` over every row and `geo` over the 75.90% the GBIF join placed on the ground. A **seven-level tiered taxonomy over every row** — 1,001,193 artifacts, 1.63×10⁹ membership entries — drawn on both views. **39.97 GB bundle in 1 h 10 m at 35.3 GB peak**, `verify --deep` clean in 37.1 s, served under a 24 GiB cap with every masked count identical, and the *f* = 50% ingest cell run: 116,527,993 rows in at 11,060 items/s and a 1,313 s fold. The vectors are 346 GB and were **never staged**: the layout is fitted on 2.5M rows and every row placed in one 2 h 55 m pass off the share |
| 6 | GBIF | **3,495,729,729** | **Built, verified and served under a 24 GiB cap** 2026-09-13 — see §4d. The whole corpus, built twice to measure [the bounded-assembly design](evidence/memos/2026-09-12-bounded-assembly-design.md): **196 GiB bundle (record-blob format 10) in 3 h 30 m 55 s**, against 207 GiB in 4 h 09 m 35 s (format 9) before six branches landed. `verify --deep` clean in 14 m 42 s at 0.61 GB anonymous, against 20 m 50 s at 47 GB anonymous on the earlier bundle. `tessera serve` opens to `/readyz` in 193 s at 6.5 GB anonymous under the cap, `oom_kill` 0 — before the merge set, the open was modelled at ~60 GB anonymous and could not be attempted on this box. One stage, `filter_postings`, still held 7.6 GB over the budget — a residency-model gap, diagnosed and not yet fixed (§4d). The taxonomy still starts at **family**, ruled 2026-09-09: kingdom Animalia's 2.81×10⁹ members would set `layers`' peak by itself |
| 7 | Overture buildings | 2.53×10⁹ | Not started. Staged; needs a second local volume |

**Disk, and a trap in clearing it.** `/` had **23 GB free** on 2026-08-28, not the 117 GB recorded
above at rung 1 — rung 2's ~20 GB transient did not fit. Clearing `data/scaled` (31 GB, regenerates
from `probes/build_scaled_corpus.py` at seed 0) and `target/debug` (53 GB) took it to 104 GB.
⊘ **`target/debug` came back within four minutes**, rebuilt by the IDE's rust-analyzer with no
cargo invocation of ours: it is not durable free space while an editor is attached to this
checkout. `data/scaled` is, and it is the fixture rung 0 and the p99 measurement both read, so
either must rebuild it. ⊘ **Deleting it also breaks the doc-link gate**: three documents cite
`data/scaled/attrs/schema.toml` and `schema-wide.toml`, and `check-doc-links.py` fails on a cited
path that does not exist. Those two files and `scales.json` were kept back and restored — 7 KB, and
the citations are about the schema's shape rather than the 31 GB beside it.

**All eight datasets are staged** at `/mnt/nas/joe/tessera/datasets/<name>/<vintage>/`, 2.5 TB, each
with a README stating what was verified at acquisition and what is the publisher's claim. **A ninth
was added 2026-09-01** — `mesh/2025/`, rung 3's label side, on the same convention (§4.1).

**`data/` is already mirrored** to `arxiv-tessera/2026-07-27/`, so the plan's §5 cleanup is a
verification rather than a copy. It has **not** been verified and nothing has been deleted; there is
no space pressure at rung 1 (117 GB free, GeoNames needs ~5 GB end to end).

**The second volume (plan §4) is not built,** and rung 6 no longer obviously needs one. Archiving the three rungs'
`staging/` intermediates and the whole of `paperseek/` to the share — verified file for file and byte for byte, at
`/mnt/nas/joe/tessera/derived-archive/` — took this volume from 202 GB free to **366 GB**, against rung 6's modelled
~304 GB. `target/debug` is a further 109 GB if the transient needs it. Restoring rung 4 is now a copy off the share
at a measured 39.7 MB/s, not an instant rebuild.

<!-- campaign-table: generated by scripts/campaign_report.py — do not edit by hand -->

### 1.1 The campaign table

**Generated** from `test_corpora/<rung>/measurements.json` by
[`../scripts/campaign_report.py`](../scripts/campaign_report.py) — every figure here is a
field of a committed file, and the schema those files are on is
[`../test_corpora/common/README.md`](../test_corpora/common/README.md). Do not edit this
block; re-run the script.

**Build.** Wall and peak are the process's own, on local NVMe; `peak` is the highest
`VmHWM` any stage reported, and `slowest stage` is from the same record.

| rung | rows | build wall | peak RSS | bundle | slowest stage |
|---|---|---|---|---|---|
| medcpt | 35,920,666 | 15:13 | 11.85 GB | 11.15 GB | layers 6:52 |
| paperseek | 102,117,343 | 67:46 | 20.54 GB | 70.78 GB | text_index 28:08 |
| treeoflife | 233,055,986 | 70:33 | 37.86 GB | 40.01 GB | filter_postings 35:25 |

**Serve.** One row per (cap, principal). `measured` is a zoom-0 whole-extent viewport
under that principal over the same under the 100% principal — the *target* is what the
greedy term composition aimed at. The three latency columns are the median **cell**'s
median under each condition, end to end; the last is the median cell's server-side p99.

| rung | cap | target | measured | terms | authorise | first viewport | cold | cold pages | hot | hot server p99 |
|---|---|---|---|---|---|---|---|---|---|---|
| medcpt | uncapped | 1% | 2.86% | 2 | 5.50 ms | 256.20 ms | 1.04 s | 23.47 ms | 3.09 ms | 2.32 ms |
| medcpt | uncapped | 5% | 2.86% | 2 | 0.80 ms | 1.26 s | 1.06 s | 26.03 ms | 6.80 ms | 1.81 ms |
| medcpt | uncapped | 10% | 9.66% | 2 | 7.80 ms | 1.73 s | 1.11 s | 31.00 ms | 9.08 ms | 2.24 ms |
| medcpt | uncapped | 25% | 24.74% | 3 | 10.10 ms | 1.84 s | 1.17 s | 28.79 ms | 9.31 ms | 2.53 ms |
| medcpt | uncapped | 50% | 48.86% | 2 | 8.30 ms | 3.08 s | 1.26 s | 39.34 ms | 8.27 ms | 3.30 ms |
| medcpt | uncapped | 100% | 100.00% | 17 | 1.00 ms | 7.54 s | 1.38 s | 63.29 ms | 14.03 ms | 2.02 ms |
| medcpt | 6.44 GB | 1% | 2.86% | 2 | 5.60 ms | 3.06 s | 1.32 s | 25.32 ms | 3.36 ms | 1.97 ms |
| medcpt | 6.44 GB | 5% | 2.86% | 2 | 1.30 ms | 3.33 s | 1.31 s | 25.45 ms | 3.40 ms | 1.88 ms |
| medcpt | 6.44 GB | 10% | 9.66% | 2 | 6.60 ms | 3.09 s | 1.29 s | 31.38 ms | 8.19 ms | 2.25 ms |
| medcpt | 6.44 GB | 25% | 24.74% | 3 | 5.40 ms | 3.20 s | 1.30 s | 33.86 ms | 13.21 ms | 2.10 ms |
| medcpt | 6.44 GB | 50% | 48.86% | 2 | 6.30 ms | 4.64 s | 1.39 s | 50.29 ms | 9.77 ms | 5.32 ms |
| medcpt | 6.44 GB | 100% | 100.00% | 17 | 1.10 ms | 7.71 s | 1.47 s | 77.01 ms | 14.32 ms | 5.65 ms |
| paperseek | 25.77 GB | 0% | 0.00% | 0 | 6.10 ms | 2.40 ms | 2.68 ms | 12.19 ms | 1.16 ms | 0.13 ms |
| paperseek | 25.77 GB | 14% | 13.74% | 2 | 6.80 ms | 928.60 ms | 832.30 ms | 288.49 ms | 46.56 ms | 6.27 ms |
| paperseek | 25.77 GB | 100% | 100.00% | 11 | 1.70 ms | 7.41 s | 7.27 s | 380.35 ms | 87.00 ms | 13.42 ms |
| treeoflife | uncapped | 1% | 1.00% | 6 | 5.40 ms | 1.65 s | 99.24 ms | 58.40 ms | 1.65 ms | 8.02 ms |
| treeoflife | uncapped | 5% | 5.00% | 5 | 7.40 ms | 2.39 s | 240.20 ms | 79.33 ms | 1.71 ms | 14.42 ms |
| treeoflife | uncapped | 10% | 10.00% | 6 | 12.20 ms | 4.65 s | 164.98 ms | 55.29 ms | 1.63 ms | 7.79 ms |
| treeoflife | uncapped | 25% | 25.00% | 12 | 7.90 ms | 4.59 s | 330.63 ms | 52.72 ms | 1.61 ms | 7.77 ms |
| treeoflife | uncapped | 50% | 42.14% | 473 | 9.00 ms | 6.10 s | 438.85 ms | 53.95 ms | 1.66 ms | 14.02 ms |
| treeoflife | uncapped | 100% | 100.00% | 474 | 3.30 ms | 28.23 s | 1.14 s | 56.80 ms | 1.71 ms | 8.91 ms |
| treeoflife | 25.77 GB | 1% | 1.00% | 6 | 5.70 ms | 2.16 s | 71.96 ms | 20.02 ms | 1.72 ms | 1.95 ms |
| treeoflife | 25.77 GB | 5% | 5.00% | 5 | 7.30 ms | 3.64 s | 104.56 ms | 17.64 ms | 1.66 ms | 2.51 ms |
| treeoflife | 25.77 GB | 10% | 10.00% | 6 | 9.70 ms | 5.11 s | 139.63 ms | 21.12 ms | 1.78 ms | 2.26 ms |
| treeoflife | 25.77 GB | 25% | 25.00% | 12 | 8.20 ms | 4.77 s | 317.35 ms | 20.37 ms | 1.69 ms | 4.01 ms |
| treeoflife | 25.77 GB | 50% | 42.14% | 473 | 10.80 ms | 6.71 s | 420.66 ms | 20.51 ms | 1.72 ms | 2.33 ms |
| treeoflife | 25.77 GB | 100% | 100.00% | 474 | 3.40 ms | 26.40 s | 1.23 s | 19.55 ms | 1.65 ms | 3.21 ms |

**Ingest.** *f* of the entities held back, the complement built, the hold-out ingested
online. `visibility` is when a zoom-0 viewport reached the expected count after the
flush, not the flush's own wall; `fold` is the server's own `compaction.last_secs`;
`driver peak` is the driver process's own `VmHWM` over the whole cell, the harness's
cost beside the server and not the server's (`—` where the cell predates the field).

| rung | f | C | items/s | ack p50 | ack p99 | visibility | fold | driver peak | 0091 equivalence |
|---|---|---|---|---|---|---|---|---|---|
| medcpt | 100% | 8 | 11,267.0 | 3.07 s | 16.62 s | 1.2 s | 1:43 | — | zoom-0 exact, layers 6 |
| medcpt | 50% | 8 | 10,746.0 | 2.58 s | 19.20 s | 0.8 s | 1:56 | — | zoom-0 exact, layers 6 |
| medcpt | 10% | 8 | 65,194.8 | 1.19 s | 2.85 s | 0.4 s | 2:00 | — | zoom-0 exact, layers 6 |
| treeoflife | 50% | 8 | 11,059.8 | 5.99 s | 15.13 s | 15:02 | 21:53 | — | 22 difference(s) |

<!-- /campaign-table -->

## 2. Rung 1 — GeoNames, against §7.1's bar

The plan's bar for *done* is six things. Two are met.

| | |
|---|---|
| ✅ declaration passes `tessera check` | 6 sources, 1 view, 8 vocabularies, 13 attributes, 2 layers |
| ✅ bundle exists, frame report recorded | 1,341,841,220 bytes; the build's own frame report, which now names the projection and the snap |
| ❌ decision 0091's build-vs-ingest test on real data | not attempted |
| ❌ masked-count census exact against an oracle | not attempted |
| ❌ one full write cycle (suppress → delete → re-ingest → fold → re-census) | not attempted |
| ⚠️ a results row | build wall, peak RSS and bundle bytes yes; **ingest rows/s, p99 at three zooms and a screenshot all absent** |

**Figures**, local NVMe, 47 GB machine, no `--memory-budget` set, 2026-08-30 on the declared
projection:

```
prepare.py       2:54            tessera build   2:59 wall, 3.55 GB peak RSS
bundle           1.34 GB         verify          1.03 s
                 99.7 B/point    artifacts       688 minted, 464,655 declared
resolution       85.7% of points have a cell of their own — 11,544,034 distinct cells
```

⊘ **The wall and the peak are not comparable with the 6:05 and 4.2 GB of 2026-08-28**: the mapped
attribute columns and the split text index (§3.1) landed between the two runs, and neither is
anything to do with the projection. The **bundle** is comparable, and it differs by **176 bytes**
across 1.34 GB — compression deltas on files whose contents shifted by a few low-order position
bits.

Neither wall the plan expects — W1's Roaring round trip at 5×10⁷ members, W2's peak RSS ignoring
its budget — is near being reached at this scale.

**What the rung is served by:** [`../test_corpora/geonames/`](../test_corpora/geonames/README.md),
which carries the preprocessing, the declaration and the full account of what the source turned out
to be.

## 3. Rung 2 — Overture, built

[`../test_corpora/overture/`](../test_corpora/overture/README.md) carries the declaration, the
pipeline and the full survey. **The whole corpus is built and verified**, 73,631,092 places.

**Built, verified, and built again to prove the optimisation below changed nothing** — and rebuilt
on 2026-08-30 on a declared projection, which is the run below.

```
prepare.py    divisions 109 s · join 2,560 s · entity ids 1,130 s · outputs 162 s
              points.parquet 3.09 GB · members-taxonomy 293 MB · artifacts-divisions 4.62 GB
tessera check OK in 526 s, and it reports the polygon decomposition from the geometry alone
tessera build 31:18 wall · 26.75 GB peak RSS · exit 0
bundle        12,565,390,654 bytes — 170.7 B/point
verify        OK in 5.98 s — 1 partition, 1 view, 1 segment, high-water 73,631,092
artifacts     625,754 divisions, every one with a polygon · 2,097 taxonomy across 6 levels · 9 predicate
no artifact   3,285,234 taxonomy (4.5%) · 46,844 places in no division (0.06%)
resolution    12.1% — 8,895,128 distinct cells
```

⊘ **This run and the 2026-08-29 one are not the same build**, and the difference is not the
projection. That build read `boundaries/divisions` as an **enumerated** layer over
`members-divisions.parquet`; the declaration moved to a **spatial** layer over the division
polygons when the shape work landed and had never been run, so the 7.90 → 12.57 GB is the polygon
decomposition arriving — 58,595,897 interior tiles and 80,699,330 boundary cells, 1.34 GB held
before the build starts. The spatial resolution itself is 386 s of the 31:18: 261,555,158 rows
admitted from interior tiles and 90,460,123 tested one by one in boundary cells.

⊘ **The box was not idle**, two other agents building and testing on it throughout, so the wall and
the peak are upper bounds. Bytes and counts are unaffected.

⊘ **The join's artifact roster is not reproducible.** Two runs over the same staged bytes gave
625,821 and 625,754 division artifacts, differing on 1,526 and 1,459 keys — while the
lineage-depth histogram, the containing-areas histogram, the per-tier counts and the 46,844
unplaced places matched exactly. `arg_max(a.lineage, a.depth)` picks an arbitrary maximum among
equal-depth containing areas and 18.2M places sit in two or more. It is a property of the rung's
own pipeline rather than of anything Tessera does, and it means an artifact count from this rung
carries ±0.25% between runs.

**Both walls the plan expected here did not fire.**

**W1 was never approached**, and that follows from the declaration rather than from luck. It needs a
whole-corpus root cluster over 5×10⁷ members; `boundaries/divisions` is a `nested` tree whose roots
are countries, so its largest membership is the US at ~16×10⁶, and `places/taxonomy` splits 73.6M
across 14 roots. **The wall is still there and this corpus does not ask the question** — it wants a
layer that declares one root over everything.

**W2 did not fire either**: 18.9 GB peak against the 47.3 GB the artifact campaign was killed at,
with no `--memory-budget` set. Part of that is this rung's own work (§3.4): consuming `resolved`
rather than borrowing it took a whole copy of the memberships out of the peak.

⊘ **Resolution is 12.1% against GeoNames' 85.7%**, and it is the data rather than the frame — the
frame is full-world and the points span it. Places cluster into cities, so 73.6M of them land in
8.9×10⁶ distinct cells at zoom 16. State it beside any density figure from this rung.

### 3.0 Where the build's time goes, at last

`tessera build --stage-timings` was added for this (§4). Its first run charged one 615.0 s number to
`filter_postings`, which turned out to be four jobs sharing a stage name; splitting them is what
this table records. 73,631,092 points, one 23:03 run:

| stage | wall | share | peak RSS at end |
|---|---|---|---|
| `text_index` | **411.0 s** | **30%** | 18,400 MiB |
| `layers` | 335.2 s | 24% | **18,400 MiB** — the peak arrives here |
| `attribute_tail` | 226.1 s | 16% | 8,393 MiB |
| `filter_postings` | 70.3 s | 5% | 18,400 MiB |
| `record_blob` | 53.3 s | 4% | 18,400 MiB |
| `assignment` | 42.3 s | 3% | 3,419 MiB |
| `column_release` | 35.3 s | 3% | 18,400 MiB |
| `manifests` | 32.2 s | 2% | 18,400 MiB |
| `dictionary` | 30.3 s | 2% | 1,626 MiB |
| the artifact pass | 30.1 s | 2% | — |
| `geometry_read` | 28.9 s | 2% | 3,213 MiB |
| `segment_write` | 25.3 s | 2% | 18,400 MiB |
| `source_ids`, `pairs_pack`, `signature_sort`, `postings_write`, `tiler_sort` | 9.6 s total | 1% | — |

**One text column is the largest cost in the build.** `text_index` is 411.0 s over 10,508,413
distinct terms — 72% of the 615.0 s the unsplit stage reported, against the nine category columns'
70.3 s. Both investigations of that block modelled the text index at about three quarters of it
before the split was written; the measurement agrees with them, and neither could have been acted
on without it. This declaration indexes **ten** columns — nine keyword, one `text` — and the tenth
is the expensive one.

**Two days of optimisation went into `layers`, which is 24%**, because that was the stage visible
through `ps` while the build sat in it. That is the failure mode `--stage-timings` exists to end,
and it is worth stating plainly rather than filing as a lesson.

**The peak arrives in `layers`** and does not move afterwards. That is the first per-stage
attribution W2 has ever had: if `--memory-budget` is to bound peak RSS, `layers` is the stage it
must bound, and `attribute_tail` is what it climbs through to reach it. The staircase is two
structures and no more — the twelve entity-order columns add 5.0 GB at `attribute_tail`, the layer
plan adds 10.0 GB at `layers`, and every stage after the second holds both without needing to.

⊘ **The two runs are not a controlled comparison.** The 23:15.90 run predates both the stage split
and the shape-membership merge; this one carries both. The perf work between them is byte-neutral —
`SEGMENTS-0.json` differs only by `shape_held_extents` and `shape_rows_extents`, two empty fields
the merge added — so the bundle is unchanged, but the 61 s `layers` rose and the 45 s the filter
block fell are not separated from run variance and are not attributed.

### 3.1 What the text index and the mapped columns bought

Two changes followed from §3.0 and were measured **as a matched pair on an idle box, minutes apart,
against the same corpus and the same identity key**. Both bundles are 7,900,567,451 bytes: the pair
is byte-neutral at full scale, not merely at the corpus a unit test can hold.

| | baseline | merged | |
|---|---|---|---|
| wall | 21:52.36 | **15:12.08** | −30.5% |
| max RSS | 18.52 GB | **15.41 GB** | −3.11 GB |
| `text_index` | 384.5 s | **108.8 s** | **3.54×** |
| `attribute_tail` | 240.9 s | 267.7 s | +26.8 s |
| `column_release` | 28.0 s | **0.4 s** | −27.6 s |
| `attribute_tail` peak | 8,438 MiB | **5,409 MiB** | −3.0 GB |
| global peak | 18,964 MiB | **15,784 MiB** | −3.1 GB |

**Mapping the columns is free in wall-clock and worth 3.1 GB.** The cost is +26.8 s at
`attribute_tail`, where a column is filled by random scatter; the saving is −27.6 s at
`column_release`, where unlinking a file replaces dropping five gigabytes of heap. They cancel. On
the read side `filter_postings` moved +3.4% and `record_blob` −5.9%, both inside the noise below.
The 3.1 GB is anonymous memory becoming page cache the kernel may evict, which is the property that
matters: it is the difference between a smaller machine building slowly and a smaller machine being
OOM-killed.

**The text index is chunk → spill sorted runs → k-way merge**, parallel over contiguous ascending
entity ranges. Its peak is `clamp(--memory-budget/16, 128 MiB, 2 GiB)` across all workers, plus at
most 128 run readers and one merged term's list; **no term of it is a function of corpus size**, and
above 128 runs the runs merge in passes rather than exhausting file descriptors.

⊘ **A quarter of the 30.5% is not attributable.** The stages neither change went near moved by
about 115 s between the two runs — `dictionary` 63.2 → 30.8 s, `layers` 310.6 → 266.3 s,
`assignment` 43.1 → 31.0 s. That is run-to-run variance on an idle box, and it is larger than it
looks like it should be. The attributable gain is ~285 s against ~400 s observed. The 3.54× and the
3.1 GB are far outside that band; the wall figure is not, and a later run quoting 15:12 as
reproducible would be overclaiming.

**The order has changed.** `attribute_tail` (267.7 s) and `layers` (266.3 s) are now the two largest
stages and together 59% of the build; `text_index` has gone from first to fourth. **`layers` is the
whole of the peak** and is the one structure left that is unbounded by construction: about
5.07×10⁸ membership entries — six taxonomy levels at ~98% coverage plus one division level — held
twice over, once as `Vec<u64>` source ids and again as resolved entity ids, all anonymous. It is the
same postings shape the text index now solves, so the banding-and-merge machinery to bound it
exists rather than needing inventing.

### 3.2 Four things the survey corrected in the plan

Measured over the staged bytes on 2026-08-28, before anything was written.

**`hierarchies` is on `type=division`, not on `division_area`.** The plan and the staging README
both put the explicit hierarchy array on the polygons. The polygons carry `division_id` and
`subtype`; the ancestry is on the point form, so the pipeline joins the two once.

**The division subtypes are not levels, so the boundary layer is `nested` and not `tiered`.** A
division's path runs 1 to 9 entries deep and `locality` occurs at every path position from 1 to 8 —
a locality contains a locality, which is a same-level edge no ladder holds. The plan's "one tiered
layer over twelve subtype columns" is refuted by the data it names. There are also **nine subtypes
in `division_area`, not twelve**: no macroregion, macrocounty or borough polygon exists.

**Three of the plan's seven columns are not columns.** `country` is `addresses[1].country`;
`source_dataset` and `update_time` are on the one `sources` entry whose `property` is empty. The
rest of `sources` is property-level provenance, and counting it makes `Overture` look like the
dataset every place came from.

**`basic_category` is a rollup, not the leaf** — 278 values against 1,847, an ancestor inside the
same path. So the rung declares three category columns over one tree rather than one.

**And one thing the survey confirmed rather than corrected: there is no polyhierarchy.** All
4,658,700 divisions carry exactly one hierarchy path, asserted at every run. The polyhierarchy the
campaign expects to force a ruling is still MeSH at rung 3.

### 3.3 The predicate layer works, and the build's report said it did not

Recorded because the report cost an hour, not because anything was broken. `programmes/source` is
`membership = { attribute = "source_dataset" }`, the tagged-programme case the plan asks for. The
build's artifact-pass report printed

```
programmes/source level 0 [world]: 0 artifact(s), 0.000 everywhere, 0.0 blocks/artifact
```

and it read as a layer that is declared, reachable and serving nothing. **It is not.** The manifest
carries nine artifacts for it and a served viewport returns all nine with masked counts, beside
13 taxonomy and 8,448 division artifacts, over a three-country principal at zoom 0.

**Why the zeros are honest and the line was not.** The pass observes a level by walking its
*stored* Roaring memberships. An attribute predicate has none — its members are the value column,
evaluated per request — so the walk finds no rows and every figure in `LevelShape` comes back zero
for a level that holds its artifacts and serves them. `artifact_pass.rs` already says as much where
it declines to write such a level a row-major column; the report a line above did not.

**Fixed** — the report now prints the registry's count and says the shape is not observed:

```
programmes/source level 0 [world]: 8 artifact(s) from its column — served column; no spread to
observe, the membership being the column rather than a stored bitmap
```

and `a_predicate_over_a_category_column_mints_its_values` covers the case. The existing test for
this path, `a_build_mints_an_attribute_predicates_artifacts_from_its_column`, reads a bare indexed
`u32` and asserts the level's *version* rather than its count — so it would have passed whether or
not any artifact existed. The new one asserts the count.

⊘ **`test_corpora/overture/corpus.toml` is still the only declaration in the repository that uses
an attribute membership**, and this is what that costs: the kind's only end-to-end exercise is the
one a rung brought.

### 3.4 What the rung cost the build's own code

Eight changes, all behaviour-neutral and all proved so on the corpus itself: the rebuild's
`SEGMENTS-0.json` digest is **identical** and its `MANIFEST.json` differs in `created_at` and
nothing else. Wall time **52:01 → 23:16–26:12**.

The two that mattered were quadratics, and both were invisible to every existing test:

- **`detect_cycles` recomputed a loop-invariant bound by scanning the whole artifact map per
  artifact** (`layers.rs`). A `nested` layer puts every artifact at level 0, so that scan is the
  whole level every time — 3.6×10¹¹ key visits at 600,000 divisions. **A `tiered` corpus skips the
  function entirely**, which is why GeoNames never showed it and why nothing caught it.
- **`prepare_publish`'s `batch_ordinal` linear-scanned the batch per parent lookup**
  (`registry.rs`), and `parent_ref` asks it before the store. Now indexed once. The same quadratic
  is on the ingest and control planes, which share the function.

The rest: the ancestor walk replaced with a colour-marking pass (one visit per artifact, and it
removes a latent hang — a corpus that genuinely held a cycle ran the bound's full length for every
artifact whose lineage reached it); `verify_hierarchies` hoisted above the publish loop so
`resolved` is consumed rather than borrowed; **text member keys interned** into a plan arena, which
turned three `BTreeMap<(String,u32,String)>` probes and two `String` allocations per member entry
into one hash probe over 3×10⁸ entries; the containment pass's two `HashSet<u64>` replaced by a
sorted array and a coverage bitset walked with a galloping cursor — 2.5 MB where a country-sized
parent needed two ~300 MB tables; `Permutation::project`'s 512 KB scratch hoisted out of a
per-artifact call (the artifact pass 84.9 s → 23.1 s); the `resolve_artifact` map parallelised; and
the `keys` and `shapes` indexes nested so their lookups borrow.

⊘ **Peak RSS rose slightly**, 18.83 → 19.37 GB. The parallel map holds several resolutions in
flight where the serial loop held one, and the containment pass's saving did not quite offset it.

⊘ **A report defect, found the slow way.** The artifact pass printed `0 artifact(s), 0.000
everywhere` for `programmes/source` — a layer holding 8 artifacts and serving all 8. The walk
observes *stored* memberships and an attribute predicate has none, its members being the value
column. An hour went into looking for a defect in a working layer. The report now prints the
registry's count and says the shape is not observed.

### 3.5 What is still open at this rung

- **A `nested` layer has no levels, so it has no zoom bound** — the other half of §5's first
  finding, at roughly 600,000 artifacts and with no zoom-to-level map to offer. Named in the
  declaration at the layer it applies to.
- The rung declares both a `nested` boundary layer and three indexed division columns, so the
  attribute-membership comparison the plan asks for is one declaration away. It has not been run.
- **`layers` is now the largest structure and the whole of the peak** (§3.1). It holds about
  5.07×10⁸ membership entries twice over, all anonymous and all a function of corpus size, so it is
  the one part of the build that fails the ingest-beyond-memory test outright. The text index's
  banding-and-merge machinery is the shape that answers it.
- §7.1's bar: the 0091 build-vs-ingest test, the oracle census, the write cycle, ingest rows/s, p99
  at three zooms and a screenshot. None attempted.
- **The roster's ±0.25% run-to-run drift** (§3), which is `prepare.py`'s tie-break and not
  Tessera's, and which nothing yet needs to be stable.
- The **spatial** boundary layer has been built but never served. 386 s of the build goes into
  resolving 73.6M rows against 625,754 polygons, and what that costs a request is unmeasured.

## 4. Rung 3 — MedCPT / PubMed, built

**Built, verified and served 2026-09-02** — [`../test_corpora/medcpt/`](../test_corpora/medcpt/README.md),
which carries every figure with its medium. §4.1–§4.5 below are the survey that preceded it, kept
because they record what was corrected in the plan and why the layer has the shape it has; **§4.6 is
the outcome**, and where the two differ the outcome wins.

Surveyed 2026-09-01 over the staged bytes, before anything was written. What the survey settled is
what the rung is made of, which of the plan's prerequisites are real, and the shape of its artifact
layer.

**35,920,666 rows**, counted from the 38 `.npy` headers rather than inferred from the chunk list —
768-dimensional `float32`, 105 GB. The plan's 3.6×10⁷ is right.

### 4.1 Three things the survey corrected in the plan

**The 51.8 GB PubMed baseline is not a prerequisite.** The plan (§9.3) says acquiring it and
extracting `(pmid, descriptor, tree_numbers)` is "a prerequisite, not a step". It is neither: the
staged `pubmed_chunk_N.json` files already carry, per PMID, the date, the title, **the abstract**
and **the MeSH descriptors** with their qualifiers and major-topic flags. The baseline is now worth
its 51.8 GB only for `journal` and `publication_type`, two of the three rendered columns the plan
named, and that is a scope choice rather than a gate.

**What was actually missing is the MeSH tree, and it is 2.7 MB.** The chunks name descriptors; they
do not say where a descriptor sits. `mtrees2025.bin` is the NLM's flat `Descriptor Name;TreeNumber`
file and it is the whole of the structure. Acquired 2026-09-01 to
`/mnt/nas/joe/tessera/datasets/mesh/2025/`, with its own README carrying the counts below and the
join's cost. The 2026 vintage is not published at that path; 2025 already post-dates the corpus.

**Abstracts are staged and free to read.** The plan defers them to rung 4 as the forcing case for
the streaming text column, on the reasoning that they would have to be joined from the baseline.
They are in the chunks — roughly 30 GB of strings at 36M rows — so whether rung 3 forces that work a
rung early is now a decision rather than an acquisition.

### 4.2 The polyhierarchy is not what blocks the layer

The plan expects rung 3 to force a ruling on MeSH's polyhierarchy, marks it **blocking** in §7.1,
and lists it in §8 as one of the things the campaign will break. Measured, that is not where the
rung stops. Two mismatches were found and they are independent.

| | Measured | |
|---|---|---|
| **An article is in many concepts** | mean **10.6** descriptors, median 10, max 48 · 3.6 of them major topics | the blocker |
| **A concept is at many positions** | **52.9%** of 30,954 descriptors carry more than one tree number, up to 24 · 2,633 span more than one top-level branch | not the blocker |

**Keyed by tree number, MeSH is a strict tree.** 64,883 nodes, 115 roots across 16 branches, depth
13, every node's parent its own dotted prefix and **zero** nodes whose prefix is absent. So the
two-parents refusal (`artifacts-from-points.md` §4) need never fire, and the case the plan expected
to argue about dissolves without a surface change.

**What stops the rung is multi-membership.** A member source is one row per point, and the only
hierarchy kind that reads a list as plain multi-membership is `flat`, which carries no edges.
`tiered` wants a fixed list of one entry per level; `nested` wants a single lineage. Neither can say
*this article is in ten concepts*, and that is true of the flat spelling of the layer as well — it is
not a property of the hierarchy at all. **A levelled kind admitting several member rows for one
point is the change this rung requires**, and it is required under every option below.

### 4.3 The layer's shape — an owner ruling, 2026-09-01

Four routes were put up; the ruling is **key the artifacts by descriptor and let a child name
several parents**, which makes the layer a **DAG** rather than a tree. So two surface changes are
needed rather than one: multi-membership under a levelled kind, and a hierarchy that is declared as
a DAG. ⊘ **Both were unbuilt when this was written; both now exist** — the levelled-kind change
turned out not to be a change at all ([`design/dag-hierarchies.md`](design/dag-hierarchies.md) §2,
and §8 below), and `kind = "dag"` is built and carried a 1.66×10⁹-entry membership at §4.6.

**Why not key by tree number**, which would have cost nothing. Because the duplication cascades. A
polyhierarchical concept's *descendants* are duplicated with it — `Respiratory Tract Neoplasms` is
itself at two positions, so everything under it appears twice — and 30,954 concepts become 64,883
artifacts. A client browsing that sees one concept, with one count, in several places, with nothing
on the wire to say it is one thing. It remains the cheap fallback if the DAG is not taken.

**A DAG corrupts no count, and that was checked rather than assumed.** The number beside a served
artifact is always the masked count of the artifact's **own declared membership** (`annotations.md`
§3), never a sum over children; roll-up within a level is *substitution* of a parent for its
children rather than aggregation (decision 0087);
and containment is verified one intersection per edge, so a concept need only be a subset of each of
its parents, which it is. The two-parents refusal is there because ambiguous data is not the tree the
layer *declared* — a layer declaring a DAG is not ambiguous, and this one would be declaring the
shape the NLM publishes.

### 4.4 Two coverage figures that must travel with every number from this rung

Both are properties of the source and neither is repairable by preparation.

**MeSH coverage runs with time, and the chunks are in PMID order.** Indexing lags publication:

| chunk | articles | with MeSH | with abstract |
|---|---|---|---|
| 0 (1975–1979) | 977,492 | **100.0%** | 43.5% |
| 18 (to 2009) | 940,707 | 86.8% | 69.3% |
| 37 (to 2023) | 380,761 | **37.5%** | 87.0% |

A whole-corpus MeSH figure is a weighted average over a strong trend, and abstract coverage runs the
opposite way. ⊘ Neither was measured over all 38 chunks; three were read.

⊘ **5.87% of descriptor mentions do not resolve against the 2025 vintage, and the miss is not
random.** 89 descriptors carry all of it — headings the NLM has since retired or renamed, weighted
towards the ancestry and ethnicity terms revised in 2022–23 (`african americans`,
`asian continental ancestry group`). The articles were indexed against the MeSH of their year,
running back to 1975; the file is one vintage. Measured over chunk 18: 25,907 distinct descriptors
seen, 89 unresolved (0.3% of distinct, 5.87% of mentions). Dropping them drops a slice with a
subject. The repair, if one is wanted, is the NLM's replacement-terms file and not a fuzzy match.
**Ruled 2026-09-01: dropped, and said so** — the rung is a demonstrator and the slice is stated
rather than repaired.

### 4.5 The projection experiment — run, folded into the arXiv rung, and re-scoped

Plan §6.2 proposed building the arXiv geometry both ways — full-dimension cosine kNN into UMAP
against the shipped PCA-64 route — and judging which distorts the geometry less. It was run over
all 2,422,486 papers on 2026-09-01, as two views of one entity space (owner direction), both on
cuML's GPU UMAP ("this is a demonstrator; speed wins over accuracy"). **Then the question was
re-scoped by the owner**: the ladder's corpora are demos and speed benchmarks for Tessera, the
layout exists to make a useful view, and how faithfully UMAP preserves neighbourhoods is not a
question this campaign asks. The recall and purity apparatus built to answer it was deleted.

What survives is what bears on Tessera. **The kNN route is the pipeline for the larger rungs**:
CAGRA in fp16 builds the graph over 2.4×10⁶ × 1024 in about a minute on a 10 GB card, cuML lays it
out in under half a minute, and the whole route is **3× faster** than PCA-then-UMAP (94 s against
280 s on an idle box) — reducing to 64 dimensions leaves UMAP a slower graph to build than the card
had already built in full dimension. **And the layout decides the serving cost of every artifact
over it**: in the `knn` view a cluster is 3.5–4.1 contiguous row runs and most of each level sits
under tile-index nodes (0.22–0.33 "everywhere"); in `pca64` the same clusters are 27–29 runs each
and every one is "everywhere" — served on every request at the full masked probe. That is a
property of the structure the engine serves from, visible only because the two layouts are two
views over one membership, and it is why `knn` is the anchor.

**The arXiv rung now carries the two views** — `knn` (*Topic map*) and `pca64` — both clustering
layers on both, titles, abstracts and authors indexed, dates filterable, and each cluster titled by
its own c-TF-IDF text as supplied content. Whole corpus: `prepare.py` 13 m 0 s at 22.9 GB peak,
`tessera build` 54.5 s to a 1.5 GB bundle, `verify --deep` clean; the rung README carries the
build's own per-view report verbatim.

**A layer earns its place by drawing something in the view it is declared over** (the ruling that
withdrew Overture's taxonomy, §3), and the arXiv taxonomy failed the same test on 2026-09-01.
Measured in the `knn` view, the box holding the middle 90% of an artifact's members as a share of
the map: k-means median **1.0%** and HDBSCAN **0.3%**, 94% of each under 5%; the taxonomy's
archives median **13.7%** and subject classes **9.6%**, with `hep-th` and `gr-qc` at 34% and
`physics.hist-ph` at 64%. 97–98% of its 209 artifacts were "everywhere" — served on every viewport
for outlines that draw nothing, while `archive` and `primary_category` already give the same
information as colour and filter. **Withdrawn** (owner ruling); the two indexed columns stay.

⊘ **The `knn` route is not reproducible under a seed** — CAGRA's index build takes none, so UMAP is
handed a different graph each run and the HDBSCAN tree differs with it (186, 192 and 200 clusters
across three runs); `pca64` reproduces bit for bit. Stated at the claim in the rung.

⊘ **The viewer cannot show the second view.** `clients/ts/viewer/src/main.ts` takes `meta.views[0]`
in three places and no selector exists. The design for holding and switching between views is
`design/view-switching.md`, on branch `client/view-switching` with its implementation tracks, not
yet merged.

**`run_demo.sh` wrote into `clients/ts/`** — 5.5 GB of bundles, WAL and cache under `.dev/`, the
viewer's `public/datasets.json`, an `.env.local` — and held port 5173, so two sessions on one
checkout overwrote each other's demo. **Ruled 2026-09-01, and done the same day**: everything it
produces is under `./tessera-demo/` in the checkout (`TESSERA_DEMO_DIR` moves it), gitignored; the
viewer is handed its dataset list by the URL the script prints (`?datasets=/@fs/<path>`, served
through Vite's `fs.allow`) and its session credential through the environment of the `npm run dev`
process; and `VITE_PORT` chooses the viewer's port, which is the one written into every
`dev_cors_origins` the script generates.

### 4.6 Built — what it cost and what it found

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080 with ~8.2 GB free)
unless the medium says otherwise. The staging pass is the one **network-source** figure.

**Staging is a step here, and it is the only one on the ladder.** 163 GB of publisher bytes over
SMB is ~40 minutes a pass, so `stage.py` makes exactly one: **60.5 minutes**, 15.0 GB peak RSS,
writing 67 GB locally — one parquet per chunk and a flat `(35_920_666, 768)` float16 memmap with a
sidecar that refuses a partial matrix rather than reading a sparse file's zeros. Measured over all
38 chunks, which §4.4's table extrapolated from three: MeSH **84.9%**, abstracts **68.9%**, 27,957
unparseable dates (0.078%, nulled and counted), no zero-norm vectors.

**The route changed, and the change was measured first.** The arXiv `knn` route puts the whole
matrix on the card; here it is 55 GB. cuML's UMAP over a precomputed graph peaked at 1,417 bytes a
row at 2×10⁶ and 1,283 at 2.5×10⁶, so the whole corpus is **~46 GB of device memory** for the
layout alone — six times the card and past the host RAM managed memory would oversubscribe into, so
no managed run was attempted. Taken instead: fit UMAP on a uniform **2.5×10⁶** rows through one
CAGRA index, then place every other row at the similarity-weighted mean of its 15 fit-set
neighbours against a second index over the same set. ⊘ The index is built twice because it cannot
be held across the layout — 5.46 GB and 2.99 GB against ~8.2 GB free. Sharded CAGRA over all 36M is
in the code, unused, and unmeasured at scale.

| | |
|---|---|
| `prepare.py --sample 0` | **17 m 27 s**, **43.3 GB peak RSS** — route 480 s (CAGRA build 18.1 s, search 22.1 s at 111,872 q/s, UMAP 36.6 s, placement of 35,920,666 rows 361 s), MeSH 421 s, k-means 22 s, titles 25 s |
| `tessera build` | **12 m 10 s**, **16.03 GB peak RSS**, **11.15 GB bundle**, 165,272,740 pairs, 90.6% of points with a cell of their own, none on the frame's edge |
| `tessera verify --deep` | clean in **5.1 s** at 1.15 GB — 1 partition, 1 view, 1 segment, 35,920,666 rows |
| served | `run_demo.sh` on its own deployment; principals 4,910 / 4,910 / 6,024,843 / 25,357,425 / 35,920,666 visible |

**The rung's scaling finding: 1,658,437,807 closed membership entries against rung 2's 5.07×10⁸ —
3.27×**, where [`design/dag-hierarchies.md`](design/dag-hierarchies.md) §8 extrapolated 3.4× from
chunk 18 alone. It fits: 2.75 GB of member parquet inside an 11.15 GB bundle. **Neither W1 nor W2
fired.** No artifact reaches W1's 5×10⁷-member Roaring round trip — a closed MeSH root is bounded by
the 3.05×10⁷ indexed articles — and W2's OOM did not happen, the build peaking at a third of the
box. So §8's fallback, explicit assignments with the containment report beside every figure, is not
needed and was not taken. The DAG's own shape in the built layer: 30,217 descriptors with members,
41,321 edges, 9,095 with more than one parent, 107 roots, at most 6 parents.

**Both layers draw, so neither is withdrawn.** The measure is the one that withdrew the taxonomies
of rungs 1 and 2 — the box holding the middle 90% of an artifact's members as a share of the map,
in the layout the layer is declared over:

| | median | p90 | max | under 5% |
|---|---|---|---|---|
| `clusters/kmeans` (all 256) | **0.04%** | 0.15% | 0.83% | 100% |
| `mesh/descriptors` (150 sampled) | **1.4%** | 5.1% | 7.9% | 88% |
| *withdrawn for comparison:* arXiv archives · Overture taxonomy | 13.7% · 9.6% | | | |

⊘ **That is not the build's `everywhere` fraction**, which is 0.180 for the clustering and **0.984**
for the DAG. A box covering 1.4% of the map is still wider than a tile-index node at the depth the
level is served from, so nearly every descriptor is served as a list rather than bounded by a node,
at 241.4 contiguous row runs each against the clustering's 7.3. Compactness in the map and
boundability in row space are different properties and this rung is the first corpus to separate
them.

**The abstracts ruling stays open, and now has numbers** (§8). The 10⁶-row sample was built both
ways: `points.parquet` 118.6 → 634.6 MB, `tessera build` 19.7 → 34.3 s, **build peak RSS 716 MB →
2,246 MB**, bundle 333 → 799 MB. Linearly ×36 that is a 28.7 GB bundle and ~81 GB of build RSS on a
47 GB box — modelled, not measured, and W2 says the peak is not bounded by `--memory-budget`, so it
is a wall to meet rather than a refusal to expect.

⊘ **Three distinct, non-reproducing, localised faults in one evening on this host, and none is
attributed to the code.** The first whole-corpus build's containment report named 56 edges holding
**45 member rows of 1.66×10⁹** under the wrong article, all inside a 35-wide window of consecutive
entities, each article losing its highest-id descriptors to the next with totals preserved. A second
whole-corpus run on the same code and the same staged input has that window **correct** and fails
elsewhere and differently — one escaping member on an edge the first run had right, where a single
entity is **missing two ancestor rows** rather than having any shifted, which is also why the two
runs' membership totals differ by three. That run's build died of `SIGSEGV` after 3 m 12 s
(`error 6`, a write to a non-present page) and then, relaunched on the same binary and inputs with
the bundle directory cleared, **built cleanly**: 12 m 42 s, 16.07 GB, 11,152,157,764 bytes,
`verify --deep` OK, hierarchy identical in shape. Two of the three are in Python/NumPy and one in
the Rust build; **each run is otherwise bit-consistent with a recompute**; five candidate code paths
are excluded with numbers in the rung README. Recorded as a **host fault, ⊘ not proven** — the
action is a memtest (§8), not more detection machinery.

## 4a. Rung 4 — PaperSeek + OpenAlex: prepared whole, built at 10⁷, stalled at 10⁸

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The rung is [`../test_corpora/paperseek/`](../test_corpora/paperseek/README.md), which
carries the per-step tables, and the raw output is
[`../probes/2026-09-03-rung-4-build-stall/`](../probes/2026-09-03-rung-4-build-stall/README.md);
this section is what the campaign takes from them.

**The rung was chosen to put a bundle past the box's memory**, and nothing in the declaration was
trimmed to make it fit — the abstracts are 118.9 GB of characters uncompressed and are indexed as
text. That decision is what the rung measured, and the answer arrived one stage earlier than
expected: **it is the build, not the server, that meets the wall.**

**Two tracks, and the interface between them held.** The vectors track staged, laid out and wrote
the corpus; the OpenAlex track produced the extract, the topic tree and the licence resolve. Neither
waited on the other and the merge was clean.

| | |
|---|---|
| staging | **164.7 min** over SMB, 22.2 GB peak, 254 GB written locally (195 GiB of `float16` vectors, 59 GB of per-chunk parquet). ⊘ Not comparable with rung 3's 60.5 min — the OpenAlex track's own scan of `works` shared the share for half of it |
| `prepare.py --sample 0` | **43.8 min**, **18.44 GB** peak — route 1,487 s (1,208 s placing 102,117,343 rows against a 1.5M-row fit set), the one streaming pass 980 s at a flat 18.4 GB |
| `tessera build` | **10,578.4 s (2 h 56 m) to a 70.78 GB bundle** over the whole corpus, 2026-09-04 — §4b. **545 s to a 7.44 GB bundle** at a 10⁷ prefix |
| the 10⁶ sample, end to end | prepare 601 s at 12.53 GB · build **23.3 s** to **744.3 MB**, anonymous high-water **968 MB** against 2,398 MB of `VmHWM` · `verify --deep` clean in 0.33 s at 52.5 MB · served, driven, counts move with the mask |

**The compartment is the first on the ladder that is a property of the row.** GeoNames and Overture
compartment on a country of convenience and MedCPT on the branch letters of an indexing vocabulary;
a work's licence is a rights fact about the work. **Owner ruling 2026-09-03:** a work with no
licence carries `unlicensed`, an eleventh key of the closed vocabulary, rather than no term and the
view's `public` default — the campaign's principal ladder starts at 1% and cannot be composed under
a 77% floor every principal would hold for free. The ladder is then **0 / 14,028,593 /
102,117,343** across no terms, `cc-by` and all eleven keys.

### The build stalls in the abstract text index, and the mechanism is measured

`tessera build --stage-timings` got through every stage before the text index and then stopped
making useful progress. It was **neither refused nor killed** — no OOM, no pre-flight refusal, no
signal; it is stalled on I/O.

| stage | wall | `VmHWM` |
|---|---|---|
| `source_ids` … `external_ids` | 47 s total | 6,274 MiB |
| `attribute_tail` | 759.6 s | **24,409 MiB** |
| `layers` | 84.5 s | 24,409 MiB |
| `text_index` | **> 4 h and counting** | — |

⊘ **The box was not quiet.** Another track's 3.2×10⁷-row MedCPT base build ran on the same disk
from 03:48 and two serve batteries were driving cgroup `memory.reclaim` eviction beside it, for most
of the 03:13–06:25 window. The mechanism is not in doubt; **the rates and the wall include
contention** and are not this build's cost alone.

Sampled four hours in: **11 of 13 threads in uninterruptible sleep on `folio_wait_bit_common`**,
**93% of CPU in the kernel**, **~480 major faults a second**, PSI reporting the process group
**fully stalled on I/O 60.8% of the time** — and an **anonymous high-water of 5.19 GB**. `title`'s
text index finished, at 2.4 GB; the abstract column's does not.

**What binds is the file the design maps, not the heap a budget models.** The build preallocates one
arena per text column: `.build-tmp/column-13.arena` is **137,438,953,472 bytes — 128 GiB exactly** —
against 47 GB of RAM. The text pass walks it and the page cache cannot hold enough of it, so nearly
every access is a major fault. `--memory-budget` cannot reach this, the anonymous figure being a
tenth of the box.

⊘ **This is W2 arriving in a shape the campaign did not name.** W2 is an OOM the pre-flight should
refuse; what happened is neither — the build stays well inside memory and stops progressing. The
first two walls fired at neither rung 2 nor rung 3, and this is the first time either has been met
at all.

⊘ **[`../probes/2026-09-02-text-peak-split/`](../probes/2026-09-02-text-peak-split/README.md)
extrapolated the right quantity and could not have predicted this.** Its ~6 GB for the abstracts'
own anonymous share at 10⁸ is close to the 5.19 GB measured — which is exactly why it does not
predict the stall. It measured to 10⁷, where the arena is ~13 GiB and fits, and the wall it named as
"a disk question and a wall-clock question, not a memory one" is a **page-cache** question, which is
neither of the two it separated. §8's abstracts entry should be read with that correction.

**Not patched.** No `--memory-budget` arm was tried, the declaration was not trimmed and the
abstracts were not dropped: each answers a different question from the one the rung was built to
ask. What to do about it is the owner's, and the options are visibly (a) a budget arm, (b) an arena
the text pass streams rather than maps, (c) a smaller corpus, (d) more RAM. **(b) was then built and
measured**: [`../probes/2026-09-03-text-arena-streaming/`](../probes/2026-09-03-text-arena-streaming/README.md)
reproduces the stall in isolation at 10⁷ under a 4 GB cap, attributes it to an arena walk that is
uniformly random rather than sequential, and takes this rung's own `text_index` from *over four
hours without finishing* to **2,371.9 s** at 1.74 major faults a second and byte-identical output —
**and the build still did not complete**, because `record_blob` reads the same arena by entity and
cannot take the same fix: the blob's rows *are* entity order.

**Then the other half, and it found a third random walk.** Owner ruling 2026-09-03 took option (a)
of that probe's own question — an arena filled in **entity** order by a second decode of the
source's text column, when the columns' Parquet payload exceeds half the memory budget
(`--arena-order auto|entity|arrival`, `crate::ArenaOrder`). That is built, and building it exposed
that the join's **scatter** was uniformly random for the same reason the arena walk was: the sweep
resolves in source-id order and entity ids are signature-then-Morton. At 10⁷ under a 4 GB cap,
sorting the scatter ascending takes `record_blob` from **> 2,134 s unfinished** — 2,348 GiB read for
a 10.2 GiB arena, 85.5 MB of a 4.4 GB `blocks.bin` written — to **64.31 s**, which is its uncapped
wall, *in arrival order*. In entity order it is 60.72 s. The second decode costs 2.04× the join
(32.45 s → 66.18 s uncapped). Both orders build byte-identical bundles at 10⁶ and 10⁷, against each
other and against the code before either change.
[`../probes/2026-09-03-entity-ordered-arena/`](../probes/2026-09-03-entity-ordered-arena/README.md).

**And then the 10⁸ build completed.** 2026-09-04, on 448 GB of free disk — the pre-flight had
refused it 16.3 GiB short until another session's 209 GB `target/debug` was reclaimed — over the
whole 102,117,343-row corpus with the same declaration that stalled, abstracts indexed, nothing
trimmed. **2 h 56 m 18 s to a 70.78 GB bundle**, `verify --deep` clean, served under a 24 GiB cap.
§4b.

### The bracket at 10⁷, and everything the rung could still prove

The same inputs built as a prefix (`--limit 10000000`, member files and artifact rosters cut to
match) say where the turn is: at 10⁷ the abstract arena is ~13 GiB, fits in page cache, and the text
index that would not finish at 10⁸ takes **178 seconds**.

| | |
|---|---|
| `tessera build --limit 10000000` | **545 s**, **7.44 GB** bundle, anonymous high-water **1,685 MB** against 15,705 MB of `VmHWM` |
| `verify --deep` | clean in **5.45 s** at 342 MB |
| bundle | `attrs` 6.6 GB — `record` 4.4 GB, `abstract` 1.8 GB, `title` 242 MB — `views` 286 MB, `entities` 77 MB, `members` 54 MB. **Two thirds of it is prose**, which scales to a **~76 GB bundle** at 10⁸ |
| served, uncapped and under `MemoryMax=24G` | **survives, and the cap never binds**: `oom 0`, `oom_kill 0`, scope `memory.peak` **242.5 MB**, and **85 of 85 count-bearing responses identical** between the two runs. p50 0.05–9.3 ms across every request kind either way |
| anon at rest after open | **166 MB**, against rung 3's 2.06–2.17 GB on a comparable bundle |
| the ladder, on a built bundle | 0 / 1,631,343 / 10,000,000 across no terms, `cc-by`, all eleven keys; `match abstract:"network"` 404,853 matched without moving the visible count |

⊘ **The prefix is not a uniform sample** — `--limit` keeps `entity_id < 10⁷`, the first five chunks
in staging order. It brackets the build's cost; it does not stand in for the corpus.

**The serving floor is a twelfth of rung 3's, and that is a second data point for the probe's open
question.** `probes/2026-09-02-serve-under-memory-cap/` attributed rung 3's ~2 GB floor to the
`mesh/descriptors` artifact-projection build over 1.66×10⁹ member rows, and asked whether it scales
with the membership. This rung's tiered layer over 38.8×10⁶ member rows in the prefix opens at
166 MB, which is consistent with it doing so.

**Both layers draw and neither is withdrawn.** The box holding the middle 90% of an artifact's
members, as a share of the map, at 10⁷: `clusters/kmeans` median **0.069%**, `topics/openalex`
median **2.08%** and tightening with depth — 8.23% at domain, 4.93% at field, 2.52% at subfield,
**1.10%** at topic. Against the taxonomies withdrawn at rungs 1 and 2 (medians 13.7% and 9.6%) even
the domain level is compact.

⊘ **The build's `everywhere` fraction disagrees, and more sharply than at rung 3.** Every level of
`topics/openalex` reports **1.000 everywhere** — at 153.0, 147.8, 115.0 and 53.0 blocks an artifact
— against the clustering's 0.448 at 3.9. A topic whose members occupy 1.1% of the map is still
spread across enough of row space that no tile-index node bounds it. This corpus separates
compactness in the map from boundability in row space at every level of one layer.

**All four of these are now measured** — `verify --deep` at 10⁸, the whole bundle's size and
breakdown, the serve-under-cap result and the layer spread at full scale. §4b.

## 4b. Rung 4, whole: the build completes

⊘ **2026-09-04, dated note.** `--arena-order` is deleted (`build-column-extents.md` §6); the figures
below are unaffected history.

**2026-09-04.** The same corpus, the same declaration, the same box — 102,117,343 rows,
118.9 GB of abstracts indexed as text, 47 GB of RAM. `tessera build --stage-timings
--stage-timings-json --arena-order auto`, under `sample_rss.py`, on 448 GB of free disk. It is the
first build of this corpus to finish, and it took **10,578.4 s — 2 h 56 m 18 s**.

⊘ **The box carried rung 5's staging throughout** — share passes and GPU work, no local `tessera
build` and no serve battery. The disk was this build's alone.

| stage | wall | what changed since §4a |
|---|---|---|
| `source_ids` … `external_ids` | **100.7 s** | 83 s on the arrival-order run |
| **`attribute_tail`** | **6,369.1 s (106:09)** | 704.7 s. **This is where the change is paid for**, below |
| `layers` | **90.4 s** | 93.4 s |
| `text_index` | **2,233.9 s (37:14)**, 57,637,877 terms | 2,371.9 s, and *over four hours without finishing* before that |
| `filter_postings` | **762.9 s** | 702.6 s |
| **`record_blob`** | **905.4 s (15:05)** | ⊘ **over four hours making no progress**, 52 MB written at 56 KB/s |
| `column_release` · `tiler_sort` · `segment_write` | 13.0 · 8.2 · 10.4 s | not reached before |
| `manifests` | **76.2 s** over 70,783,029,628 bytes | not reached before |

**`record_blob` is the stage the change was made for and it is now sequential.** Over the whole
stage: **0 major faults a second** and 133.6 MB/s read, against ~144 a second and 1,173.6 GiB read
in thirteen minutes on the arrival-order run. Its wall is 15 minutes.

**What paid for it is the join, and the price is large.** `--arena-order auto` chose `entity` —
123,869 MiB of declared string payload against a 32,508 MiB budget, share 16,254 MiB — so the join
decodes the source's prose a second time and writes each value at the offset a prefix sum gave it.
That is **6,369.1 s at 140 major faults a second and 690.5 MB/s**, against 704.7 s in arrival
order: **9× the join**, where at 10⁷ the same switch costs 2.04×. ⊘ **The extra is the scatter, not
the decode.** Pass two's writes ascend within a chunk and the chunk buffer does not grow with the
corpus, so at 10⁸ the arena is written as ~54 interleaved ascending runs rather than the six at
10⁷ — the term `probes/2026-09-03-entity-ordered-arena/` §3 flagged as argued rather than measured,
now measured, and it went against the argument.

**The trade is still worth taking and it is not the cheapest one available.** 6,369 s of join
against a `record_blob` that does not finish is not a close call. But a build that took the
arrival-order arena *with the ascending scatter* — which is what fixed the blob at 10⁷ — was not
run at 10⁸, and it is the arm most likely to beat this one. **That is the open question this rung
posed**, and §4c answers it: neither arena order, because the prose stops being held in entity
order at all. The same bundle takes 4,169.9 s.

**The arena is also 17 GB smaller.** The entity fill sizes it to exactly its records —
`.build-tmp/column-13.arena` is **119,350,876,262 B (111.2 GiB)** — where the arrival fill's
doubling produced **137,438,953,472 B (128 GiB exactly)** for the same column.

**Peak `VmHWM` 29,239 MiB, anonymous high-water 5,047 MiB.** The anonymous figure is a tenth of the
box on a build whose largest file is 111 GiB, which is the mapped design working as designed.

### The bundle

**70,783,029,628 bytes — 70.78 GB**, against the ~76 GB §4a projected from the 10⁷ prefix.
`verify --deep` **clean in 74.24 s** at 3.22 GB: 1 partition, 1 view, 1 segment, 102,117,343 rows,
`entity_id_high_water` 102,117,343, 12 terms, 102,117,343 pairs rows.

| | |
|---|---|
| `attrs` | **66.22 GB** — `record` **44.77 GB**, `abstract` **17.61 GB**, `title` 2.50 GB, `openalex_id` 916 MB, `publication_year` 416 MB |
| `views` | 3.06 GB |
| `entities` | 817 MB |
| `members` | 487 MB |
| `row-column` | 204 MB |
| everything else | under 1 MB |

**88% of the bundle is prose** — the record blob and the abstract index are 62.4 GB of 70.8 GB —
which is what the rung was built to demonstrate and what §4a projected at two thirds from a prefix
whose abstracts are shorter.

Both layers publish at full scale: `clusters/kmeans` 256 artifacts at 0.395 everywhere and 14.0
blocks each, disjoint; `topics/openalex` **4 / 26 / 252 / 4,516** artifacts across its four levels,
every one at 1.000 everywhere, the deepest served as a row-major column rather than a list. ⊘ The
`everywhere` fraction at every topic level is 1.000, as it was at 10⁷ — a topic whose members
occupy about 1% of the map is still unbounded in row space, and this corpus separates the two at
every level of one layer.

### Served, under a 24 GiB cap

`serve_battery.py` on 8131–8133 inside a transient scope at `MemoryMax=24G`, over the ladder the
rung's compartment gives it. **The cap held**: `memory.peak` sat exactly at the cap, `memory.events`
counted **110,266** reclaim-at-max events, and **`oom_kill` was 0**. The open — `tessera serve` to
`/readyz`, which on this rung is five levels of artifact projection being built — is **98.9 s**.

| principal | terms | visible | authorise | first viewport | hot p50 (z0 / z6 / z12) |
|---|---|---|---|---|---|
| no terms | 0 | **0** | 6.1 ms | 2.4 ms | 1.13 / 1.16 / 2.53 ms |
| `cc-by` | 2 | **14,028,655** (13.74%) | 6.8 ms | 0.93 s | 81.2 / 18.5 / 46.6 ms |
| all eleven keys | 11 | **102,117,343** | 1.7 ms | 7.41 s | 115.8 / 24.7 / 87.0 ms |

`match abstract:"network"` answers at a **134.9 ms** server-side median, hot.

⊘ **Cold is a different budget from hot at this scale, and the table above is the hot one.** A cold
sample is a fresh session's fragments plus a re-fault of a 70.78 GB bundle under a cap a third its
size: the 100% principal's cold p50 is **7.27 s** at zoom 0 and 11.13 s at zoom 12. The battery is
a reduced run — zooms 0/6/12, decile 9 only, one cell per decile, 40 hot samples and 8 cold — for
that reason; `test_corpora/paperseek/measurements.json` carries the parameters and every cell.

⊘ **The `cc-by` rung carries `mit` as well**, 62 pairs on a 14-million-row principal, because the
ladder's greedy takes the smallest unused term when it lands closer to the pair budget in ratio.
The campaign's stated ladder for this rung, 0 / 14,028,593 / 102,117,343, is those 62 pairs from
what was measured.

## 4c. Rung 4, again: the same bundle in a third of the time

⊘ **2026-09-04, dated note.** `--arena-order` is deleted (`build-column-extents.md` §6); the figures
below are unaffected history.

**2026-09-04.** Same corpus, same declaration, same box, and **byte-identical output**: 46 files
compared against §4b's bundle, none differing but `MANIFEST.json`'s `created_at` and the `CURRENT`
that carries its digest. `tessera build --stage-timings --arena-order auto` under `sample_rss.py`
on branch `build/prose-extents`. **4,169.9 s — 1 h 09 m 30 s**, against 10,578.4 s.

What changed is that a `text` column's prose is no longer placed at an entity index at all
([`build-column-extents.md`](design/build-column-extents.md)). Each chunk the join stages is already
sorted by entity, so each chunk of each text column is written out as one record-blob extent in
that chunk's entity order. The text index reads the extents in block windows; the record blob
merges them with the entity-ordered columns through the lifecycle's own row merge. The blob's
format and addressing do not change, which is what the byte equality demonstrates.

| stage | §4b, entity-ordered arena | this run |
|---|---|---|
| `source_ids` … `external_ids` | 100.7 s | **83.1 s** |
| **`attribute_tail`** | 6,369.1 s (106:09) | **890.2 s (14:50)** — 7.2× |
| `layers` | 90.4 s | **70.4 s** |
| `text_index` | 2,233.9 s (37:14) | **1,688.4 s (28:08)**, the same 57,637,877 terms |
| `filter_postings` | 762.9 s | **482.4 s** |
| `record_blob` | 905.4 s (15:05) | **777.4 s (12:57)** |
| `column_release` · `tiler_sort` · `segment_write` | 13.0 · 8.2 · 10.4 s | 0.0 · 13.8 · 7.8 s |
| `manifests` | 76.2 s | **52.8 s** over the same 70,783,029,628 bytes |
| peak `VmHWM` | 29,239 MiB | **19,590 MiB** |

**The join stopped paying for the blob.** §4b's `attribute_tail` was 6,369.1 s because `auto` chose
`entity`: 123,869 MiB of declared string payload against a 16,254 MiB share, so the source's prose
was decoded twice and scattered into a 111 GiB arena. Here the only column with an arena left is
`openalex_id` at **1,456 MiB**, so `auto` chooses `arrival` and there is no second decode. The
89 s the extents cost over §4a's one-pass join of 704.7 s is the zstd the join now runs.

**Every stage reads less.** `attribute_tail` runs at **0 major faults a second**, `text_index` at 1
and `record_blob` at 16, against §4b's 140 a second in the join. `record_blob` reads the extents
rather than 119 GB of arena, and reads 45 GB to do it.

⊘ **The improvement does not appear at 10⁷ and is not expected to.** On `medcpt-10m-abs`, where the
10.2 GiB arena fits the page cache, the same change costs 11%: the whole run is 447.9 s against
403.2 s, `attribute_tail` 71.4 s against 33.8 s and `record_blob` 71.4 s against 64.4 s, the extra
being compression the arrival-order arena never paid. What it buys there is the peak (8.0 GB
against 11.2 GB) and indifference to a cap: under `MemoryMax=4G` the same build is **461.5 s**, 1.03×
its uncapped self, at ~1 major fault a second. The two bundles are byte-identical, 38 files.

⊘ **The box was not quiet.** Rung 5's serve batteries ran on the same disk throughout this build
and the 10⁷ pair, where §4b's run had the disk to itself. The walls above are therefore an upper
bound rather than a clean measurement; the byte equality does not depend on it.

**The open question §4b posed is closed by not arising.** That question was which arena order to
build, and the answer is neither: prose is not held in entity order under any of them.

## 4b. Rung 5 — TreeOfLife-200M: built, and the first rung with two geometries

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The rung is [`../test_corpora/treeoflife/`](../test_corpora/treeoflife/README.md), which
carries the per-step tables; this section is what the campaign takes from it.

**Two tracks, and the interface between them held again.** The join track staged every non-vector
column of the 666 source files and scanned GBIF's 8,369 occurrence parts for coordinates; the
vectors track staged the fit sample, laid out 233,055,986 rows and wrote the corpus. Neither waited
on the other.

| | |
|---|---|
| the fit sample, off the share | **33.9 min** at 25.0 MB/s — 2,500,000 rows drawn evenly from all 666 files, 3.84 GB. **The 346 GB of vectors were never staged**: 217 GB free |
| the placement pass, off the share | **2 h 55 m**, 20,977 rows/s, 32.2 MB/s — every row positioned at the similarity-weighted mean of its 15 fit-set neighbours, nothing kept but a 1.86 GB layout |
| `prepare.py --sample 0 --reuse-layout` | **10.6 min** at **9.44 GB** peak |
| `tessera build --stage-timings` | **4,233 s — 1 h 10 m 33 s**, **39.97 GB** bundle, **35.3 GB** peak `VmHWM` of which **15.9 GB anonymous** |
| `verify --deep` | clean in **37.1 s at 10.29 GB** |
| the 10⁶ sample, end to end | prepare 111.8 s at 9.24 GB · build **18.4 s** to **162.3 MB** · verify clean in 0.13 s · served, driven, counts move with the mask in both views |

### The build converges, and the stage that binds is not rung 4's

Rung 4 reached `text_index` on 118.9 GB of abstracts and stopped making progress. This rung's
`text_index` is **32 seconds**: 164 million common names are 2.3 GB of characters. What binds
instead is **`filter_postings` at 2,125 s — half the build** — which is the price of an indexed
474-key `publisher` column plus two indexed name columns over 2.33×10⁸ rows.

The bundle is **19 GB of views** (`bioclip` 11 GB, `geo` 8.1 GB) and **12 GB of attributes**, of
which `uuid` alone is **8.0 GB** — a 36-byte indexed keyword is a fifth of the bundle and buys a
lookup nobody on this map performs. A rung that wanted the bundle smaller would drop it first.

⊘ **Both views report RESOLUTION LOST and only one is news.** `bioclip` puts 233,055,986 points in
8,330,745 distinct cells — **3.6%** — with the frame already `extent = "auto"`. That is fit-and-place
at a 93:1 ratio: rows sharing their fifteen fit-set neighbours share a position *exactly*, and a
photographic corpus carries very large near-duplicate sets. It is a property of the route, and the
price of not staging 346 GB.

### Two geometries over one entity space, and the layers separate on them

This is what the rung was chosen for, and it is the first result of its kind on the ladder. The box
holding an artifact's middle 90% of members, as a share of the view:

| layer | `bioclip` median | `geo` median |
|---|---|---|
| `taxonomy/tree` | **0.021%** | **1.71%** |
| `publishers/source` | **0.064%** | **0.169%** |

*`taxonomy/tree` by level on `geo`*: kingdom 37.8% · phylum 15.9% · class 4.70% · order 6.32% ·
family 4.73% · genus 0.711% · **species 0.118%**.

**A clade is compact in the embedding at every level and only compact on the ground below family**;
a publisher is the mirror image, spread through the tree and tight on the map, because an
institution collects near itself and across the tree of life. Both layers draw on both views and
neither is withdrawn. ⊘ The `bioclip` shares are against a frame set by a handful of outlying fit
positions, so the *ratios* between levels are the readable part there, not the absolute figures.

⊘ **The build's `everywhere` fraction disagrees, as at rungs 3 and 4.** `taxonomy/tree` reports
0.51–0.69 everywhere at 80–175 blocks an artifact on `bioclip` while its members occupy 0.02% of the
map. Compactness in the map and boundability in row space are different properties, and this corpus
now separates them on two geometries at once.

### The compartment, and the ladder every count is against

`publisher` — the institution that published the record, 474 keys. **Owner ruling 2026-09-03:** a
row with none carries `unpublished`, a key of the closed vocabulary, so a principal holding no term
sees nothing.

| principal | `bioclip` | `geo` |
|---|---|---|
| no terms | **0** | **0** |
| `iNaturalist.org` (1 of 474) | 134,852,438 (57.9%) | 126,390,401 (71.4% of the view) |
| all publishers | 233,055,986 | 176,899,537 |

**A principal's count differs between the views by the join rate, not by the mask.** ⊘ The
campaign's 50% target cannot be hit here: `iNaturalist.org` alone is 57.9% of the corpus, so a
greedy composition under a 50% budget takes every *other* publisher — 473 terms — and reaches
42.14%. The other five targets are exact.

### Served, and the anon floor at open is now 16 GB

`tessera serve` over the 39.97 GB bundle **opens in 87.7 s** building every level's artifact row
form over 1,001,193 artifacts across two views, and sits at **15.96 GB anonymous** before any
request. Rung 3 sat at 2.06–2.17 GB over a 1.66×10⁹-row DAG membership and rung 4's prefix at 166
MB; this is the third data point and the largest, and it is the same fixed cost §6 identified.

**A 24 GiB cap holds.** `oom 0`, `oom_kill 0`, `memory.peak` exactly at the cap with 3,449
reclaim-at-max events over the drive, **160 of 160 count-bearing responses identical** to the
uncapped run, and zero request failures across the whole battery. Uncapped, `memory.peak` is 27.4
GB. **Zoom 0 is the expensive request**, and the tiered layer is why: 1,001,193 artifacts against
the 464,655 that produced §6's first finding at rung 1.

### The ingest cycle at *f* = 50%, and a finding that belongs to the driver

`ingest_cycle.py --fraction 0.50 --concurrency 8 --state-extent`, 4 h 2 m. The hold-out —
**116,527,993 rows — went in at 11,060 items/s** at *C* = 8 (ack p50 5.99 s, p99 15.13 s; 11,653
× 200 and 2,487 × 429 retried), and the **fold took 1,313 s at 28.9 GB**. `verify --deep` on the
folded bundle is clean: **321,511,824 rows over two views**, `entity_id_high_water` 233,055,986,
**233,055,986 external-id bindings**. Nothing is lost.

⊘ **The hold-out enters the anchor view alone.** `bioclip` holds all 233,055,986 rows after the
fold; `geo` holds 88,455,838, which are the base's own. The driver's wire batch carries one row
space, so a rung with several of them measures the write path on one, and a `geo`-side census
differs by construction.

⊘ **2,142,399 rows are visible on the all-in bundle and not on the folded one, and it was the
wire's shape rather than the write path.** This cell sent `access` as one string per row, which
`encode_batch` filled with a comma-joined label list, and 70 of the 474 publisher names contain a
comma. Each split on the wire into fragments and every fragment not already a term was minted: the
folded deployment carries **617 terms against the declaration's 475**. Measured on it — 230,913,587
visible under the 474 declared terms, **233,055,986 under those plus the 148 fragments**. Worse than
hiding: **where a fragment is itself a real key the rows land in that compartment**, which is why
the 25% principal sees **73,212 rows more** on the folded deployment (`Natural History Museum,
Vienna` → `Natural History Museum`). This was the first rung whose compartment keys contain a
comma. **Ruled and built 2026-09-05**: the wire carries `access` as a list, one label per element,
taken verbatim ([decision 0129](decisions/0129-the-ingest-wire-carries-access-labels-as-a-list.md)),
and a layer with no supplied content travels as the ingest batch's column named for it
([decision 0128](decisions/0128-a-layer-with-no-supplied-content-travels-as-a-column-at-ingest.md)).
Proved on `treeoflife-1m` at *f* = 10%: zoom 0 exact at all six principals where three had differed,
`taxonomy/tree` exact where it had been absent, and with `--state-extent` every surface exact
([the rung's README](../test_corpora/treeoflife/README.md); records under
[`probes/2026-09-05-publish-streaming/runs/`](../probes/2026-09-05-publish-streaming/runs/)). ⊘ This 50% cell is not yet re-run
under the list.

**The 0091 census, retaken.** The run's own was shed mid-body on the zoom-0 whole-extent request
with `layers: "all"` — 1,001,193 artifacts against a stream deadline, §6's artifact-response-volume
finding at 2.2× rung 1's size, not a count difference. Retaken once after the fold with both sides
served in turn: **zoom-0 exact at the 1%, 5% and 10% principals**, and 22 differences over three
surfaces (3 zoom-0, 6 box, 13 layer). **Every layer difference is a layer that was never published**
— `clusters/kmeans` declined at 233,118,470 member rows, and `taxonomy/tree` absent from the
driver's roster because its member file is list-keyed and the publication path does not take one.
**`publishers/source` needs no publication and reproduces exactly** at five of the six principals:
its membership is the indexed column, which is the predicate layer's whole point.

⊘ **The flush figures are the driver's timeouts, not the write path's cost.** The executor's
counter did not move within 900 s and the visibility poll did not reach its target within 902 s —
the target being the all-in total, which the wire encoding above had put out of the polling
principal's reach. The fold completed regardless.

### Ruled 2026-09-03 — the contiguity experiment is withdrawn

The plan's §7 and §9.5 asked for the same rows ingested in taxonomy order and shuffled. Allocation
orders entity ids within a signature group by the item's Morton code and not by arrival
([`design/annotation-representation.md`](design/annotation-representation.md) §2.2), so the two arms
produce the same entity space and differ only by the cost of shuffling. The plan's rows are struck
with the reason. ⊘ The ruling cited `docs/decisions/0073-…`, which the 2026-09-02 docs cut deleted;
the rule it settled is the design document's.

### ⊘ One source row group is corrupt, and 50,000 rows carry a position that is not theirs

`train-00035-of-00666.parquet` row group 4 fails on its `emb` column with
`ZSTD decompression failed: Src size is incorrect` at every attempt — off the share and off a byte
copy on local disk — while the same group's other columns read cleanly. The publisher's bytes, not
this box's SMB. **50,000 of 233,055,986 is 0.021%**; the rule for an input is ignore and report, so
those rows take the layout's centroid and the manifest names the file and the group. Re-acquiring
that one file would fix them.

⊘ **The share also dropped three transient reads** in the 2 h 55 m pass. The placement now retries
four times and checkpoints per row group, so a multi-hour pass survives one; a first attempt lost 25
minutes to a read that a retry would have covered.

## 4d. Rung 6 — GBIF, whole corpus: the bounded-assembly design measured

The rung is [`../test_corpora/gbif/`](../test_corpora/gbif/README.md); the fraction runs it
describes are §"Modelled — the whole corpus" there, superseded below. Two whole-corpus builds are
compared, one before the bounded-assembly changes and one after, over the same **3,495,729,729**
placed rows and the same ten signature batches of 369,098,752 items, so the entity-id assignment is
identical between them (I9). Both `tessera build --memory-budget 24g --no-oracle-pairs
--stage-timings` on this box (WSL2, 12 cores, 47 GiB in the VM, local NVMe): **run 1** on main
`a4152e79`, 2026-09-13 01:01; **run 2** on main `d7d26c16`, 2026-09-13 21:52, after six branches
merged. The observations that drove the design are
[`evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md`](evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md);
the design itself is
[`evidence/memos/2026-09-12-bounded-assembly-design.md`](evidence/memos/2026-09-12-bounded-assembly-design.md);
the acceptance measurement is
[`../probes/2026-09-12-bounded-assembly/`](../probes/2026-09-12-bounded-assembly/README.md).

**The wall fell 15%, and one stage still exceeded the budget.** 4 h 09 m 35 s → **3 h 30 m 55 s**;
the bundle 207 GiB → **196 GiB**. `filter_postings` held **31.6 GB anonymous for twenty minutes,
7.6 GB over the 24 GB budget** — Finding A, below — and is the one respect in which run 2 did not
fit its budget.

### The build

| stage | run 1 | run 2 |
|---|---|---|
| `source_ids` | 85 s | 93 s |
| `dictionary` | 162 s | 189 s |
| `geometry_read` | 398 s | 476 s |
| batch loop (10 × `signature_sort` + `assignment`) | 1,672 s | **1,463 s** — sorts 262 s, assignments 1,203 s |
| `postings_write` | 71 s | 63 s |
| `attribute_tail` | 1,646 s | 1,574 s |
| `layers` | 2,141 s | 2,258 s |
| `filter_postings` | 3,677 s | **1,606 s** |
| `record_blob` | 1,917 s | **1,350 s** |
| `tiler_sort` | 242 s | 328 s |
| `segment_write` | 562 s | 629 s |
| `artifact_pass` | 1,728 s | 1,901 s |
| `manifests` (the digests) | 65 s | 69 s |
| **wall** | **4 h 09 m 35 s** | **3 h 30 m 55 s** |
| **bundle** | 207 GiB, record-blob format 9 | **196 GiB, format 10** |

The bundle shrank on a format change, not the build's own doing:
[decision 0142](decisions/0142-the-record-blob-delimits-a-row-by-a-length-the-row-states.md) has a
row carry its own length rather than the reader consulting a per-row offset table, format 9 → 10,
landing in the same merge set.

**Per-batch assignment seconds**, run 1 → run 2, batches 1 to 10 (the tenth is the
173,840,961-row remainder): 110, 65, 65, 152, 187, 204, 233, 219, ~200, 46 → 107, 67, 64, 135,
153, 165, 164, 164, 140, 44. The hoist that removed the entity map's scattered writes (below) took
the climb from a 65–233 s range down to 64–165 s, about 12% off the loop total — smaller than the
whole-loop change first suggested, because most of what remains is not writeback: one core at
100% with no disk traffic, rising superlinearly with batch size (the half-size remainder batch
costs 0.25 µs an item against 0.45 for a full batch). What is left is unexplained past that and the
next step is a `perf` profile of a late batch, not another guess.

**`tiler_sort` (+86 s) and the artifact pass (+173 s) got slower with no code change between the
two runs, and neither change is explained** — assumed to be the page cache the process inherited
from the stage before it, not measured. ⊘ The `peak=` figure stage timings print is `VmHWM`, the
process's lifetime high-water RSS including file pages, not the anonymous figure below, and a
stage's number in that column can belong to an earlier stage that mapped the file — read from the
code (`crates/tessera-build/src/observer.rs`), not from a profile of this run.

**Six branches landed between the runs, all on main `d7d26c16`.**

- The label-agreement check hoisted into ordinal order, removing the scattered entity-map writes
  the assignment walk made under run 1.
- Memberships mapped from their own extents at open, publication and fold.
- The record blob's format 10 (decision 0142).
- The string-column extent fold bounded by the memory budget instead of a fan-in of 128 — no fold
  ran at this rung.
- `verify` holds no row-sized structure.
- One executor owns a bundle root under a file lock, and a side-manifest number is never planned
  twice.

### Memory and disk

**Peak anonymous RSS**: run 1 **21.5 GB in `record_blob`**; run 2 **31.6 GB in
`filter_postings`** (Finding A, below), otherwise under **22.4 GB** in the batch loop and
**17.9 GB** in `record_blob`. Swap peaked at **0.4 GB** in run 1 and **0.9 GB** in run 2.

**Disk never came close to the pre-flight's forecast.** 429 GB free at the start, a minimum of
**221 GB free** at the end — the transient never exceeded the finished bundle. The printed
forecast was **~539 GB**, a model that sums three ceilings (artifact-pass buckets at 80 GB,
row-column lanes at 40 GB, and the ordinal geometry) and is known to overstate; it did here too.

**Finding A: `filter_postings` held 7.6 GB over budget for twenty minutes, and the residency model
has no term for it.** Diagnosed from the code and the on-disk file sizes, not from a heap profile.
`ExtentColumn::open` (`crates/tessera-build/src/extents.rs`) deserialises every extent's has-row
bitmap onto the heap and builds a per-extent live set by subtracting later extents' rows
(`andnot_inplace`). Each of the two string columns spilled 988 extents; the has-row files are
run-encoded on disk (2.2 GB for `scientificname`), but the subtraction produces array containers
at 2 B an entity — about 7 GB of live sets a column, plus about 4.5 GB of has-row for the two
columns, over a 6 GB base. Run 1's fold to 8 extents put 437 M entities in each so every container
was a fixed 8 KiB bitset, about 14 GB for both columns: the fold halved the term by changing the
container's encoding and never bounded it. `merge_fan_in`'s comment assumes a join chunk covers a
contiguous entity run; a chunk is in the attribute source's order, scattered over entity space, so
that assumption does not hold here.

The designed fix opens extents cursor-only with the has-row file mapped, replaces the per-extent
live sets with one duplicate map a column (`seen` and `repeat` bitmaps plus the last extent index
per repeated entity, one sequential pass over the has-row files, about 90 s here) and adds the
one-bitmap term to the residency model. Modelled cost after the fix: about 1 GB for both columns,
no disk, and about no change to the pass's time. Not built.

### Verify

`tessera verify --deep` under `systemd-run --user --scope -p MemoryMax=24G -p
MemorySwapMax=2G`. Run 2's format-10 bundle verified **clean in 14 m 42 s at 0.61 GB peak
anonymous**: 254 terms, 254 dictionary records, 3,495,729,729 record-blob rows, 1 segment,
`entity_id_high_water` 3,495,729,729. Run 1's format-9 bundle took **20 m 50 s at 47 GB
anonymous** — verify holding no row-sized structure is the change between the two runs, and the
anonymous figure is the direct measurement of it.

### Served, under a 24 GiB cap

`tessera serve` on run 2's bundle under `systemd-run --user --scope -p MemoryMax=24G -p
MemorySwapMax=0`. Opened to `/readyz` in **193 s at 6.5 GB anonymous** — three taxonomy levels'
row forms built at open cost 1.03 s of that. Before this merge set, the open was modelled at about
60 GB anonymous and could not be attempted on this box. `memory.peak` sat at the cap (file pages),
**6,844** reclaim-at-max events, **`oom_kill` 0**.

Single-request latencies, hot, under the 253-term all-countries principal:

| request | latency |
|---|---|
| zoom 0, whole extent, with layers | 51–59 s |
| zoom 0, whole extent, `layers: []` | 20 s |
| a 10°×10° box, zoom 0 | 20 s |
| zoom 3 | 0.23 s |
| zoom 6 | 0.24 s |
| zoom 12 | 0.02 s |

⊘ **The first battery run failed after exactly 3,600 s**, a 403 while ranking 500 candidate boxes
at zoom 0 under the 100% principal: the deployment's `token_max_lifetime` is 3,600 s and
`serve_battery.py` authorises once and never re-authorises — a harness defect at this scale, not a
serving one. The second run raises the lifetime to 43,200 s and 40 candidates a zoom.

⊘ **The battery itself was stopped after three of its six principals.** Its figures are dominated
by a zoom 0 anomaly now under investigation, so the owner stopped the run after the 10% principal
to use the server for that investigation; the battery will be re-run after the fix.

`serve_battery.py --view geo --zooms 0,6,12 --deciles 9 --candidates 40 --samples 10
--cold-samples 3 --text-samples 0` against the running capped server, ranks from
`country-ranks.json`, token lifetime 43,200 s. The 100% principal sees all 3,495,729,729 rows at
zoom 0 over the whole extent. Candidate boxes at zoom 6: visible min 0, median 4,075, max
25,845,590 over 40; at zoom 12: min 0, median 5,703, max 28,088,143.

p50, ms unless marked; two decile-9 cells a zoom, both given as *a* / *b*:

| principal | terms | authorise | first viewport | zoom 0 cold | zoom 0 warm | zoom 0 hot | zoom 6 cold | zoom 6 warm | zoom 6 hot | zoom 12 cold | zoom 12 warm | zoom 12 hot |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1% (AI, MC, MX, SM, XZ) | 5 | 8 ms | 6.45 s | 7,923 / 7,878 | 4,922 / 4,650 | 268 / 267 | 5,102 / 4,960 | 84 / 134 | 6.4 / 6.1 | 4,642 / 4,730 | 101 / 290 | 2.6 / 4.4 |
| 5% (EC, GW, IS, SE, SM, XZ) | 6 | 16 ms | 14.21 s | 12,321 / 12,821 | 6,455 / 6,392 | 1,269 / 1,263 | 7,824 / 7,726 | 88 / 83 | 6.3 / 1.0 | 8,141 / 8,116 | 152 / 302 | 2.7 / 3.1 |
| 10% (AU, ES, RU, SH, VA, XZ, ZM) | 7 | 31 ms | 20.92 s | 20,899 / 20,630 | 8,728 / 8,823 | 2,653 / 2,661 | 14,405 / 14,125 | 110 / 100 | 6.3 / 6.1 | — | — | — |

**Hot zoom 0 grows linearly with the visible share**: 0.27 s at 1%, 1.27 s at 5%, 2.65 s at 10%,
about 20 s at 100% (the single-request figure above) — about 6 ns a visible row. The zoom 0 path
walks the visible rows rather than working per container. **Cold zoom 6 and zoom 12 also grow
with the principal**, 5.0 s, 7.8 s and 14.4 s at zoom 6 across the three principals measured, so a
cold request re-reads the principal's mask or the memberships behind it, and not only the tile.
Hot zoom 6 and zoom 12 stay at 1–6 ms at every principal measured. The zoom 0 investigation is
open at the time of writing.

## 5. The machinery this campaign built

- **[`../test_corpora/`](../test_corpora/README.md)** — one directory per rung, in git: `prepare.py`,
  `corpus.toml`, `README.md`. Derived files go to `$TESSERA_LADDER/<rung>` (default
  `data/ladder/<rung>`), so the second volume is one environment variable rather than an edit to
  every script.
- **`test_corpora/common/projection.py`** — the WGS84 → Web Mercator transform, unit square,
  **y south**. It placed both geographic rungs while Tessera had no projection layer and places
  none now; what it is instead is the **second implementation** the engine's own transform is held
  to. `tessera_spatial::projection` runs it over 100,000 sampled coordinates and requires the same
  *stored* position, its `TEST_VECTORS` and `TILE_VECTORS` (the only real test of the y direction)
  are data both languages read, and each rebuilt bundle was checked by recomputing every point's
  expected 32-bit fixed-point position through it from the source degrees.
- **`~/venvs/ingest`** — DuckDB and PyArrow, with `spatial` installed for rung 2's point-in-polygon
  join (§3). ⊘ Its Python is 3.10, so it has no `tomllib`; `~/venvs/projection` does.
- **`run_demo.sh --terms / --ranks / --label`**, and `custom` on ports of its own — see §6.
- **The measurement drivers** (2026-09-03) — `tessera build --stage-timings-json`,
  [`../test_corpora/common/serve_battery.py`](../test_corpora/common/serve_battery.py) and
  [`../test_corpora/common/ingest_cycle.py`](../test_corpora/common/ingest_cycle.py), booted by
  [`../test_corpora/common/deployment.py`](../test_corpora/common/deployment.py) and collated by
  [`../scripts/campaign_report.py`](../scripts/campaign_report.py). Every rung records build time
  per stage, view latency across a principal ladder, and online ingest through flush and
  compaction, on **one schema** —
  [`../test_corpora/common/README.md`](../test_corpora/common/README.md) is that schema, field by
  field with each one's unit and how it was measured. The table in §1.1 is rendered from the
  committed `measurements.json` files and must not be hand-edited.
- **Every deployment of a rung shares the frame its first all-in build recorded**, and a deployment
  may start with no points in it at all (owner ruling, 2026-09-03). A rung declaring
  `extent = "auto"` fits its frame to the rows the build saw, so a base built from part of the
  corpus quantises onto a different grid and every box-level count differs at the margins for a
  reason that has nothing to do with the write path; `ingest_cycle.py --state-extent` copies
  `MANIFEST.views[].quantisation` out of the all-in bundle into the measurement's own copy of the
  declaration, never the rung's committed one. Stating the frame is also what makes the *f* = 100%
  cell expressible: with the frame given there is nothing to fit, so `tessera build` writes a bundle
  with no points and the whole corpus arrives through `/control/ingest` (decision 0091). `auto` over
  no rows stays a refusal, and it names the remedy.
- **Everything is ingested after the build** (owner ruling, 2026-09-03). A cell's base bundle
  carries the built fraction's points and every layer's *declaration* — kind, levels, visibility
  rules, content kinds — and no roster, member table or content; each layer's artifacts, their
  memberships, their ranked content with its generating set and their `parent` lists are published
  through `PUT /control/layers/{name}/artifacts` once every point they depend on has been ingested.
  That ordering is the only constraint and it holds at every fraction, so at *f* = 10% the base is
  90% of the points and none of the artifacts. It is also why no membership travels on a column
  here: a column names artifacts that do not exist yet, and a layer declaring supplied content
  refuses to mint one from a key alone.

## 6. Cross-cutting findings

Ordered by how much they matter beyond this rung.

**A tiered layer returns every level whatever the zoom, and at the opening view that is 49 MB.**
464,655 artifacts and 2.9 s per viewport request for a broad principal at zoom 0, against 4 KiB and
67 ms for the points alone. The two bounds that work — the mask and the tile index — both bound
*which artifacts are in range*; neither bounds *which levels the client wanted*, and at whole-world
zoom 0 nothing is out of range, so the level is the only axis left and it is the one a request
cannot name. The corpus already declares the answer: its zoom→level map says level 0 alone applies
at zoom 0, 254 artifacts against 464,655 served. The full account, and the questions a design pass
has to answer, are in
[`evidence/memos/2026-08-28-artifact-response-volume.md`](evidence/memos/2026-08-28-artifact-response-volume.md).
**This is the campaign's first real finding and it arrived at rung 1**, on the serving side, where
the plan expected its first walls at rung 2 on the build side.

**A geographic corpus is reproducible, and an embedding corpus is not** — and this was spent
rather than merely asserted. A projection is a pure function, so a geographic rung built on a frame
that later changes costs a rerun rather than the loss `data/geometry.parquet` would be. Both rungs
were placed by a Python module before Tessera had a projection layer and both were rebuilt on the
declared projection on 2026-08-30 for the price of a `prepare.py` and a `tessera build` each. It
does not transfer to rungs 3–5.

**Declare a width from a measured range, never from a maximum.** `population` was declared `u64`
from a census that measured only the maximum; the build refused on a **-12** two reefs in Kiribati
carry. `prepare.py` now prints every numeric's full range for exactly this reason. The build
refusing rather than truncating is the system behaving correctly, and it is a slow way to learn it.

**`default = "public"` makes unlabelled rows universally visible, and their attribute values leak
into every principal's derived listing.** GeoNames' 6,997 blank-country rows are public by
declaration, so their `admin1` values appear for every principal — 32 of GB's 37. Correct given the
declaration, and worth deciding deliberately at each rung rather than inheriting.

**A published hierarchy's codes are only unique within their parent.** GeoNames' `admin1` has 823
distinct codes standing for 4,823 real regions; keying on the bare code would merge Scotland with a
Brazilian state. Qualify by the full path. Expect the same at Overture, GBIF and MeSH.

**`parent_edges` conflates two different nulls.** For a clustering, a null entry means *noise at
this resolution* and reading across it would state a containment no row makes — which is why
`parent_edges` is `windows(2)`. For a gazetteer it means *no code was recorded*, and the containment
is not in doubt. GeoNames is the first corpus where the two come apart, and the surface has one
spelling for both. Routed around here by materialising the hole as an explicit artifact (1,373 of
them, against 464,000 real); **not raised as an issue and not designed**.

**A 4 GiB cgroup cap survives `tessera serve`'s open and then OOM-kills on the first request; 12
GiB serves the whole drive cleanly, with byte-identical masked counts to an uncapped run.** Tested
against `data/ladder/medcpt` (11.15 GB) under `systemd-run --user --scope -p MemoryMax=…`: open
always completes and `/readyz` answers 200 at anon ≈ 2.06–2.17 GB resident, but at 4 GiB the first
viewport request — even the cheapest principal measured — reliably exceeds the ~2 GB of headroom
left and the kernel OOM-kills the process (reproduced three times; `dmesg` confirms reclaim was
attempted and insufficient, not a reclaim that ran out of candidates). The category/text postings
and value columns behave exactly as designed — mapped, resident only where a request scans, and
confirmed both by code and by `/proc/<pid>/smaps` — so **that part of the design already tolerates
a bundle larger than memory**. What does not yet tolerate it is a fixed, per-process anon floor at
open, best-evidenced (not directly profiled) as `mesh/descriptors`'s DAG artifact-projection build
now paid at open rather than lazily (`Engine::warm_artifact_projections`,
`probes/2026-09-02-cold-start/`) over this bundle's 1.66×10⁹-row closed membership. Whether that
floor scales sub- or super-linearly with a DAG's membership size was not measured — this bundle is
the ladder's only DAG-layer data point — and is worth measuring with a heap profiler before rung 4
(a ~60 GB bundle on a 47 GB box) commits to a hierarchy shape, because it is the one part of the
request path that reads real, unavoidable memory into the heap at open rather than paging it in on
demand. Full method, the three OOM attempts and the anon/file breakdown (including why `file`'s
figure is contaminated by cgroup v2's first-toucher page-cache charging and should not be trusted
across runs) in `probes/2026-09-02-serve-under-memory-cap/`.

## 7. Problems found in tooling, and what was done

**`run_demo.sh` reported a scale ready when another process held the port.** The readiness poll asks
the *port*, not the process it started, so a stale server answered, the script declared success, and
everything downstream talked to a different bundle — surfacing as "no candidate term is visible to
anyone", which names neither the port nor the cause. **Fixed**: a port already in use is now a
refusal that says so.

**`--bundle` could not serve any corpus with its own dictionary.** It hardcoded the demo fixtures'
synthetic `0..200` terms, so every principal measured empty. **Fixed**: `--terms`, `--ranks` and
`--label`, and `custom` now has ports of its own rather than sharing `2m4`'s.

**The projections work was committed under an unrelated message.** HEAD moved from `37e43a8` to
`b1cb81b` mid-session and the edits to [`design/projections.md`](design/projections.md) were swept
into `dd195c9`, a commit about the rings track. Content intact, provenance misleading.

**`test_corpora/` is untracked** and needs its own commit.

**`ingest_cycle.py` was MedCPT-shaped in three places and rung 5 found all three.** Its base
directory copied one hardcoded `branch.parquet` and one points file, so a rung declaring eleven
vocabularies and a second view refused the base build on a missing file; its wire batch named
MedCPT's own four attribute columns, so every batch was a 422 — *every declared column must be
present, the scalar tail being read back by position* — and the access column may be a scalar and a
declared attribute rather than a list that is neither; and it sent no `x-tessera-view`, which a
bundle with more than one view requires and rightly refuses without (contracts §3.4). **Fixed**: all
three are read off the rung's own declaration, and `wire_columns` returns MedCPT's four unchanged,
so rung 3's cells are unaffected. ⊘ **The hold-out still enters the anchor view alone**: a second
view's rows for the same entities do not travel, so a rung with several row spaces measures the
write path on one of them.

**A fourth was the wire's shape, and the wire changed.** `access` on `/control/ingest` was one
string per row, which the driver filled with a comma-joined label list, so a compartment key
containing a comma split on the wire and its fragments were minted as terms — 2,142,399 of rung 5's
ingested rows became invisible to every declared principal, and 73,212 landed in **another
publisher's** compartment (§4b). Every rung below has keys with no commas — country codes, MeSH
branch letters, licence names — so nothing before this one could see it. The owner ruled that the
wire carries a list, one label per element, taken verbatim (decision 0129); the plugin's list form
is now the only path an item's labels take at either entry point, and the driver sends the list.

## 8. Open, and what is next

**Owner calls outstanding**

- The design pass on artifact response volume (§6, and the memo it points at).
- Whether `parent_edges`' two nulls need separating, and whether that is worth an issue.
- Whether this tracker is the campaign's status record or the campaign moves to issues.
- ~~Whether rung 3 takes its abstracts~~ (§4.1) — **ruled 2026-09-05: taken** (`prepare.py
  --abstracts`; the rebuild and re-measurement follow). The memory objection had been answered
  first. The 10⁶ figures behind it (`tessera build` 716 MB against 2,246 MB, extrapolating to
  ~81 GB) were `VmHWM`, which counts file-backed pages the kernel may evict alongside heap it must
  keep — and since 2026-08-30 the columns are mapped, the text index spills under a budget and the
  blob streams, so on prose most of that is page cache.
  [`probes/2026-09-02-text-peak-split/`](../probes/2026-09-02-text-peak-split/README.md) split the
  two at 10⁶ and 10⁷ against a control that is the same 10⁷ corpus with the column undeclared: the
  abstracts cost **+9,594 MiB of `VmHWM` and +562 MiB of anonymous memory** at 10⁷, and **+208 MiB**
  under `--memory-budget 6g`, where the text pass spills 298 runs against 96 and the cascade fires.
  Anonymous memory alone extrapolates to **18.6 GB at 36M with abstracts against 16.4 GB without** —
  modelled, and the without-figure is 2% from the real whole-corpus build's 16.03 GB (§4.6). The
  anonymous high-water is the `manifests` stage in every arm, which is the MeSH DAG's layout and not
  the prose — the stage has since been split, and that layout is now `artifact_pass`, so the figure
  predates the name it is stated under. So the ruling turns on a 27.7 GB bundle and roughly double the wall time, not on a
  memory wall, and the "streaming text column" the plan called for is machinery that already exists.
  The built rung takes them off, which is `prepare.py`'s default; `--abstracts` is the other run and
  needs no code change.
- ~~Whether rung 3 is worth the 51.8 GB baseline~~ — **ruled 2026-09-01: not needed.** `journal`
  and `publication_type` are not taken; the rung renders what the chunks carry.
- ~~What to do with the 5.87% of unresolved descriptor mentions~~ — **ruled 2026-09-01: dropped**,
  and the drop is stated beside every coverage figure (§4.4). This is a technology demonstrator, not
  a production system, and the replacement-terms repair is not worth its step.

**Designed 2026-09-01, provisional** — [`design/dag-hierarchies.md`](design/dag-hierarchies.md),
reviewed once (r2), all five rulings made, awaiting promotion

- ~~Several member rows for one point under a levelled kind.~~ **Not a change.** A member source is
  one row per `(artifact, entity)`, not one per point, and the reader has no per-entity uniqueness
  under any kind; `prepare.py` explodes the `m` field and today's reader takes it (design §2). What
  the survey described was the point-source *list* column, which nobody needs here.
- **A hierarchy declared as a DAG** — `kind = "dag"`, a child naming several parents recorded rather
  than refused, depth the longest path, the cut reading every depth's count. Measured on the MeSH
  file: 30,954 descriptors, 42,287 edges, 30.0% with more than one parent, acyclic, longest path 17
  (`probes/2026-09-01-mesh-dag/`). The one question the data forces is the membership's closure:
  ≈3.1×10⁸ rows explicit against ≈1.7×10⁹ closed upward, extrapolated from chunk 18.

**Delivery, track `store` (2026-09-01, branch `dag/store`)** — the declaration, the durable record,
the build and the ingest side of the design above are built: `kind = "dag"`; a record's parents as
a list in the WAL row and the record blob, `BUNDLE_FORMAT` 4 → 5 and a bundle at any other number
refused at open; the artifact row's `parent` as a list; a second parent recorded under `dag` and
refused as before under `nested` and `tiered` at both entry points (since 2026-09-03 the `parent`
list is the only edge spelling under `dag`, a member row's list being plain multi-membership —
[decision 0125](decisions/0125-a-dag-list-column-is-membership-not-lineage.md); the ingest cycle
carries the layer from a `mesh/descriptors` column on the points rather than declining it); and the cycle check the ingest
side lacked, in the registry's publication so one body serves the build, `publish_artifacts` and
the commit window's mint. What is *not* in this track: the cut over parent lists, longest-path
depth, `parent_ids` on the wire and the client — the engine and client tracks'. Ledger:
`.superpowers/sdd/2026-09-01-dag-hierarchies/progress-store.md`.

**Found at rung 3, not owned by this campaign**

- ⊘ **The artifact drill-down omits the DAG's edges.** `POST /v1/artifacts/{tessera_id}` answers
  `layer`, `key`, `masked_count`, `centroid`, `box`, `shape`, `content` and `rung`
  (`tessera-server/src/viewer.rs`, the `ArtifactResp` construction) — **no `parent_ids`**, where the
  viewport's artifact frame carries them (`tessera-wire/src/payload.rs`, `ArtifactRow::parent_ids`;
  `tessera-engine/src/viewport.rs`). A client that drills into a descriptor is told its count and
  not where it sits, so a DAG cannot be walked from a drill-down. Read from the source 2026-09-02;
  no test asserts either way.
- ⊘ **`clients/ts/viewer/smoke-artifacts.mjs` draws no hull ring on this corpus.** Its own report
  reads `253 clusters, 253 with geometry, 0 rings drawn over 0 artifacts` under the broadest
  principal, with labels drawn and no console error — the geometry reaches the client and nothing
  renders it as a ring. The same script's other assertions pass on the substance: counts move with
  the mask (`mesh/descriptors` `#723223` is 134,030 / 647,908 / 756,640 across three principals).
  Two of its failures are its own calibration against arXiv — this rung's `narrow` and `sparse`
  presets resolve to the same single term — and are not defects.

**Host, not Tessera**

- ⊘ **Run a memtest on this box before chasing any further one-off.** Three corruption-class
  symptoms on 2026-08-22/23, and rung 3 added three more on 2026-09-02 — in three different places,
  in three different shapes, across two processes, none reproducing (§4.6):
  45 member rows of 1.66×10⁹ shifted under the wrong article in a structured way no code path
  accounts for; one entity missing two ancestor rows in the next run, which had the first run's
  window right; and a `SIGSEGV` in `tessera build` — `signal 11 … error 6`, a write to a
  non-present page — that did not recur when the same binary was relaunched on the same inputs.
  Each run is otherwise bit-consistent with a recompute. None is attributed to Tessera and none
  should be until a reproduction exists. ⊘ **The per-slice check rung 3 added catches the first
  shape and not the second** — a dropped ancestor row leaves every explicit id in place — and that
  gap is deliberate: closing it would mean recomputing the closure to compare against itself, and
  against a hardware fault a second run is not a defence.

**Rung 1 work not done**

- `places/containment` — the third layer, from `hierarchy.txt`, as a `nested` lineage. Needs a DAG
  walk and will meet genuine multiple parents — **the same surface change §4.3 rules for**, arriving
  at rung 1 rather than at rung 3.
- Everything in §2's ❌ rows: the 0091 test, the oracle census, the write cycle, p99 and a screenshot.

**Found at rung 4**

- ~~**The build's mapped text arena is what stops rung 4, and no budget reaches it**~~ (§4a) —
  **answered by removing the arena** (§4c). The abstract column's `.build-tmp` arena was 128 GiB
  against a 47 GB box, at 93% system time with 11 of 13 threads on `folio_wait_bit_common` and PSI
  `io` `full` at 61%: the campaign's first wall actually met, and not the shape W2 names. A `text`
  column has no arena now. Its prose is written once as record-blob extents as the join decodes it,
  and the two passes that read it read those.
- ⊘ **The abstracts ruling's evidence needs the correction above, not a reversal.** The text-peak
  probe's anonymous extrapolation was accurate; what it could not see at 10⁷ is that the arena it
  never had to page becomes the binding constraint at 10⁸.

**Before rung 4** — *carried; rung 4 ran without either*

- ~~The DAG design, reviewed and ruled~~ — **done, and built**: rung 3 is the corpus it was designed
  for and it carries a `kind = "dag"` layer end to end (§4.6).
- Rung 0, still not taken, and rung 4 is a reason to want it rather than a reason to drop it. It
  confirms W1 and W2 reproduce and whether the pre-flight refuses rather than being killed. ⊘
  **Rungs 2 and 3 both passed without either wall firing**; rung 4 met a wall that is neither of
  them (§4a), which is the case a controlled run would have named first.
