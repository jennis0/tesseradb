# Publication that reads the member table once: the 1M cycles and the two grouping passes

**Status:** Evidence, never normative. WSL2, 12 cores, 47 GB, local NVMe; binary from
`f543b033`'s tree, release profile; the box otherwise idle (`pgrep -af "tessera (build|serve)"`
empty) for every run below. The driver is `test_corpora/common/ingest_cycle.py`; what it does
and why is in its `Publication` docstring.

## Result

| run | what | wall | driver peak RSS | transient disk | outcome |
|---|---|---|---|---|---|
| `medcpt-1m`, *f* = 0.10, C = 8, full cycle | 2 layers, 29,272 artifacts, 47,178,538 members published | 93 s whole cycle; publication 26.6 s service, 13.9 s driver | **1.20 GiB** | 3 buckets | **0091 census exact** on every surface; write cycle through |
| `treeoflife-1m`, *f* = 0.10, C = 8, full cycle | 1 of 3 layers publishable: 17 artifacts, 1,000,000 members | 919 s (900 s is the flush's visibility timeout, below); publication 0.7 s | **0.43 GiB** | none (streamed) | census differs by the two things recorded: `taxonomy/tree` not published, and the wire-encoding defect |
| grouping pass, `paperseek` `topics/openalex` | 394,325,928 member rows, 4,798 artifacts, 1,448 row groups | **68 s** (count 5.6 s, partition 22.0 s) | **0.99 GiB** | 2.92 GiB | 4,787 artifacts and 230,715,873 members assembled into 113 bodies (3.22 GiB); 11 artifacts declined over the cap, 163,610,055 members |
| grouping pass, `medcpt` `mesh/descriptors` | 1,658,437,807 member rows, 30,217 artifacts, 1,677 row groups | **333 s** (count 31.1 s, partition 115.6 s) | **1.19 GiB** | 14.42 GiB | 30,172 artifacts and 1,113,517,598 members assembled into 558 bodies (15.56 GiB); 45 artifacts declined over the cap, 544,920,209 members |

Measured. Peak RSS is the process's `VmHWM`; transient disk is the largest total size of the
bucket files under `--work`, sampled once a second. Raw results are in [`runs/`](runs/).

The driver that inverted a layer's member table in memory declined `mesh/descriptors` at
1.66×10⁹ rows and would have held ~25 GB of bodies for it. The reader here holds one bucket and
one artifact: 1.19 GiB over the same table, and the figure does not move with the table's size
(0.99 GiB over paperseek's 394M rows, 1.20 GiB for the whole medcpt-1m cycle including its
split and hold-out).

## Growth on a DAG, medcpt-1m (decision 0127, driver side)

**Measured 2026-09-05** (`runs/medcpt-1m-f010-grow.json`, and `runs/medcpt-1m-f010-grow-after-fixes.json`
after the referee's fixes; binary from the `wire` track's tree): medcpt-1m at *f* = 0.10 with
`--publish-max-bytes 262144`, so that many artifacts exceed the cap, and `--state-extent`. Every
artifact published: `mesh/descriptors` **29,229 of 29,229**, of which **416 were published with a
first slice and grown by 1,550 `PATCH` requests carrying 22,885,982 members** (`joined` from the
route sums to the same; `grown_members_unjoined` 0), **40,075 of 40,075 parent edges**, 0 refusals,
0 declined; `clusters/kmeans` 43 of 43, 30 grown. **Census exact on every surface and principal**
(`mesh/descriptors` 16,063 artifacts and 45,958,206 masked members at 100% on both sides). The
first growth of a hierarchy's roots on the wire; the 36M cascade below is what it replaces.

## The 36M cycle, and the cascade a declined root causes

**Measured 2026-09-05** (`runs/medcpt-36m-f010.json`, binary `e442e139`, box otherwise idle):
MedCPT at *f* = 0.10, C = 8, the full cycle. Base 32,328,599 rows; 3,592,067 rows ingested at
**65,884 rows/s**; flush to visibility **2.1 s**; fold **166 s at 8.1 GB** peak; **driver peak
1.79 GiB** over the whole cycle (the reader this probe measures, at the base size that used to
stall it). `clusters/kmeans` published in full: 256 artifacts, 35,920,666 members in 70.6 s,
**508,457 members/s**, 17 requests, census exact.

`mesh/descriptors` did not: **183 of 30,217 artifacts published**, 190,602,551 members, 558
requests over 2,305 s, of which **465 answered 422** — "publishes an artifact whose parent is
*X*, which the layer does not hold". The 45 roots over the cap were declined as designed, and
every descendant of a declined root then named a parent the level did not hold. The DAG's 30,217
descriptors hang almost entirely under those 45, so the decline cascaded to 29,989 artifacts, and
the census for the layer lists the whole hierarchy as missing (folded 182 artifacts against the
all-in build's 28,789 under the 100% principal) rather than the 45 declined. The driver recorded
the statuses in the layer's block and said nothing in its log line. Two driver defects, both
assigned to the `wire` track: a child's edge to a declined parent is dropped and counted rather
than sent, and a refused publication is logged as one. Decision 0127's growth request removes the
decline itself; until it lands this cascade is what a declined root costs.

Publish throughput over the requests that landed: 82,674 members/s on MeSH against 508,457 on
kmeans, the difference being 465 refused requests each carrying a full body, and the roster's 9,095
multi-parent artifacts in 72 buckets against kmeans' 256 in a stream.

## The two 1M cycles

`runs/medcpt-1m-f010.json`. Base 900,000 rows built in 7.0 s; 100,000 rows ingested at
125,288 rows/s, ten batches, every one a 200. Publication, after the hold-out:

| layer | read path | artifacts | members | edges | requests | driver (`prepared_s`) | service (`wall_s`) | members/s |
|---|---|---|---|---|---|---|---|---|
| `clusters/kmeans` | streamed (1 row group) | 43 of 43 | 1,000,000 | 0 | 1 | 0.12 s | 0.67 s | 1,493,344 |
| `mesh/descriptors` | partitioned (47 row groups, 3 buckets; count 0.9 s, partition 2.7 s) | 29,229 of 29,229 | 46,178,538 | 40,075 of 40,075 | 22 | 13.8 s | 25.9 s | 1,780,749 |

Flush to visibility 0.009 s, fold 8 s at 2.40 GB, and the 0091 census `equal: true`: zoom 0,
every box and both layers identical to the all-in build under all six principals (at the 100%
principal 39 artifacts and 999,906 masked on `clusters/kmeans`, 16,063 and 45,958,206 on
`mesh/descriptors`, on both deployments). The write cycle ran: 1,000 deletes, 1,000
suppressions, 1,000 re-ingests, a second fold, 997,281 visible after it. Before this change the
same rung's `mesh/descriptors` cell published through an in-memory inversion; the bodies this
reader produces are byte-identical to that one's (checked on both files of this rung and on
treeoflife-1m's, every `(artifact, rank)` set equal, before any run).

`runs/treeoflife-1m-f010.json`. Base 900,000 rows; 100,000 rows ingested into `bioclip` at
112,802 rows/s. The declaration has three layers and `publish.declined` names the two that did not
publish:

| layer | outcome |
|---|---|
| `clusters/kmeans` | published: 17 of 17 artifacts, 1,000,000 members, one request, 0.7 s; streamed over 51 row groups |
| `taxonomy/tree` | not published: an open value set with no roster, 1,000,000 list-keyed member rows. Recorded under the pending ruling on whether the route grows a layer's artifacts in pieces |
| `publishers/source` | nothing to publish: membership is the `publisher` attribute column, which the ingest batch carries |

The census: 3 zoom-0 differences, 22 box differences, 10 layer differences. Every one is
attributable to one of two things, neither of them the publication.

*`taxonomy/tree` is declared and empty on the folded deployment.* Six of the ten layer rows
(`folded: null` against the all-in build's 2 to 6 artifacts by principal).

*The wire encoding splits publisher names on commas* (handover memo §4, the passthrough plugin's
descriptor list). 1,779 of the 100,000 hold-out rows carry a comma in `publisher`; 60 of them are
`Natural History Museum, Vienna`, whose first fragment is itself a declared key, so they land in
`Natural History Museum`; the other 1,719 are minted as fragment terms no principal holds. That
predicts the folded deployment at 998,281 under the 100% principal and at −1,719 under the 50%
principal, and +60 under the 25% principal, which is what the census reads (`998,281`, `415,164`
against `416,883`, `249,630` against `249,570`). The 22 box differences and the `publishers/source`
rows are the same rows counted by box and by publisher. The `clusters/kmeans` row at the 100%
principal (15 artifacts and 884,700 masked against 17 and 1,000,000) is the same defect through the
layer's content rule: its supplied `topic` content is generated from 200 sampled members at rank 0
and 66 at rank 1 under `require_member_visibility = "all"`, and exactly two clusters, `km-000009`
and `km-000014`, have an invisible row in both ranks' samples, so neither content is disclosable
and the artifact is withheld (51,523 + 62,058 visible members = the 113,581 gap); three other
clusters have a hit at rank 0 only and fall back to rank 1. The flush's 900 s is the visibility
poll waiting for 1,000,000 rows that the encoding put out of reach; the executor's own flush was
0.5 s.

## The two grouping passes

`grouping_pass.py` runs `Publication.bodies()` over one layer with the sender replaced by a
counter, the process's `VmRSS`/`VmHWM` and the bucket files' size sampled once a second
(`runs/grouping-*.rss.csv`). Both files are partitioned: every row group of each spans most of
the key range, so nothing can be streamed. Bucket budget 16×10⁶ rows (the driver's default),
body cap 32 MiB between artifacts, the route's 64 MiB cap per artifact.

| | `paperseek` `topics/openalex` | `medcpt` `mesh/descriptors` |
|---|---|---|
| member rows, row groups | 394,325,928 in 1,448 | 1,658,437,807 in 1,677 |
| roster | 4,798 artifacts, 4 levels, 4,794 edges | 30,217 artifacts, one level, 41,321 edges (`dag`) |
| counting pass | 5.6 s | 31.1 s |
| declined at planning (membership alone over 64 MiB) | 11 artifacts, 163,610,055 members; largest `3` at 40,846,248 members, 584 MiB; smallest 4,556,155 | 45 artifacts, 544,920,209 members; largest `eukaryota` at 27,171,642 members, 389 MiB; smallest 4,715,309 |
| partitioning pass (rows of the artifacts not declined) | 22.0 s, 15 buckets | 115.6 s, 72 buckets |
| peak transient disk | 2.92 GiB | 14.42 GiB |
| bucket reads and assembly | the rest of 68.3 s | the rest of 333 s |
| bodies assembled | 113, 3.22 GiB; 4,787 artifacts, 230,715,873 members, 4,787 edges | 558, 15.56 GiB; 30,172 artifacts, 1,113,517,598 members, 41,292 edges |
| peak RSS | 0.99 GiB | 1.19 GiB |

The declined counts are the controller's 2026-09-05 measurement reproduced (11 of 4,798 and 45 of
30,217, the same largest keys). Every declined artifact here was declined at planning, on its
membership list alone (`content_counted: false`), so `body_bytes` for them omits the content; the
assembly-time check, which counts content, caught none in addition. The edges not assembled
(7 and 29) are the declined artifacts' own `parent` lists.

Not measured here: the service's side. The bodies were counted, not sent, so `members/s` on the
route at these sizes is still the campaign's figure to take, on a quiet box, when the controller
schedules the 36M and 92M cells.

## What the reader is bounded by

The streamed path holds the row groups spanning one key boundary; the partitioned path holds one
bucket (at most `--publish-bucket-rows` rows, 14 bytes each on disk, sorted in memory) or one
artifact where that artifact alone is larger, plus the body being assembled. Neither holds
anything proportional to the table. The external-id table the old driver built (16 bytes per
entity, 3.7 GB at rung 5) is gone: an id is base64-encoded from the entity id in NumPy as the
body is assembled.

## Method

```bash
export TESSERA_LADDER=/home/user/code/tessera/data/ladder
python3 -m test_corpora.common.ingest_cycle --rung-dir data/ladder/medcpt-1m --work <scratch> \
    --binary target/release/tessera --out runs/medcpt-1m-f010.json \
    --fraction 0.10 --concurrency 8 --port0 8191 --write-cycle --state-extent
# the same for treeoflife-1m
python3 probes/2026-09-05-publish-streaming/grouping_pass.py --rung data/ladder/paperseek \
    --layer topics/openalex --work <scratch> --out runs/grouping-paperseek-topics.json \
    --rss-csv runs/grouping-paperseek-topics.rss.csv
python3 probes/2026-09-05-publish-streaming/grouping_pass.py --rung data/ladder/medcpt \
    --layer mesh/descriptors --work <scratch> --out runs/grouping-medcpt-mesh.json \
    --rss-csv runs/grouping-medcpt-mesh.rss.csv
```

A cycle takes six consecutive ports from `--port0`: three for the folded deployment and three
for the all-in one the census compares it with.
