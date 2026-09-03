# TreeOfLife-200M — the metadata pass and the GBIF join

**What this half of rung 5 is.** TreeOfLife-200M publishes 233,055,986 image embeddings with taxonomy
and provenance, sorted by taxonomy, and no coordinates. A quarter of its rows are sourced from GBIF,
which does publish coordinates, keyed by its own occurrence id. This track is the join between them:
one pass over the 666 TreeOfLife files projecting everything but the embedding, one pass over GBIF's
8,369-part occurrence snapshot keyed by a hash of that id, and the figure the campaign plan called
"the first probe to run" — what share of the corpus can sit in a geographic view at all.

The interface between the two tracks is fixed in
`.superpowers/sdd/2026-09-03-rung-5-treeoflife/interface.md`. This track delivers `stage.py` and
`gbif_join.py`; the vectors track delivers everything else and folds this file into the package
README at merge.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder
P=~/venvs/projection/bin/python

$P -m test_corpora.treeoflife.stage       # metadata.parquet: scan (share), then combine (local)
$P -m test_corpora.treeoflife.gbif_join   # gbif-ids: ids, then scan (share), then combine (local)
```

Every figure below is **measured** on 2026-09-03 against `treeoflife-200m/2026-08-27` and
`gbif/2026-06-01` on the SMB share unless marked otherwise, with another track (rung 4's GPU build)
staging tens of gigabytes off the same share for part of the run — so the scan's throughput is a
contended figure, a floor rather than the share's capability, the same caveat rung 4's own
`README-openalex.md` records for its OpenAlex scan.

## 1. The metadata pass

**233,055,986 rows, matching the card's figure.** 666 files, one row group per file in the output,
16 non-`emb` columns, `row` (u32, `entity_id`) and `file` (u16) stamped from arithmetic rather than
read (`stage.py`'s module docstring). Resumable per file (a shard under `staging/metadata-parts/`
plus a JSONL ledger); the run here needed one resume after an unrelated interruption, at file 3.

| | |
|---|---|
| wall (this run, 663 of 666 files) | **2,188.0 s (36 m 28 s)** |
| **`metadata.parquet`** | **8,828,110,265 B ≈ 8.83 GB** — the interface's "~7 GB" estimate, high by about a quarter, most likely the demo taxonomy strings (`scientific_name`, `identifier`) running longer on average than budgeted |

**Per-`source_dataset`:**

| source | rows | share |
|---|---|---|
| `gbif` | 222,660,750 | 95.53% |
| `bioscan` | 5,150,850 | 2.21% |
| `eol` | 5,206,441 | 2.23% |
| `fathomnet` | 37,945 | 0.02% |

The card's schema lists four sources; all four are present, and `gbif` is not merely the largest —
it is nineteen times the other three combined. Everything this join track does is about that 95.53%.

**Null rates by rank** (of 233,055,986):

| rank | null rate | distinct values |
|---|---|---|
| `kingdom` | 0.0% (34 rows) | 191 |
| `phylum` | 0.04% | 1,265 |
| `class` | 0.70% | 4,169 |
| `order` | 0.81% | 8,357 |
| `family` | 1.14% | 27,166 |
| `genus` | 3.67% | 138,171 |
| `species` | 10.26% | 240,084 |

Nulls grow monotonically down the tree, as expected of a taxonomy resolved to varying depth — a
tenth of rows carry no species-level identification at all, but kingdom is all but universal.
**473 distinct publishers.**

## 2. Building the id set — and the RAM bug this track shipped and then fixed

**The id set is `source_id` of the 222,660,750 `gbif` rows, hashed with `pandas.util.hash_array`
into a `uint64`, sorted.** Both `source_id` and GBIF's `gbifid` are plain digit strings on this
vintage (checked directly against a sample, not assumed from either schema), so a numeric cast would
work today — the join is written against the string column each side actually declares, the way
PaperSeek's `W<digits>` OpenAlex ids still went through a parsed, asserted key rather than a bare
cast (rung 4's `extract.py`).

**The first version of `build_ids` held the whole column and was killed at 24.8 GB RSS** — on a box
where a sibling track already held ~22 GB and this track's budget is ~8 GB — forty seconds into a
`dataset.to_table(...).combine_chunks().to_numpy(...)` call. The fix streams `metadata.parquet` one
row group at a time (a row group is one source file, ≤350,000 rows), hashes only that row group's
`gbif` strings, and writes the `(hash, row)` pair into a preallocated **structured** numpy array —
sorted **in place** with `ndarray.sort(order="hash")` at the end, rather than a separate `argsort`
plus two independent gathers, which on its own first attempt (after the streaming fix) still peaked
at **11.47 GB**: the streaming loop itself held a measured, flat 2.75 GB throughout, and the *sort
step* was where the second version blew the budget, not the read. The structured-array sort brought
the full-scale peak to:

| step | peak RSS | wall |
|---|---|---|
| streaming loop (666 row groups, local disk) | 2.75 GB | 90 s |
| **full run** (streaming + sort + write) | **5.44 GB** (5,436,088 KB, `/usr/bin/time -v`) | 410.9 s (6 m 51 s) |

**152,130,702 distinct hashes** among the 222,660,750 `gbif` `source_id`s — a third of them repeat:
one GBIF occurrence backs several TreeOfLife images (a sample file that is entirely `gbif`,
`train-00030`, has 350,000 rows over only 225,527 distinct ids). The id set kept is a multiset, one
`(hash, row)` pair per row, not a deduplicated set — `expand_ranges` in `gbif_join.py` turns a
`searchsorted` hit into every row that shares the hash, verified on synthetic duplicate ids before
the real scan ran.

`gbif-ids.parquet`: **2,122,233,588 B ≈ 2.12 GB**, `hash` (u64) and `row` (u32) only — no strings, on
purpose: the wanted set stays resident for the whole ~6.6 h GBIF scan below, and 222,660,750 Python
string objects for that long was the other way this stayed over budget even after the build fix.

## 3. The GBIF scan — 6 h 41 m against a ~2–3 h model, and why

**8,369 parts, all scanned, matched by hash.** Resumable per part (a shard under `staging/gbif-parts/`
plus a JSONL ledger); 60 parts were scanned in an earlier smoke and skipped on the full run.

| | measured | the brief's model |
|---|---|---|
| wall (sum of part seconds) | **24,070.6 s (6 h 41 m)** | ≈2–3 h |
| bytes moved | 277,575,866,524 B ≈ 277.6 GB | — |
| aggregate rate | **11.53 MB/s** | ~33 MB/s (one part in ~1 s, the brief's figure) |
| peak RSS throughout | **6.33 GB**, flat from the first hundred parts to the last | ~8 GB budget |

**The brief's "~1 s a part" was a single-part, uncontended measurement; this run averaged ~2.9 s a
part early and drifted to ~4 s a part by the end, under sustained contention from rung 4's GPU build
staging tens of gigabytes off the same SMB share for most of the run** (`probes/2026-09-02-rung-4-share-reads/`
records the same effect on OpenAlex's projected scan: contended reads over SMB run at a fraction of
an uncontended single-file timing, not because the projection got more expensive but because another
process is on the wire too). The rate held essentially flat, 8.1–11.6 MB/s, across the whole run
rather than degrading further, which is consistent with steady contention rather than a worsening
fault. **This is a floor, not the share's capability** — the same caveat rung 4 records for its own
number.

**Memory stayed flat.** A 60-part smoke measured 6.33 GB and the full 8,309-part run measured the
same 6.33 GB — the resident cost is the loaded `(hash, row)` wanted set (≈2.7 GB of data plus the
parquet decompression and pyarrow overhead of reading it back), paid once, not per part; nothing in
the per-part loop accumulates.

**3,631,642,208 GBIF rows scanned** across the 8,369 parts — **132,551,257 more** than the
acquisition README's stated 3,499,090,951 (3.8% higher), an independent count rather than a
repetition of the card. Not investigated further, flagged rather than reconciled — GBIF republishes
occurrence.parquet monthly and the README's figure may be for a different sync than the 8,369 files
actually staged, or the publisher's count may itself be approximate; either way, this is what is
actually on this share's copy, and nobody should quote the card's number as this rung's row count.

## 4. The combine — and a second, smaller memory bug caught the same way

**The first `combine()` read all 2,989 matching shards into one table before doing anything else,
and peaked at 18.55 GB RSS** (19,450,424 KB) — well past budget, on a step that runs once for under a
minute. The scan that produced the shards never touched more than 6.33 GB in over six and a half
hours; concatenating its output at the end was where this version spent memory, not the scan.
Rewritten to stream shard by shard — one row group written per shard into `gbif-coordinates.parquet`,
exactly `stage.py`'s combine shape for `metadata.parquet` — the coordinate counts become a running
sum and the string-equality audit (below) reads one row from a hundred *separate* shard files rather
than from a full concatenation. Peak RSS on the rerun: **0.29 GB**, in 31.5 s — faster as well as far
smaller, because nothing is copied twice.

**A second bug this same discipline caught: `NaN` was not null.** `scan()` builds `lat`/`lon`/
`coordinateuncertaintyinmeters` through `np.asarray(table[col])[qi]` to do the duplicate-row gather
(§2's multiset), and that conversion silently turns an Arrow null into a float `NaN` rather than
carrying the null forward — so every shard on disk already had this baked in: a GBIF record with no
coordinates was stored as a real, finite-looking `NaN`, not a null. The first `combine()` run
reported **100% of matched rows had coordinates**, because `pc.is_null` correctly saw no nulls — there
were none, only `NaN`s. GBIF never publishes `NaN` as an actual coordinate, so a `NaN` is unambiguous
evidence of the same missingness a null bit would have carried, and `combine()` now converts it back
before writing and before counting. This did not require re-running the six-and-a-half-hour scan —
the already-written shards carry the real numbers, just under the wrong sentinel, and the fix is a
column rewrite, not a re-read of GBIF.

**String-equality spot check: 100/100.** `gbif_join.py`'s matching is by hash alone — no wanted-side
string is held resident during the scan (§2), so a candidate is checked against the true `gbifid`
GBIF's own part already carries, never against the wanted set's `source_id`, which isn't there to
check against. `combine()`'s audit instead reads the true `source_id` back from `metadata.parquet`
for a hundred rows, one drawn from each of a hundred randomly chosen shards, and confirms it against
that shard's `gbifid` — **100 sampled, 100 verified.**

**The birthday bound on hash-only matching is small and stated, not assumed away.** With no
wanted-side string resident during the scan, a genuine risk exists that some unrelated GBIF id
happens to share a `source_id`'s 64-bit hash. Modelled from the measured cardinalities — 152,130,702
distinct wanted hashes against 3,631,642,208 GBIF rows scanned — the expected number of such
cross-collisions across the *entire* scan is **≈0.03** (`n₁·n₂ / 2⁶⁴`), and the expected number of
*self*-collisions among the wanted set's own 152,130,702 distinct hashes is **≈0.0006**. Both are
modelled, not measured directly (there is no way to observe a non-event), and both are consistent
with the spot check's 100/100.

## 5. The join rate — the finding

**Three ways, as asked for:**

| denominator | matched-with-coordinates | rate |
|---|---|---|
| all 233,055,986 TreeOfLife rows | 176,899,537 | **75.90%** |
| the 222,660,750 `gbif` rows | 176,899,537 | **79.45%** |
| the 205,901,893 rows that matched a `gbifid` at all | 176,899,537 | **85.91%** |

**Nearly three-quarters of the whole corpus, and it is a real number, not a small one.** 75.90% of
every row in TreeOfLife-200M — not just its GBIF-sourced quarter — can carry a coordinate and sit in
the `geo` view. That is because `gbif` is 95.53% of the corpus in the first place (§1): a high match
rate against a dominant source translates almost directly into a high match rate against the whole.

**The two leaks, stated plainly:**

- **7.53% of `gbif` rows (16,758,857) matched no `gbifid` at all.** The id spellings agree exactly
  where they do match (the spot check), the scan covered every one of the 8,369 parts, and this
  mirrors PaperSeek's 3.13% gap against OpenAlex (rung 4) for the same reason stated there: ids
  drift out of a snapshot between one publisher's vintage and another's — TreeOfLife's `gbif` rows
  were sourced at some point before 2026-08-27, and GBIF's own occurrence records get merged, split,
  or withdrawn over time. Not investigated further; carried into the corpus as rows with no
  coordinate, which is what the `geo` view's "an entity absent from a view is fine" already covers.
- **14.09% of matched rows (29,002,356) carry a `gbifid` with no coordinates at all.** GBIF is not
  purely a geographic dataset — `decimallatitude`/`decimallongitude` are themselves optional Darwin
  Core fields, and a record can exist (with a taxonomy, a licence, a publisher) without ever having
  been georeferenced. This is GBIF's own null rate on this pair of columns for the matched subset,
  not a join defect.

## 6. Staged files

| file | bytes | rows |
|---|---|---|
| `staging/metadata.parquet` | 8,828,110,265 (8.83 GB) | 233,055,986 |
| `staging/gbif-ids.parquet` (intermediate, hash + row only) | 2,122,233,588 (2.12 GB) | 222,660,750 |
| `staging/gbif-coordinates.parquet` | 2,498,452,911 (2.50 GB) | 205,901,893 |

`gbif-coordinates.parquet` carries `row` (u32), `lat`, `lon`, `coordinateuncertaintyinmeters`
(all `float64`, properly null where GBIF has no value — §4), and `gbif_license` (`string`,
dictionary-encoded), exactly the interface's five columns.
