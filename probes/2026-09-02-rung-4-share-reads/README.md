# Rung 4 preliminaries — the share, measured for this rung's reads

**Status:** Measurement. Answers the ingest-campaign plan's §8 item 5 for rung 4 (PaperSeek +
OpenAlex): "the 67 MB/s figure is sequential; a footer read plus per-column range reads over SMB
may be nothing like it. ⊘ Measure on 20 files before committing a plan to the projected figure."
Measured on 2026-09-02 against `/mnt/nas/tessera/datasets/paperseek-openalex/2026-08-27/` and
`/mnt/nas/tessera/datasets/openalex/2026-08-27/`, read-only, `~/venvs/projection/bin/python`
(pyarrow 23.0.1). Two other measurement tracks (a build, a capped server) shared the box; nothing
here used more than one Python process at a time.

**Headline: the plan's worry was right, and worse than the number it named.** Column-projected
reads over `works` run at **~10 MB/s aggregate, not 67** — about a seventh of the sequential
figure — because the columns worth reading are nested structs (`primary_topic`, `open_access`,
`best_oa_location`) stored as many small leaf column chunks, and SMB's per-request overhead
dominates at that grain. The join is still cheaper this way than reading whole files (which do run
close to the 67 MB/s figure): projecting is ~4.3× faster in wall time than a full-file scan, just
not proportionally cheaper in bytes moved.

## 1. PaperSeek — all 53 chunks, footers; one chunk, two read patterns

Script: `paperseek_footers.py`. Full per-chunk table (rows, row groups, per-column compressed and
uncompressed bytes) in `paperseek_footers.json`.

**Row total (measured): 102,117,343** across 53 chunks (`chunk_0`–`chunk_54`, missing `chunk_3` and
`chunk_45` by naming — 53 files, matching the dataset card's "~102 million"). Rows per chunk range
1,380,630–2,145,974 (most chunks are exactly 2,000,000; the short ones are chunks 4, 10, 11, 17, 20,
24, 27, 34, 37, 38, 41, 48, 54). Footer reads are cheap: 0.005–0.06 s each except one 0.96 s outlier
(chunk_39, unexplained — a single SMB stall, not reproduced by any neighbouring chunk).

**Column bytes, summed over all 53 chunks:**

| column | compressed | uncompressed |
|---|---|---|
| `id` | 0.47 GB | 3.67 GB |
| `title` | 3.80 GB | 9.24 GB |
| `abstract` | 40.86 GB | 118.87 GB |
| `embedding` (1024×f32) | 190.24 GB | 418.80 GB |
| **total** | **235.6 GB** | **550.6 GB** |

`id, title, abstract` together are 45.1 GB compressed — 19.1% of the 235.6 GB whole-file total. The
README's 236.7 GB (53 chunks + 1.1 GB `index/`) matches this to within rounding.

**One-chunk timings** (`chunk_0.parquet`, 4.70 GB, 2,000,000 rows):

| read | bytes | wall | MB/s |
|---|---|---|---|
| whole chunk, all columns | 4,699,752,481 | 115.25 s | 40.8 |
| `id,title,abstract` projected | 964,895,325 (footer-declared) | 9.75 s | 98.9 |

The projected read is **faster in MB/s than the sequential baseline**, not slower — the opposite of
`works`' pattern. `id`, `title` and `abstract` are three plain, non-nested string columns; each is
one contiguous range read per row group (7 row groups here), so SMB serves it as a few large
sequential fetches rather than the deep-struct fan-out `works` triggers below. The whole-chunk read
is *slower* than 67 MB/s because it also decodes 4 GB of float embeddings across the same wall
clock, and decode, not the wire, dominates there.

**`index/`**: a RocksDB (`speedict`) database — `CURRENT`, `IDENTITY`, `LOG`, `MANIFEST-000005`,
`OPTIONS-000007`, `speedict-config.json`, and `.sst` files, 912 MB. This is the publisher's search
index; nothing in this rung reads it.

## 2. OpenAlex `works` — 20 parts, plus all-footer pass

Script: `openalex_works.py`. Per-part table in `openalex_20parts.json`; the full 2,428-part footer
pass in `openalex_all_footers.json`.

**Note on the licence field.** The dataset README names `open_access` and `license` as top-level
useful columns. There is no top-level `license` or `ids.license` column in the actual schema — the
licence lives nested at `best_oa_location.license` (and identically at `primary_location.license`
and per-entry in `locations[].license`), confirmed by sampling non-null values (`cc-by`,
`cc-by-nc`, `cc-by-nc-sa`, `other-oa`). The projection below reads the whole `best_oa_location`
struct to get it — there is no cheaper column-level route to just the licence string in this
layout.

**Projected columns**: `id, publication_year, type, primary_topic, open_access, best_oa_location`.

**20 parts, chosen evenly across the 2,428-part range** (index 0, ~128, ~256, …):

| | rows | file size |
|---|---|---|
| min | 1 | 0.07 MB |
| median | 146,199 | 181 MB |
| max | 400,000 | 1,021 MB |

Row counts vary by three orders of magnitude across the sample — one part has exactly 1 row, the
brief's "one part has 2 rows" claim is close but not exact on this vintage. Bytes track rows
closely (recent, larger `updated_date=` partitions carry the bulk of the corpus).

**Aggregate over the 20 parts** (sum of bytes / sum of wall time, not a mean of ratios):

| read | bytes | wall | MB/s |
|---|---|---|---|
| footer only | — | 0.97 s total (20 parts) | — |
| projected (6 fields) | 229.7 MB | 23.0 s | **9.98** |
| whole part | 4,837 MB | 97.9 s | 49.4 |

**Projected MB/s vs the 67 MB/s sequential figure: 9.98 vs 67 — 15% of it, not close.** Per-part
projected throughput ranged 0.8–15.2 MB/s with no part exceeding a fifth of 67. Whole-part reads
sit closer to the baseline (41–54 MB/s) because they're satisfied as large sequential fetches; the
projected read instead touches the leaf columns of three struct-typed fields
(`primary_topic`, `open_access`, `best_oa_location`), each with several nested string/float/bool
sub-columns stored as separate column chunks, so one projected row group costs a double-digit
count of small SMB range requests instead of one or two big ones. Projected bytes are only 4.7% of
whole-file bytes (229.7 MB of 4,837 MB summed), but wall time is 24% of the whole-file wall time —
the projection buys a bytes reduction the network round-trips don't let it cash in fully.

**Projected to all 2,428 parts** (row-weighted from the 20-part sample, against the 504,861,414-row
footer total below — **modelled, not measured**):

- Projected-column full scan: **≈3,265 s ≈ 54 minutes**, moving ≈32.6 GB.
- Whole-file full scan: **≈13,890 s ≈ 3.9 hours**, moving ≈686 GB.

Projecting is **4.3× faster in wall time** than reading whole files, despite the poor MB/s — the
byte reduction still wins, it just isn't free.

**All-footer pass, all 2,428 parts**: **92.6 s**, i.e. ~38 ms/footer — cheap, as expected; this ran
after the 20-part timing and is not counted against the projected-scan estimate above.

- **Row total (measured, footers, this mirrored copy): 504,861,414** across **2,428** files.
- `parquet/manifest.json` (dated 2026-06-26, presumably from the upstream sync manifest) claims
  **510,372,821 rows across 2,446 files** for the `works` entity. **Both numbers disagree with the
  actual copy on disk**: 18 fewer files (2,428 vs 2,446) and 5.5M fewer rows (1.1%). Not
  investigated further — flagged so nobody quotes the manifest figure as this copy's row count.
  The dataset README's "~322M under the API's default filter" is a different, filtered population
  and isn't comparable to either number here.

## 3. The join's shape

**Id spelling matches exactly on both sides**: full URL form, e.g.
`https://openalex.org/W1526114719` (PaperSeek `id`) and `https://openalex.org/W2396084620`
(OpenAlex `works.id`) — not the bare `W123…` form on either side. A join is a plain string-equality
join, no reformatting needed.

**No pruning from partition or file order.** `works` is Hive-partitioned by `updated_date` (the
date a record was last touched by OpenAlex, not created), and a sample of 6 parts spread across the
partition range shows `id` values spanning nearly the full `W1e8`–`W4.4e9` numeric range in *every*
partition checked — `updated_date=2016-06-24` alone runs `W1000329410`–`W999483054`. Ids are not
sorted within or across parts by any usable key. **The join is a full scan of `works`** — there is
no row-group statistic or partition boundary that lets any of the 102M PaperSeek ids skip a part.

**Matched-extract size, 102M rows (modelled)**: using the measured 20-part sample, the six
projected columns average **64.5 bytes/row** in SMB-compressed source form (229.7 MB / 3,558,232
rows across the 20 parts). Scaled to 102,117,343 matched rows: **≈6.6 GB**. This is a rough model —
it assumes the matched subset's per-row byte cost equals the whole population's, and a small local
check (`join_shape_sample.py`, in this directory) suggests real matched-row zstd extracts can run
*higher* per row (122 B/row on a 1,867-row matched sample vs 47 B/row on the same part's 358,712
unfiltered rows written the same way) — smaller, filtered slices compress worse. That sample is too
small (1,867 rows, one part against one PaperSeek chunk) to use as the scaling figure itself; it's
a flag that **6.6 GB may be an underestimate**, not a replacement number. Budget nearer 10 GB.

## 4. Disk on `/`

```
$ df -h /
Filesystem      Size  Used Avail Use% Mounted on
/dev/sdd       1007G  694G  263G  73% /
```

**263 GB free** on `/` at measurement time (2026-09-02, ~21:35), with two other tracks (a build and
a capped server) writing to the same filesystem concurrently — this number will have moved by the
time rung 4 starts.

```
$ du -sh /home/user/code/tessera/data/ladder/*
3.1G   arxiv
10G    geonames
84G    medcpt
829M   medcpt-1m
81M    medcpt-1m-abs
87M    medcpt-mesh-smoke
6.1M   multiview
25G    overture
1.9G   probe-suggest
```

`data/ladder` totals **125 GB** already, ahead of rung 4.

**What rung 4 needs, against 263 GB free**: the coordinator's estimate is bundle ~65 GB,
`points.parquet` ~25 GB, build transient ~100 GB, extract ~10 GB (raised to ~10 GB here per §3
above). If the extract and build transient must coexist with the bundle being written, that's up to
~200 GB at the peak, against 263 GB free today — tight but plausible, and it will get tighter as
`data/ladder` grows and if the other two tracks' builds are still on disk when rung 4 starts. **The
209 GB fp16 vector set cannot be staged whole** — it isn't in this estimate at all; the build must
read it in slices the way `medcpt`'s `stage.py` does, not copy it to `/` first. This is a statement
of the numbers, not a plan — the operator decides the order and whether anything needs clearing
first.

## Scripts

- `paperseek_footers.py` — §1: all-chunk footers, one-chunk whole vs projected timing.
- `openalex_works.py` — §2: 20-part footer/projected/whole timings, all-part footer pass, manifest
  cross-check.
- `join_shape_sample.py` — §3: id spelling/sampling and the small matched-extract size check.

Raw output: `paperseek_footers.json`, `paperseek_timing.json`, `openalex_20parts.json`,
`openalex_all_footers.json`, `openalex_manifest_summary.json`, `run.log`, `extract_sample.parquet`.
