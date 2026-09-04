# PaperSeek + OpenAlex — the ladder's largest rung, and the build that twice could not finish

**102,117,343 OpenAlex works**, each with a 1024-dimensional Stella V5 embedding a third party
computed from its title and abstract and released on Hugging Face, joined to OpenAlex itself for a
four-level topic tree, a publication year, a work type, an open-access flag and — the rung's
compartment — **a licence**. 2.8× rung 3's rows at 1.33× the width, one view, and the abstracts
**on**.

It is a **demonstrator and a speed benchmark** (owner rulings 2026-09-01 and 2026-09-02: this
corpus tests Tessera's speed and memory, not the UMAP pipeline; layout quality matters only as far
as the demo looks good). Recall against an exact neighbour search is not measured and layout
fidelity is not judged.

**The rung was built to put a bundle past the box's memory**, and nothing about the declaration was
trimmed to make it fit — the abstracts are 118.9 GB of characters uncompressed and they are indexed
as text, because a corpus trimmed to fit deletes the finding. **The answer arrived one stage
earlier than expected: it is the build, not the server, that met the wall.** The corpus prepares in
43.8 minutes; `tessera build` reached the abstract text index and stopped making progress there,
and then did the same one stage later at the record blob. Both were random walks over an arena two
and a half times the size of the box, both are fixed, and **on 2026-09-04 the whole corpus built in
2 h 56 m to a 70.78 GB bundle**. Everything below the *Measured* heading is what that cost.

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

**The raw output is [`../../probes/2026-09-03-rung-4-build-stall/`](../../probes/2026-09-03-rung-4-build-stall/README.md)**
— every log, every RSS and fault sample, and the four drive results — because the headline here is a
negative one, and a negative result that cannot be re-read is an assertion.

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

### The whole corpus — 102,117,343 works

`prepare.py --sample 0 --drop-vectors` **43.8 minutes** at **18.44 GB** peak `VmHWM`, on a box
otherwise idle.

| step | | |
|---|---|---|
| route `knn` | **1,487 s** | gather the 1.5M fit set off the 209 GB memmap 216 s · CAGRA build 16.6 s · graph search 16.3 s (92,009 q/s) · UMAP 27.2 s · **place 102,117,343 rows 1,208 s** |
| k-means | 48 s | cuML over the 102M × 2 layout, whole rather than sampled, k = 256 |
| vectorise titles | 83 s | 104,680 candidate terms over a 4,000,000-title sample |
| titles | 2 s | 254 of 256 cells titled |
| **resolve, layer and points** | **980 s** | the one pass: the OpenAlex join, 394,325,928 topic member rows streamed, and 52.2 GB of `points.parquet` written — at a **flat 18.4 GB** of `VmHWM` from the first row group to the last |

99.98% of the fit set's rows came back with themselves first. 256 k-means cells hold 187 …
1,056,873 works (median 459,697).

**The OpenAlex join, over the whole corpus:**

| | |
|---|---|
| ids matched | 98,925,699 of 102,117,343 (**96.9%**) — 3,191,644 unmatched, null throughout and carrying `unlicensed` |
| with a topic | 98,581,482 (96.5%) |
| with a publication year | 98,894,561 (96.8%) |
| open access | 42,549,161 (41.7%) |
| **with a real licence** | **22,814,890 (22.3%)** |
| topic-layer member rows | **394,325,928** over 4,798 artifacts across four levels |
| clustering member rows | 102,117,343 over 256, plus 42,672 ranked generating-set rows |

**The principal ladder** — the ruling's whole point, and the figure every count below is against:

| principal | terms | visible |
|---|---|---|
| no terms | — | **0** |
| `cc-by` | 1 of 11 | **14,028,593** (13.7%) |
| all licences | 11 of 11 | **102,117,343** |

The rest of the roster, for composing a ladder between those: `unlicensed` 79,302,453 ·
`cc-by-nc-nd` 2,864,246 · `cc-by-nc` 2,127,041 · `other-oa` 1,503,856 · `cc-by-sa` 921,373 ·
`cc-by-nc-sa` 809,116 · `public-domain` 455,882 · `cc-by-nd` 106,419 · `publisher-specific-oa`
7,902 · `mit` 62. `licence-ranks.json` and `licence-terms.txt` beside the corpus carry the same
list, ranked, which is what a measurement driver composes principals from.

### ⊘ The whole-corpus build does not converge on this box

**This is the rung's finding and it is a negative one.** `tessera build --stage-timings` reached the
text index and stopped making useful progress there. It was neither refused nor killed: it is still
running, and it is stalled on I/O rather than computing.

| stage | wall | `VmHWM` |
|---|---|---|
| `source_ids` | 2.3 s | 1,584 MiB |
| `dictionary` | 18.1 s | 1,832 MiB |
| `geometry_read` … `external_ids` | 27 s total | 6,274 MiB |
| `attribute_tail` | **759.6 s** | 24,409 MiB |
| `layers` | **84.5 s** | 24,409 MiB |
| `text_index` | **> 4 hours and counting** | — |

Every attribute landed on every entity it should: `publication_year` 98,894,561, `type`
102,117,343, `is_oa` 98,925,699, `openalex_id` 102,117,343, `title` 102,117,343, `abstract`
102,117,343, and **no source row named an entity the build did not load**. `title`'s text index
finished — 2.4 GB of it. It is the abstract column the stage does not get through.

⊘ **The box was not quiet, so the rates below are not a clean measurement.** Across the whole stall
window (03:13–06:25) another track's 3.2×10⁷-row MedCPT base build ran on the same disk from 03:48,
and two serve batteries were driving cgroup `memory.reclaim` eviction beside it. The stall is real
and its mechanism is not in doubt — a 119 GB arena against 47 GB of RAM, walked rather than streamed
— but **the fault rate, the PSI figures and the four-hour wall all include contention**, and none of
them should be quoted as this build's cost alone.

**What it is doing, measured rather than inferred**, sampled four hours in:

| | |
|---|---|
| threads in uninterruptible sleep on `folio_wait_bit_common` | **11 of 13** |
| CPU in the kernel | **93%** (`stime` 1,336,792 ticks against `utime` 251,644) |
| major faults | **~480 /s**, 6.2×10⁶ so far |
| PSI `io` `full` avg300 | **60.8%** — the process group is fully stalled on I/O three fifths of the time |
| PSI `memory` `full` avg300 | 21.2% |
| **anonymous high-water** | **5.19 GB** |
| abstract spill runs written | 42, ~22 MB each, at roughly one every four to eight minutes |

**The mechanism is the mapped design meeting a column bigger than the box.** The build preallocates
one arena file per text column and maps it: `.build-tmp/column-13.arena` is **137,438,953,472 bytes
— 128 GiB exactly** — against 47 GB of RAM. The abstract text pass walks that arena, and the page
cache cannot hold enough of it, so nearly every access is a major fault. `--memory-budget` does not
reach this: the anonymous high-water is 5.19 GB, so the build is nowhere near a budget it could
respect, and what binds is the *file* the design maps rather than the heap the budget models.

⊘ **`probes/2026-09-02-text-peak-split/` extrapolated the wrong quantity, and said so.** Its
"abstracts are a run, not a build" conclusion is correct about *memory* — the anonymous figure it
predicted, ~6 GB for the abstracts' own share at 10⁸, is close to the 5.19 GB measured here — and
that is exactly why it does not predict this. The probe measured to 10⁷, where the arena is ~13 GiB
and fits; the wall it named as "a disk question and a wall-clock question, not a memory one" turns
out to be a **page-cache** question, which is neither of the two it separated.

⊘ **The build was not patched to get past it** (the brief's instruction: report rather than patch).
No `--memory-budget` arm was tried, the declaration was not trimmed, and the abstracts were not
dropped — each of those would answer a different question from the one the rung was built to ask.
The process was stopped after 3 h 25 m and its partial bundle deleted.

### Since: two stalls fixed, and the wall is now the disk

**Both random walks this rung found have been removed, and the 10⁸ build has still not been run.**

| | |
|---|---|
| `text_index` at 10⁸ | *over four hours, unfinished* → **2,371.9 s (39.5 min)**, 4,131 major faults over the whole stage and 153.3 GiB read against a 128 GiB arena. [`../../probes/2026-09-03-text-arena-streaming/`](../../probes/2026-09-03-text-arena-streaming/README.md) |
| `record_blob` at 10⁷ under a 4 GB cap | *> 2,134 s, unfinished*, 2,348 GiB read for a 10.2 GiB arena → **64.31 s**, its uncapped wall. [`../../probes/2026-09-03-entity-ordered-arena/`](../../probes/2026-09-03-entity-ordered-arena/README.md) |
| what the second fixed | the attribute join's **scatter** — resolved in source-id order, written at signature-then-Morton entity indices — and, behind the switch `--arena-order`, an arena filled in entity order by a second decode of the source's prose |
| the bundle | **byte-identical** under every arm, at 10⁶ and 10⁷ |

⊘ The 10⁸ build was then refused by the **disk** pre-flight — 274.5 GB needed against 257.7 GB
free, 16.3 GiB short, on a volume 209 GB of which was another session's `target/debug`. With that
reclaimed it ran, and it finished.

### 2026-09-04 — the whole corpus builds

**102,117,343 rows, the same declaration, 2 h 56 m 18 s, a 70.78 GB bundle, `verify --deep` clean.**
`docs/ingest-campaign.md` §4b carries the full stage table, the bundle breakdown and the battery;
this is the rung's own summary.

| | |
|---|---|
| `tessera build --arena-order auto` | **10,578.4 s**, peak `VmHWM` **29,239 MiB**, anonymous high-water **5,047 MiB** |
| the stages that bend | `attribute_tail` **6,369.1 s** · `text_index` **2,233.9 s** (57,637,877 terms) · `record_blob` **905.4 s** · `filter_postings` **762.9 s** |
| `verify --deep` | clean in **74.24 s** at 3.22 GB — 1 partition, 1 view, 1 segment, 102,117,343 rows, 12 terms |
| bundle | **70.78 GB**: `attrs` 66.22 GB (`record` **44.77 GB**, `abstract` **17.61 GB**, `title` 2.50 GB, `openalex_id` 916 MB, `publication_year` 416 MB) · `views` 3.06 GB · `entities` 817 MB · `members` 487 MB · `row-column` 204 MB |
| served under `MemoryMax=24G` on 8131–8133 | **`oom_kill` 0**, `memory.peak` exactly at the cap, 110,266 reclaim-at-max events, open **98.9 s** |
| the ladder | **0 / 14,028,655 / 102,117,343** across no terms, `cc-by` (+`mit`, 62 pairs) and all eleven keys — hot p50 at zoom 0 of 1.13 / 81.2 / 115.8 ms |
| `match abstract:"network"` | **134.9 ms** server-side median, hot |

**88% of the bundle is prose** — the record blob and the abstract index are 62.4 GB of 70.8 GB.
The 10⁷ prefix projected ~76 GB at two thirds prose; the whole corpus is 70.78 GB at seven eighths,
because the prefix's abstracts are shorter than the corpus's.

**`record_blob` is what the entity-ordered arena bought**: 905.4 s at **zero major faults a second**,
against over four hours making no progress and 52 MB written at 56 KB/s. **What it cost is the
join**: `auto` chose `entity` — 123,869 MiB of string payload against a 16,254 MiB share — so the
source's prose is decoded twice and the arena written by entity, and `attribute_tail` went from
704.7 s to 6,369.1 s. ⊘ Nine times the join at 10⁸ against 2.04× at 10⁷, and the extra is the
scatter rather than the decode: the join's chunk buffer does not grow with the corpus, so the arena
is written as ~54 interleaved ascending runs here against six at 10⁷.

⊘ **The box carried rung 5's share passes and GPU work throughout.** No other `tessera build` and no
serve battery ran; the disk was this build's alone.

### 2026-09-04 — the same bundle in a third of the time

**Byte-identical output in 1 h 09 m 30 s**, on branch `build/prose-extents`: 46 files compared
against the bundle above, none differing but `MANIFEST.json`'s `created_at` and the `CURRENT` that
carries its digest. `docs/ingest-campaign.md` §4c carries the stage table and the 10⁷ counterweight;
the design is [`build-prose-extents.md`](../../docs/design/build-prose-extents.md).

| | before | this run |
|---|---|---|
| `tessera build --arena-order auto` | 10,578.4 s | **4,169.9 s** |
| `attribute_tail` | 6,369.1 s | **890.2 s** |
| `text_index` | 2,233.9 s | **1,688.4 s**, the same 57,637,877 terms |
| `record_blob` | 905.4 s | **777.4 s** |
| `filter_postings` | 762.9 s | **482.4 s** |
| peak `VmHWM` | 29,239 MiB | **19,590 MiB** |
| bundle | 70,783,029,628 B | **70,783,029,628 B**, byte for byte |

A `text` column's prose is no longer placed at an entity index. Each chunk the join stages is
sorted by entity already, so each chunk of each text column is written out as one record-blob
extent; the text index reads the extents in block windows and the record blob merges them. The only
column with an arena left is `openalex_id` at 1,456 MiB, so `--arena-order auto` chooses `arrival`
and the second decode that cost 6,369.1 s above does not happen.

⊘ **Rung 5's serve batteries ran on the same disk throughout**, where the run above had the disk to
itself, so these walls are an upper bound. The byte equality does not depend on it.

### The bracket — 10,000,000 rows, and the declaration proved end to end

The same inputs, built as a prefix (`--limit 10000000`, with the two member files and their artifact
rosters cut to match). **This is where the corpus stops being a demonstration of the stall and starts
being a corpus again**, and it says where the turn is: at 10⁷ the abstract arena is ~13 GiB and fits
in page cache, and the text index that would not finish at 10⁸ takes **178 seconds**.

| | |
|---|---|
| `tessera build --limit 10000000` | **545 s**, **7.44 GB** bundle, anonymous high-water **1,685 MB** against **15,705 MB** of `VmHWM` |
| the stages that bend | `attribute_tail` 236.5 s · `text_index` **178.4 s** · `record_blob` 74.0 s · `filter_postings` 32.1 s |
| `verify --deep` | clean in **5.45 s** at **342 MB** — 1 partition, 1 view, 1 segment, 10,000,000 rows |
| bundle breakdown | `attrs` 6.6 GB (of which `record` 4.4 GB, `abstract` 1.8 GB, `title` 242 MB, `openalex_id` 93 MB, `publication_year` 39 MB) · `views` 286 MB · `entities` 77 MB · `members` 54 MB · `row-column` 20 MB · the rest under 1 MB |

**Two thirds of the bundle is prose**: the record blob and the abstract index together are 6.2 GB of
7.44 GB. Scaled to the whole corpus that is a **~76 GB bundle**, which is the number the rung was
built to produce and the one it did not get to.

⊘ **The prefix is not a uniform sample.** `--limit` keeps `entity_id < 10⁷`, which is the first five
chunks in staging order, so every figure in this section is *those* works — not a tenth of the
corpus drawn at random. It brackets the build's cost; it does not stand in for the corpus.

⊘ **Six of the 256 k-means cells hold no member in the prefix and were dropped from its roster**,
and the generating sets were redrawn from the prefix. A cluster whose sample lay above the cut would
otherwise publish content with an empty generating set, which the build refuses — rightly: under
`require_member_visibility = "all"` an empty set is satisfied by everyone. `prepare.py` now declines
to declare an empty cell for the same reason.

### Served, and the 24 GiB cap is not the constraint here

`tessera serve` on 8131 against the 7.44 GB bundle, driven by `drive.py` — three principals × a
25-request pan sequence over five zoom levels, a `match` on `abstract` with a common and an absent
token, and twenty item and ten artifact drill-downs. Then the same sequence again under
`systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0`.

| request kind | n | uncapped p50 | uncapped p99 | 24 GiB p50 | 24 GiB p99 |
|---|---|---|---|---|---|
| viewport pan, *no terms* | 25 | 0.06 ms | 2.1 ms | 0.05 ms | 3.6 ms |
| viewport pan, `cc-by` | 25 | 0.61 ms | 16.9 ms | 0.64 ms | 18.9 ms |
| viewport pan, all licences | 25 | 0.72 ms | 57.0 ms | 0.82 ms | 61.8 ms |
| `match abstract:"network"` | 5 | 9.28 ms | 12.5 ms | 7.70 ms | 7.79 ms |
| `match abstract:` absent token | 5 | 0.70 ms | 1.03 ms | 0.38 ms | 0.47 ms |
| item drill-down | 20 | ~0 ms | ~0 ms | ~0 ms | ~0 ms |
| artifact drill-down | 10 | ~0 ms | ~0 ms | ~0 ms | ~0 ms |

**It survives, and the cap never binds.** `memory.events` shows `oom 0`, `oom_kill 0`; the scope's
`memory.peak` over the whole drive is **242.5 MB**, a hundredth of the cap. **85 of 85 count-bearing
responses are identical between the capped and uncapped runs.**

**Anon at rest, after open and before any request: 166 MB.** Rung 3's server on a comparable bundle
sat at **2.06–2.17 GB**, and `probes/2026-09-02-serve-under-memory-cap/` attributed that floor to
the `mesh/descriptors` DAG artifact-projection build now paid at open, over 1.66×10⁹ member rows.
This rung's `topics/openalex` is a four-level tiered layer over 38.8×10⁶ member rows in the prefix,
and its floor is a **twelfth** of rung 3's. That is consistent with the probe's guess that the floor
scales with the membership rather than with the corpus, and it is the first second data point.

**Counts move with the mask, and the ruling holds on a built bundle**: 0 / 1,631,343 / 10,000,000
visible at zoom 0 across *no terms*, `cc-by` and all eleven keys. `match abstract:"network"` matches
404,853 of 10,000,000 without moving the visible count; an absent token matches 0.

### Layer spread at 10,000,000 — both layers draw, and the tree tightens

| | median | p90 | max | under 5% |
|---|---|---|---|---|
| `clusters/kmeans` (239 of 250 with ten members or more) | **0.069%** | 0.17% | 0.48% | 100% |
| `topics/openalex` (330 sampled across four levels) | **2.08%** | 8.56% | 23.1% | 77% |
| *by level:* domain (4) · field (26) · subfield (150) · topic (150) | 8.23% · 4.93% · 2.52% · **1.10%** | | | |

⊘ **The build's `everywhere` fraction disagrees, and the two measure different things** — rung 3's
finding, repeated here more sharply. Every level of `topics/openalex` reports **1.000 everywhere**
at 153.0, 147.8, 115.0 and 53.0 blocks an artifact, against the clustering's 0.448 at 3.9 blocks: a
topic whose members occupy 1.1% of the map is still spread across enough of row space that no
tile-index node bounds it. Compactness in the map and boundability in row space are different
properties, and this corpus separates them at every level of one layer.

**Neither layer is withdrawn.** Against the taxonomies withdrawn at rungs 1 and 2 (medians 13.7% and
9.6%) even the domain level is compact, and the topic level — the one a client is served at zoom
11–16 — is an order of magnitude tighter than either.

### What is on disk

`$TESSERA_LADDER/paperseek` is **108 GB**, and `paperseek-1m` beside it is 1.2 GB.

| | |
|---|---|
| `staging/` | **51 GB** — the 53 chunk parquets (49 GB) and the OpenAlex track's extract, id set and parts (2 GB) |
| `points.parquet` | **49 GB** |
| `bundle-10m/` | 7.0 GB |
| `layout-knn.npy` | 780 MB — the whole corpus's positions, so a rebuild needs no GPU and no vectors |
| the two member files | 863 MB |
| `limited/` | 205 MB — the 10⁷ prefix's cut member files and rosters |

**`staging/vectors.f16` is gone** — 194.8 GiB, deleted by `prepare.py --drop-vectors` the moment the
layout was written, because nothing after the layout reads it. So is the stalled build's
`.build-tmp` (151 GB) and its partial bundle. A run that wants a *new* layout must re-stage, which
is 2.7 hours; a run that wants a new corpus over the same layout reads `layout-knn.npy`.

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv and MedCPT rungs; `requirements.txt` points at the arXiv rung's. `drive.py` is the one
exception and runs on the system `python3`: it needs `requests` and `pyarrow` and nothing the
rung's environment carries.
