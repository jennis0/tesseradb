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

**Last updated:** 2026-08-28.

---

## 1. Where the campaign is

| # | Rung | Points | State |
|---|---|---|---|
| — | arXiv | 2,422,486 | **Have, and now on the campaign's convention.** The pipeline that produces it was a notebook outside `test_corpora/`; ported to [`../test_corpora/arxiv/`](../test_corpora/arxiv/README.md) on 2026-08-28 as `prepare.py` plus an optional `toponymy.py`, and the notebook deleted. It is the ladder's only embedding corpus and the only one whose source is derived rather than staged |
| 0 | Re-run the 5×10⁷ artifact tier | — | **Deferred, deliberately.** It confirms W1 and W2, which bite at rung 2 and not at rung 1, and it costs a ~45 GB build. Take it before rung 2, not before rung 1 |
| **1** | **GeoNames** | **13,463,857** | **Built, verified and served.** Not done against §7.1's bar — see §2 |
| **2** | **Overture places + divisions** | **7.4×10⁷** | **Prepared, not built.** [`../test_corpora/overture/`](../test_corpora/overture/README.md) carries the declaration and the pipeline, written against a survey of the staged bytes taken 2026-08-28 — which corrected four things the plan had wrong — see §3 |
| 3 | MedCPT / PubMed | 3.6×10⁷ | Not started. Staged; MeSH is **not** staged and is a prerequisite |
| 4 | PaperSeek + OpenAlex | 1.02×10⁸ | Not started. Staged |
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
with a README stating what was verified at acquisition and what is the publisher's claim.

**`data/` is already mirrored** to `arxiv-tessera/2026-07-27/`, so the plan's §5 cleanup is a
verification rather than a copy. It has **not** been verified and nothing has been deleted; there is
no space pressure at rung 1 (117 GB free, GeoNames needs ~5 GB end to end).

**The second volume (plan §4) is not built.** It is rung 6/7 work and nothing before then needs it.

## 2. Rung 1 — GeoNames, against §7.1's bar

The plan's bar for *done* is six things. Two are met.

| | |
|---|---|
| ✅ declaration passes `tessera check` | 6 sources, 1 view, 8 vocabularies, 13 attributes, 2 layers |
| ✅ bundle exists, frame report recorded | 1,329,553,710 bytes; report in `frame.json` and the rung README |
| ❌ decision 0091's build-vs-ingest test on real data | not attempted |
| ❌ masked-count census exact against an oracle | not attempted |
| ❌ one full write cycle (suppress → delete → re-ingest → fold → re-census) | not attempted |
| ⚠️ a results row | build wall, peak RSS and bundle bytes yes; **ingest rows/s, p99 at three zooms and a screenshot all absent** |

**Figures so far**, local NVMe, 47 GB machine, no `--memory-budget` set:

```
prepare.py       ~2 min          tessera build   6:05 wall, 4.2 GB peak RSS
bundle           1.33 GB         verify          0.94 s
                 98.7 B/point    artifacts       465,343 minted
resolution       85.7% of points have a cell of their own
```

Neither wall the plan expects — W1's Roaring round trip at 5×10⁷ members, W2's peak RSS ignoring
its budget — is near being reached at this scale.

**What the rung is served by:** [`../test_corpora/geonames/`](../test_corpora/geonames/README.md),
which carries the preprocessing, the declaration and the full account of what the source turned out
to be.

## 3. Rung 2 — Overture, built

[`../test_corpora/overture/`](../test_corpora/overture/README.md) carries the declaration, the
pipeline and the full survey. **The whole corpus is built and verified**, 73,631,092 places.

**Built, verified, and built again to prove the optimisation below changed nothing.**

```
prepare.py    divisions 36 s · join 1,811 s · entity ids 394 s · outputs 46 s
              points.parquet 3.09 GB · members-divisions 2.23 GB · members-taxonomy 293 MB
tessera build 23:16–26:12 wall · 18.9 GB peak RSS · exit 0
bundle        7,900,567,451 bytes — 107.3 B/point
verify        OK in 6.6 s — 1 partition, 1 view, 1 segment, high-water 73,631,092
artifacts     625,821 divisions · 2,097 taxonomy across 6 levels · 9 predicate
no artifact   3,285,234 taxonomy (4.5%) · 46,844 divisions (0.06%)
resolution    12.1% — 8,895,005 distinct cells
```

**Both walls the plan expected here did not fire.**

**W1 was never approached**, and that follows from the declaration rather than from luck. It needs a
whole-corpus root cluster over 5×10⁷ members; `boundaries/divisions` is a `nested` tree whose roots
are countries, so its largest membership is the US at ~16×10⁶, and `places/taxonomy` splits 73.6M
across 14 roots. **The wall is still there and this corpus does not ask the question** — it wants a
layer that declares one root over everything.

**W2 did not fire either**: 18.9 GB peak against the 47.3 GB the artifact campaign was killed at,
with no `--memory-budget` set. Part of that is this rung's own work (§3.3): consuming `resolved`
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

### 3.1 Four things the survey corrected in the plan

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

### 3.2 The predicate layer works, and the build's report said it did not

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

### 3.3 What the rung cost the build's own code

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

### 3.4 What is still open at this rung

- The whole corpus has not been run, so **W1 and W2 have not been met**. They are expected here.
- **A `nested` layer has no levels, so it has no zoom bound** — the other half of §5's first
  finding, at roughly 600,000 artifacts and with no zoom-to-level map to offer. Named in the
  declaration at the layer it applies to.
- The rung declares both a `nested` boundary layer and three indexed division columns, so the
  attribute-membership comparison the plan asks for is one declaration away. It has not been run.
- **`filter_postings`, 44% of the build, has never been investigated** (§3.0). It is the obvious
  next optimisation and `layers` is no longer where the effort belongs.
- §7.1's bar: the 0091 build-vs-ingest test, the oracle census, the write cycle, ingest rows/s, p99
  at three zooms and a screenshot. None attempted.
- One part of sixteen built: **55 s wall, 1.33 GB peak RSS, 484,326,539 bytes** (105 B/point),
  1,955 taxonomy artifacts minted beside 17,544 declared division artifacts. ⊘ Its
  `RESOLUTION LOST — only 6.5%` warning is the slice and not the corpus: `part-00000` is Latin
  America alone, 47 countries, inside a whole-world frame. The figure to hold against GeoNames'
  85.7% is the one the full run gives.

## 4. The machinery this campaign built

- **[`../test_corpora/`](../test_corpora/README.md)** — one directory per rung, in git: `prepare.py`,
  `corpus.toml`, `README.md`. Derived files go to `$TESSERA_LADDER/<rung>` (default
  `data/ladder/<rung>`), so the second volume is one environment variable rather than an edit to
  every script.
- **`test_corpora/common/projection.py`** — the frozen WGS84 → Web Mercator transform, unit square,
  **y south**. Checked against published values, against XYZ tile addresses (the only real test of
  the y direction), and against DuckDB, which agrees bit-for-bit. Its `TEST_VECTORS` are written as
  data so the eventual Rust can be checked against them.
- **`~/venvs/ingest`** — DuckDB and PyArrow, with `spatial` installed for rung 2's point-in-polygon
  join (§3). ⊘ Its Python is 3.10, so it has no `tomllib`; `~/venvs/projection` does.
- **`run_demo.sh --terms / --ranks / --label`**, and `custom` on ports of its own — see §6.

## 5. Cross-cutting findings

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

**A geographic corpus is reproducible, and an embedding corpus is not.** A projection is a pure
function, so a geographic rung built on a frame that later changes costs a rerun rather than the loss
`data/geometry.parquet` would be. This is why rung 1 did not wait on native projection, and it does
not transfer to rungs 3–5.

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

## 6. Problems found in tooling, and what was done

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

## 7. Open, and what is next

**Owner calls outstanding**

- The design pass on artifact response volume (§5, and the memo it points at).
- Whether `parent_edges`' two nulls need separating, and whether that is worth an issue.
- Whether this tracker is the campaign's status record or the campaign moves to issues.

**Rung 1 work not done**

- `places/containment` — the third layer, from `hierarchy.txt`, as a `nested` lineage. Needs a DAG
  walk and will meet genuine multiple parents, which is the polyhierarchy refusal for real rather
  than as the keying artefact rung 1 already dissolved.
- Everything in §2's ❌ rows: the 0091 test, the oracle census, the write cycle, p99 and a screenshot.

**Before rung 2**

- Rung 0, to confirm W1 and W2 reproduce and whether the pre-flight now refuses rather than being
  killed.
- The `spatial` extension, for the `division_area` point-in-polygon join.
- A decision on whether the artifact volume finding blocks a rung whose boundary layer is larger
  still.
