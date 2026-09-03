# The ingest campaign — status

**Status:** Working status record, never normative. **This is a status document and is expected to
be edited in place** as rungs land; it is not a dated memo. The plan it executes is
[`evidence/memos/2026-08-27-ingest-campaign-plan.md`](evidence/memos/2026-08-27-ingest-campaign-plan.md),
which is a *plan* and has already been departed from in several places — where the two disagree,
this document records what was actually done and why.

⊘ **This tracker has no pointer in `CLAUDE.md`.** It follows the convention
[`artifact-delivery.md`](artifact-delivery.md) and [`client-delivery.md`](client-delivery.md) use,
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
| **4** | **PaperSeek + OpenAlex** | **102,117,343** | **Staged, prepared and ⊘ not built** 2026-09-03 — see §4a. The corpus exists: 254 GB staged in one 164.7-minute pass, laid out and joined to OpenAlex in 43.8 minutes at 18.4 GB, 52.2 GB of `points.parquet`, 394,325,928 topic member rows, and the ladder's first compartment that is a property of the row. **`tessera build` reaches the abstract text index and stalls there** — not refused, not killed, 93% system time against a 128 GiB mapped arena on a 47 GB box. The rung's finding is that negative |
| 5 | TreeOfLife | 2.33×10⁸ | Not started. Staged |
| 6 | GBIF | 3.50×10⁹ | Not started. Staged; needs a second local volume |
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

**The second volume (plan §4) is not built.** It is rung 6/7 work and nothing before then needs it.

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
children rather than aggregation ([decision 0087](decisions/0087-cross-level-edges-are-information-not-rollup.md));
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

## 4a. Rung 4 — PaperSeek + OpenAlex, prepared and not built

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The rung is [`../test_corpora/paperseek/`](../test_corpora/paperseek/README.md), which
carries the per-step tables; this section is what the campaign takes from it.

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
| `tessera build` | ⊘ **does not converge**, below |
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
the text pass streams rather than maps, (c) a smaller corpus, (d) more RAM.

**⊘ Not measured, because they need the bundle that does not exist**: `verify --deep` at 10⁸, the
bundle's size and per-directory breakdown, the serve-under-`MemoryMax=24G` result, and the layer
report's median box at full scale. The 10⁶ sample's layer spread is measured and is in the rung
README: `clusters/kmeans` median **0.75%** of the map, `topics/openalex` median **2.35%** and
tightening with depth — 6.61% at domain to **1.64%** at topic. Both layers draw; neither is
withdrawn on that evidence.

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

## 8. Open, and what is next

**Owner calls outstanding**

- The design pass on artifact response volume (§6, and the memo it points at).
- Whether `parent_edges`' two nulls need separating, and whether that is worth an issue.
- Whether this tracker is the campaign's status record or the campaign moves to issues.
- **Whether rung 3 takes its abstracts** (§4.1) — **still open, and the memory objection is
  answered**. The 10⁶ figures behind it (`tessera build` 716 MB against 2,246 MB, extrapolating to
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
  the prose. So the ruling turns on a 27.7 GB bundle and roughly double the wall time, not on a
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
refused as before under `nested` and `tiered` at both entry points; and the cycle check the ingest
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

- ⊘ **The build's mapped text arena is what stops rung 4, and no budget reaches it** (§4a). The
  abstract column's `.build-tmp` arena is 128 GiB against a 47 GB box; the build's own anonymous
  high-water is 5.19 GB. It is neither an OOM nor a refusal — 93% system time, 11 of 13 threads on
  `folio_wait_bit_common`, PSI `io` `full` at 61%. **This is the campaign's first wall actually
  met**, and it is not the shape W2 names. The owner's options are a budget arm, a streamed arena,
  a smaller corpus, or more RAM.
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
