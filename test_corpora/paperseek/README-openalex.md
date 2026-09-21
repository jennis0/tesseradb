# PaperSeek + OpenAlex — the label track

**What this half of rung 4 is.** PaperSeek publishes 102,117,343 OpenAlex works with embeddings and
no labels; OpenAlex publishes the labels and no embeddings. This track is the join between them:
one pass over OpenAlex `works`, a six-column extract keyed by the work id, and the module the
vectors track calls to turn a slice of PaperSeek ids into a publication year, a work type, an OA
flag, a **licence** — the corpus's compartment — and a **topic** in OpenAlex's four-level tree,
which is the `topics/openalex` layer.

The interface between the two tracks is fixed in
`.superpowers/sdd/2026-09-02-rung-4-paperseek/interface.md`. This track delivers `extract.py` and
`openalex.py`; the vectors track delivers everything else and folds this file into the package
README at merge.

```bash
export TESSERA_LADDER="$PWD/data/ladder"
P=~/venvs/projection/bin/python

$P -m test_corpora.paperseek.extract           # the id set, the one scan, the extract
$P -m test_corpora.paperseek.openalex          # the tree's and the extract's figures
```

Every figure below is **measured** on 2026-09-02 against `paperseek-openalex/2026-08-27` and
`openalex/2026-08-27` on the SMB share unless it says otherwise, with another track staging 209 GB
off the same share throughout — so the throughput figures are contended and are a floor rather than
the share's capability.

## 1. The join, and what it matched

| | |
|---|---|
| PaperSeek ids, 53 chunks | **102,117,343**, all distinct, all `https://openalex.org/W<digits>` |
| OpenAlex `works` rows scanned | **504,861,414** across **2,428** parts — the whole snapshot on this share |
| **matched** | **98,925,699** — **96.87%** of PaperSeek |
| unmatched | 3,191,644 — 3.13% |

**The id set is integers.** Both sides spell the id as the same URL, and the part after the `W` is a
number, so membership is a `uint64` `searchsorted` over a sorted 817 MB array — exact, with no
hashing and no collisions to argue about. Building it read the 53 chunks' `id` column in **89 s**.

**3.13% of PaperSeek is not in this OpenAlex snapshot, and that is a real gap rather than a bug in
the join.** The id spellings match exactly on both sides, the scan covered every part, and the
matched ids are distinct — so the missing works are ids OpenAlex has merged away or withdrawn
between PaperSeek's vintage and 2026-08-27. Not investigated further; it is **stated so the vectors
track can carry it into the corpus**, where those works resolve to nulls throughout: no topic, no
licence, and therefore `public` under the view's default.

## 2. The scan — 32 minutes, not 54

| | measured | the probe's model |
|---|---|---|
| wall | **1,937.9 s (32 m 18 s)** | ≈3,265 s (54 m) |
| projected bytes moved | **5.98 GB** | ≈32.6 GB |
| aggregate rate | **3.09 MB/s** | 9.98 MB/s |
| extract, combined | 163.8 s → **402.7 MB** on disk | 6.6–10 GB |

**The bytes fell 5.5x because the projection is by leaf, not by struct.**
`probes/2026-09-02-rung-4-share-reads/` projected the six *top-level* fields and measured ~10 MB/s,
which is where its 54-minute model comes from. Three of those six are structs, and reading a struct
reads every leaf under it — `best_oa_location.source` alone carries eleven scalars and two lists
nobody here wants. Parquet stores leaves, and pyarrow projects them by dotted path, so this scan
asks for `primary_topic.id`, `open_access.is_oa` and `best_oa_location.license`. On one part
(`updated_date=2025-11-06/part_0782`, 291,242 rows) that is **3.65 MB against 16.38 MB compressed**.

**The MB/s got *worse*, and the wall still improved.** 3.09 against the probe's 9.98: what is left
after the pruning is a handful of small leaf chunks per row group, so the read is even more
round-trip-bound than the probe's was, and it ran against a concurrent 209 GB staging job. The
figure that matters is the wall, and it is 1.7x better than the model for a fifth of the bytes.
Neither number is the share's capability — both are contended.

**The extract is 402.7 MB, not the 6.6–10 GB the probe budgeted.** That model scaled bytes from the
wide projection and assumed the id stayed a 33-byte URL string. Here the id is the `uint64` after
the `W`, and `type`, `licence` and `topic_id` are dictionary-encoded zstd, so the whole 98.9M-row
extract is 4.1 bytes per row.

**Resumable per part**, which was worth having: a shard per matching part plus a JSONL ledger, so a
dropped share costs the part in flight and nothing else. 2,195 of the 2,428 parts matched something.

## 3. The licence — the corpus's compartment

`best_oa_location.license`, lowercased and trimmed. **Ten distinct values, and they are the closed
vocabulary** `OpenAlex.licences()` returns, in this order:

| licence | works | share of matched |
|---|---|---|
| *(none)* | 76,110,809 | 76.94% |
| `cc-by` | 14,028,593 | 14.18% |
| `cc-by-nc-nd` | 2,864,246 | 2.90% |
| `cc-by-nc` | 2,127,041 | 2.15% |
| `other-oa` | 1,503,856 | 1.52% |
| `cc-by-sa` | 904,691 | 0.91% |
| `cc-by-nc-sa` | 800,620 | 0.81% |
| `public-domain` | 464,518 | 0.47% |
| `cc-by-nd` | 112,934 | 0.11% |
| `publisher-specific-oa` | 8,329 | 0.008% |
| `mit` | 62 | 0.00006% |

**Licensed: 22,814,890 works, 23.06% of the matched corpus.** `is_oa` is true on 42,549,161 —
nearly twice as many — so *open access* and *carries a licence string* are different populations and
a figure quoting one against the other would be wrong.

**The principal ladder this gives the rung** (the vectors track writes the declaration; these are
its numbers):

- **no terms at all** — the 76,110,809 works with no licence, plus the 3,191,644 PaperSeek ids
  OpenAlex no longer carries, which take the same default: **79,302,453 points, 77.7% of the corpus**.
- **`cc-by`** — the largest compartment, **+14,028,593**.
- **every key** — all 102,117,343.

**`mit` carries 62 works and stays in the vocabulary.** A closed vocabulary that omitted it would
refuse the build over those 62 rows, which is what closed is for.

**The dataset README's "CC-BY on ~29.2M works" is not this number** and is not comparable: it is
over all 510M works in the snapshot, and this corpus is 98.9M of them.

**Several licences per work: 7.4% of the licensed ones, and this extract keeps one.** The interface
column is single-valued because `best_oa_location` is one struct — but `locations[]` is a list, and
a work's locations can disagree. Measured on three parts spread across the partition range
(`extract.py --licence-sample 3`, 173,913 works): **21,382 carried a location licence and 1,583 of
them — 7.4% — carried more than one distinct value.** OpenAlex's own choice of best location
resolves it, and that choice is not always the most permissive one: the commonest disagreement in
the sample is `cc-by, other-oa → other-oa` (352 works), where a work with a CC-BY location is
labelled `other-oa`. **What this track did:** took `best_oa_location.license` unchanged, which is
OpenAlex's own answer to the question, and recorded the 7.4% here rather than inventing a
most-permissive rule the publisher does not state. A work labelled `other-oa` is in a *narrower*
compartment than its CC-BY location would put it in, so the error is in the fail-closed direction.

## 4. The topics

OpenAlex publishes **4 domains → 26 fields → 252 subfields → 4,516 topics** as its own CC0 entity
tables, and every count above is confirmed against `topics/`, `subfields/`, `fields/` and
`domains/` rather than quoted from the dataset card. A topic sits in exactly one subfield, a
subfield in one field, a field in one domain — which is what makes this `tiered` and not the `dag`
rung 3's MeSH descriptors needed.

| | |
|---|---|
| works with a `primary_topic` | **98,581,482** — 99.65% of matched, **96.54% of all PaperSeek** |
| topic ids in the extract that the tree does not know | **0** |
| tree nodes with at least one work in the corpus | **4,516 / 252 / 26 / 4** — **all of them** |
| member rows at full scale | **394,325,928** (four per topic-bearing work) |
| member file at full scale | ≈**730 MB** — modelled from 14.3 MB per 7.7M rows measured on chunk 0 |

**Every node in the published tree draws something here**, so the layer is 4,798 artifacts and the
`_seen` filter drops nothing at full scale. It still exists, and it is what makes the artifact count
a corpus figure on any smaller slice — the 200,000-row smoke below declares 4,371.

**`publication_year` runs 1000 to 2050** and is left exactly as OpenAlex publishes it. Both ends are
nonsense as publication dates; ignore-and-report is the rule for a demo corpus's attribute column,
and a filter over a year attribute is the vectors track's to declare.

## 5. `openalex.py` — the module, and the calls that shaped it

| operation | measured |
|---|---|
| `OpenAlex()` — tree + the whole 98.9M-row extract | **9.5 s**, **5.66 GB** peak RSS |
| `resolve` over 2,000,000 ids | **1.42 s — 1,413,308 rows/s** |
| `write_layer` over the same slice, 7,712,216 member rows | **0.60 s — 12,855,054 rows/s** |

**`resolve` is a sorted-array probe, not a hash join.** The extract is written sorted by id, so a
slice resolves in one `searchsorted` over a `uint64` array with no per-slice hash build. It is the
vectorised shape the interface asks for, and cheaper than the Arrow join the interface names.

**Membership is stated at four levels, not closed upward.** Rung 3's MeSH layer had to close
upward because indexing assigns the most specific heading; here a work's parent chain is single and
known, so `write_layer` writes the four rows directly. Containment then holds by construction,
which is what `require_member_visibility = { count = 50 }` needs for roll-up: a child's masked count
is never larger than its parent's, so a failing child always leaves a served ancestor above it.

**Three declarations differ from rung 3's MeSH layer, and each has a reason:**

- **`computed = ["centroid", "box", "hull"]`** where MeSH computes nothing. A descriptor's members
  are spread across the whole layout, so a centroid places a label on nothing; a topic in an
  embedding space should be compact, which is what the embedding is for. **This is an expectation
  and not a measurement** — the smoke's layout is a hash and says nothing about it. The vectors
  track measures the median box share on the real layout, and the layer is withdrawn if it draws
  nothing, on the rule that withdrew two of the arXiv rung's taxonomies.
- **`hierarchy = { kind = "tiered", prune_children = true }`** with four `[[layer.levels]]`, where
  MeSH is a `dag` with none. Every node here has one parent.
- **`visibility = "public"`, `artifact_visibility = { default = "inherited" }`, supplied content
  `inherited` and not `all`** — identical to MeSH's, for the identical reason: a display name of a
  published CC0 taxonomy node asserts nothing about any work, would read the same if the corpus were
  empty, and under `all` would have to carry a generating set that is tested against a claim it did
  not generate (C28).

The zoom ranges overlap by one step at each seam, following geonames' `admin/hierarchy`: 0–4, 3–8,
7–12, 11–16 over the sixteen the tile addressing caps at, so a viewport at any depth is answered at
one or two levels rather than none.

## 6. The smoke build

The first 200,000 works of `chunk_0` — entity ids 0..199,999 in the interface's entity space — with
x and y from a **SplitMix64 of the work's own id**. The vectors track owns the layout and its
`prepare.py` did not exist when this ran, so the geometry is uniform noise: it proves the plumbing
and it proves **nothing** about whether a topic is compact.

```
tessera check --payloads   OK — 4 sources, 1 view, 1 vocabulary, 3 attributes, 1 layer
tessera build              OK — 200,000 items, 10 terms, 200,000 pairs, 12,872,196 bytes,
                                50 MB peak RSS, 0 unclustered member rows
tessera verify --deep      OK — 1 partition, 1 view, 1 segment, 200,000 rows
```

| | |
|---|---|
| resolved | 191,188 of 200,000 (95.6%); 191,143 with a topic |
| artifacts | **4,371** — 4 domains, 26 fields, 249 subfields, 4,092 topics |
| member rows | 764,572 |
| access | 54,845 points carried a licence term; 145,155 took the declared `public` default |
| containment | no violation at any of the four levels |

**Every level came back `everywhere`** — 1.000, 1.000, 0.984, 0.918 by level — which is exactly what
a hash layout must produce and is the reason no compactness claim is made here.

Two defects the checker caught that reading had not: `number` is not a declarable attribute type
(`i32` is), and a keyword column sliced out of the chunk's own `large_string` id column arrives as
`large_string`, whose width is baked into every row and must match the declaration.

`smoke.py` is **deleted** now it has done its job, per the brief; it is in commit
`cef1bf5a` on `campaign/rung-4-openalex` if the build needs reproducing before `prepare.py` lands.

## 7. What this track did not do

- **No compactness measurement.** §5's expectation that a topic draws a small box is untested — it
  needs the real layout, and it is the vectors track's, and it is what decides whether the layer
  stays.
- **The 3.13% miss is not explained**, only counted. Chasing it means reading OpenAlex's merge
  records, which is a second acquisition.
- **`type`, `is_oa` and `publication_year` are carried and not declared here.** `resolve` returns
  them; which become attributes is the declaration's business.
- **The dataset README on the share was corrected**, not rewritten: a dated *Corrections* section
  naming the licence field's real path and the 2,428 / 504,861,414 on-disk counts against the
  manifest's 2,446 / 510,372,821.
