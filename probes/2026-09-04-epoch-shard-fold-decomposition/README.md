# What a fold costs, and how much of it a per-shard fold would remove: rung 3 (MedCPT, 36M points)

**Status:** measurement, 2026-09-04. One fold on the rung's all-in bundle, every part timed and
classified against the epoch-shard proposal: N shards in one process, each with its own row and
entity space, so that a compaction fold runs over one shard.

**Class C, the part a per-shard fold would still pay over the whole corpus, is 4.9 s of 329.5 s:
1.5%.** The rest splits almost evenly: 166.2 s (50.5%) is proportional to the folded rows by
construction (class A), and 158.4 s (48.1%) is proportional to the whole corpus today only because
the structure is not yet per shard (class B). **The largest single cost is class B: the
`mesh/descriptors` row form rebuilt by whole projection at publication, 135.4 s, 41% of the total,
plus 3.7 GB of anonymous memory that stays resident.** A per-shard fold buys what is claimed only
once the row forms, tile indexes and per-token structures are per shard as well; with the fold
alone sharded, 48% of the cost is unchanged.

Every figure below is measured unless marked otherwise. Server-reported figures are the
staircase on `/control/status` and the `x-tessera-server-us` header; the rest are wall time from
the driver or differences between the server log's own timestamps.

## The question

The proposal's claim: most of a fold's total cost is proportional to the rows of the shard being
folded, and only a small part is proportional to the whole corpus. "Total" includes what a fold
makes the next requests pay: the row form of every large level, the tile indexes, the per-token
row projections and the masked-count histograms are all keyed to the row space the fold replaced.

Three classes:

- **A, shard-local by construction.** Proportional to the folded shard's rows or entities.
- **B, corpus-wide today, shard-local under the design.** Proportional to the memberships or rows
  of the whole corpus only because the structure is one per deployment, not one per shard.
- **C, corpus-wide inherently.** Would still be paid over the whole corpus, or paid whole per fold
  regardless of the shard, with N shards.

## The run

`fold_decomposition.py`, against a copy of `data/ladder/.measure/medcpt36/allin` (a fold writes a
new prefix and reclaims the old one, so the original was not served). The copy's prefix was reset
to the build's state by removing the three side manifests later runs had added
(`SEGMENTS-1..3.json`), the state `../2026-09-03-growth-trigger/` measured from. Served alone on
8221 to 8223 under `nice -n 10`, no memory cap, page cache warm from the copy;
`serve.stream_deadline_ms` raised to 1200 s so that no request was shed before it reported.
Binary built from `main` at 24b3835b, which carries the held-row-form amendment
(`../2026-09-03-growth-trigger/after-the-fix/`). MemAvailable was 38.5 GB before the serve and
33.9 GB before the fold.

1. Open: 33.1 s. The engine adopted the prefix's containment partitions, tile index and row column
   and built the `mesh/descriptors` row form by transposing the fold-written column (16.0 s of the
   open; `adopted=false transposed=true`).
2. Three grants from the rung's term ladder (`compose_ladder`): head, all 17 terms, 35,920,666
   visible; mid, `L` and `V`, 3,469,942 visible; tail, `V`, 4,910 visible. Authorise: 7, 5 and
   4 ms. Each token was kept for the whole run: the projection cache is keyed by token, so a fresh
   token per request would have made every request a miss.
3. Zoom-0 whole-extent viewport, `layers: "all"`, `k: 1`, twice per grant. Server time, first then
   second: head 416 then 35 ms; mid 69 then 22 ms; tail 1.2 then 0.3 ms. The first pays the
   grant's fragment, projection and histogram; the second is the steady state.
4. One ingest batch of 10,000 rows from the rung's `points.parquet`, each carrying the same three
   `mesh/descriptors` keys: accepted 10,000, minted 0, 0.14 s. The held row form took the growth
   as a delta (47 ms, of which 13 ms was the clone; `rows_added=0`, the rows being still in the
   commit buffer). Delete of the first 100 through `/control/changes`: 12 ms. Flush: published
   0.25 s after the request; the three resident projections were refreshed in the background
   (`refreshes` 0 to 3). One viewport per grant after the flush, so that the post-fold requests
   carry the fold's effects and not the flush's: head 33 ms, mid 23 ms, tail 0.4 ms server time,
   35,930,566 rows visible to the head grant.
5. `/control/compact` at 00:55:34.3; `folds` went to 1 at 01:00:51.2. The publication line puts
   the flip at 01:00:50.711: 316.4 s from request to publication.
6. The same viewport, twice per grant, then once at depth 8 over one 256th of the extent in each
   axis, twice per grant.

The server's resident set was sampled once a second through the fold.

## Where the artifact pass is charged

Not in the staircase. `compact.rs`'s fold thread records six passes (entry, 1 row space,
2 postings, 3 external ids, 4a attributes, 4c entity terms, 5 digests + fsync) and touches no
membership. The artifact pass runs on the executor thread inside `publish_fold`, in two places:
step 3a, which rewrites every level's membership extents into the new prefix and writes the
fold-written structures (layouts, tile indexes, row columns, containment partitions); and
`warm_artifact_caches` after the swap, which rebuilds every level's row form and tile index. Both
are inside the publication line's `elapsed_ms=150762` and outside `/control/status`'s
`last_secs=162`, which is the staircase sum. **The status figure reports half of the fold.**

In this run step 3a wrote the fold-written structures for `clusters/kmeans` only. The 100
deleted rows were members of `mesh/descriptors` artifacts, so `levels_moved_by` put that level in
pending retirement, and a level the retirement moves gets no layout, no tile index, no row column,
no containment partition and no stated version in the side manifest (`SEGMENTS-4.json` names the
`clusters/kmeans` level alone). The warm therefore rebuilt `mesh/descriptors` by projecting
1.66×10⁹ entity-space memberships through the new permutation (`adopted=false
transposed=false`), the route `../2026-09-03-growth-trigger/` measured at 94 to 177 s. Any fold
that retires a member of a large level takes this route.

`../2026-08-16-fold-artifact-pass/` measured 32.8 s for 10⁷ artifacts of 100 members over 10⁹
rows on eight threads, linear in rows. This level is 3×10⁴ artifacts of 5.5×10⁴ members each over
3.6×10⁷ rows, 29,725 of them spanning the whole map, rebuilt on one thread: 1.66×10⁹ memberships
in 135 s, 12×10⁶ memberships per second. The per-row model does not transfer to a level whose
artifacts each cover most of the corpus; the cost is per membership.

## The table

Seconds are wall unless the source column says otherwise. Anonymous memory deltas are from the
one-second samples (publication rows) or the staircase (fold-thread rows); "retained" means still
resident after the run. Timestamps are the server log's.

| # | cost | s | anon Δ | class | basis |
|---|---|---:|---:|:-:|---|
| 1 | dispatch: request to fold thread, and thread end to publication start | 0.5 | | C | constant per fold: the executor tick and the hand-off; residual of 316.4 − 165.1 − 150.8 |
| 2 | 1 row space: renumbering, permutation, new base segment | 10.1 | +0.02 GB | A | staircase; proportional to the folded rows |
| 3 | 2 postings: coalesce into one base tier | 2.4 | +0.03 GB | A | staircase; proportional to the folded pairs |
| 4 | 3 external ids: run and locator | 0.2 | 0 | A | staircase |
| 5 | 4a attributes: 4.54 GB read, 4.68 GB written | 69.9 | +0.05 GB | A | staircase; per entity |
| 6 | 4c entity terms: the entity→terms transpose | 76.8 | 0 | A | staircase; per entity |
| 7 | 5 digests + fsync | 5.6 | 0 | A | staircase; per byte written |
| 8 | publication 3a: membership extents rewritten in entity space (30,473 artifacts, 1.66×10⁹ memberships), containment partition and layout re-evaluation (`clusters/kmeans` only), fold row spaces | 9.6 | +2.7 GB | B | 00:58:19.949 to the layout line at 00:58:29.583; per membership of the whole corpus. The containment and layout parts are per artifact population (C) and were not separated; both covered 256 artifacts here |
| 9 | publication: `clusters/kmeans` tile index, report, `SEGMENTS-4.json`, hard links and fsync of the carried files, `MANIFEST.json`, `CURRENT` flip, retire (Rule F, 100), open the new prefix, rotate the fragment cache (12 persisted fragments swept), swap | 2.4 | 0 | C | layout line to the identity-rotated line at 00:58:32.005; per fold, per file, per prefix. The tile index inside it (B) is 256 artifacts and cost 0.2 s by row 10's measure |
| 10 | warm: `clusters/kmeans` row form and tile index, 256 artifacts, by projection | 0.2 | 0 | B | 00:58:32.005 to 00:58:32.236 |
| 11 | warm: `mesh/descriptors` row form and tile index, 30,217 artifacts, by whole projection | **135.4** | **+3.7 GB, retained** | B | 00:58:32.236 to 01:00:47.643; per membership of the whole corpus |
| 12 | warm: lineage (30,473 records' parent pointers) and the warm's end | 1.9 | 0 | C | 01:00:47.643 to the warm line at 01:00:49.574 (`elapsed_ms=137568`); per artifact population |
| 13 | WAL rotation and reclaim | 0.1 | 0 | A | to 01:00:49.663; per row ingested since the last rotation |
| 14 | reclaim the superseded prefix, 11 GB | 1.0 | 0 | A | to the publication line at 01:00:50.711; the old prefix is the shard's |
| 15 | background refresh of the six resident projection entries: three fragments rebuilt under the rotated identity, three projections rebuilt over the new permutation | ≤0.4 | 0 | B | ran on the pool during row 11 (`refreshes` 3 to 9); not logged, bounded by each grant's establishment cost in step 3 (head 381, mid 46, tail 1 ms server time) |
| 16 | first request per grant after the fold: histogram re-derived over the composed mask (keyed by `segments_version`), first touch of the new prefix's mapped columns; projection and fragment were cache hits | 10.2 | 0 | B | wall over the steady request that followed: head 14.17 − 5.27 = 8.9 s (header 263 vs 144 ms), mid 2.32 − 1.05 = 1.3 s (27 vs 51 ms), tail 0.0 s (1.1 vs 7.0 ms); per visible row of the whole corpus |
| 17 | steady state after the fold above the steady state before it | 2.5 | 0 | B | head 5.27 vs 3.23 s wall (144 vs 35 ms header), mid 1.05 vs 0.59 s, tail 0.03 vs 0.01 s; not separated (see caveats); counted once |
| 18 | depth-8 viewport, first then second | 0.0 | 0 | | head 6.9 then 0.3 ms, mid 0.3 then 0.3 ms, tail 0.2 then 0.1 ms server time: nothing else rebuilds lazily |

| class | s | share |
|---|---:|---:|
| A, shard-local by construction | 166.2 | 50.5% |
| B, corpus-wide today, shard-local under the design | 158.4 | 48.1% |
| C, corpus-wide inherently | 4.9 | 1.5% |
| total | 329.5 | |

Resident memory: the staircase's maximum was 9.61 GB total, 4.59 GB anonymous, and that is what
`/control/status` reports as `last_rss_bytes`. The sampled peak was 17.08 GB total, 10.99 GB
anonymous, during row 11. After the run the process held 10.81 GB anonymous against 4.49 GB
before the fold: +6.3 GB retained, rows 8 and 11. Before the fold the level's row form was the
mapped, transposed column (file-backed, 5.07 GB `RssFile`); after it the form is Roaring bitmaps
in anonymous memory, about 2 bytes per membership.

## What it says

- The claim holds for the fold thread and fails for the fold. The staircase is class A
  throughout, 165 s, and would shrink with the shard. The publication and the request path add
  another 165 s, and 158 s of that is proportional to the whole corpus's memberships and visible
  rows. With N shards and one deployment-wide row form per level, a per-shard fold halves the
  cost at most, whatever N is.
- Class C is small, 4.9 s, and is per fold rather than per row: dispatch, manifests, links, the
  flip, opening the prefix, the lineage. With N shards it is paid N times per corpus, which at
  this size is N × 5 s.
- The largest term is the row-form rebuild of the one level whose artifacts span the map, and it
  is per membership: 1.66×10⁹ memberships, 135 s, 3.7 GB. Sharding the row form makes it per
  shard; the fold-written column, which this fold did not write because a member was deleted,
  would make it a 16 s transpose instead.
- `/control/status`'s `last_secs` and `last_rss_bytes` report the staircase and not the
  publication, so an operator reading them sees 162 s and 9.6 GB for a fold that took 316 s and
  17.1 GB.

## Caveats

- One run, one bundle, one fold, page cache warm, under another agent's load (load average about
  4.5) with the server niced. The staircase's 4a and 4c figures are the ones most exposed to
  that load.
- The `x-tessera-server-us` header is taken when the response is committed and excludes the
  streamed body, where the layer frames are produced; wall time includes the client's parse of
  the response (about 3 s for the head grant's 3.9 MB). Rows 16 and 17 are therefore wall over
  wall. Row 17 is not attributed: the projected row form, the histogram over it, and the new
  prefix's columns being faulted in (`RssFile` fell from 5.07 to 1.84 GB at the reclaim) are all
  candidates, and it may not be a fold cost at all.
- Rows 8, 9 and 12 are differences between neighbouring log lines and lump every sub-step without
  a line of its own. Row 15 is bounded, not measured.
- The deletions moved the large level, so the run measured the projection route for row 11 and
  not the transpose route. A fold with no member deletions would write the column and the warm
  would transpose it; both routes are class B, and the class C fraction is the same either way.
- Under N shards the per-token fragment and projection would be split per shard, so row 15 is
  taken as B. If a fragment stays one bitmap per token over a global entity space, row 15 moves
  to C and is still under a second here.
- Modelled, not measured: nothing in the table. The extrapolations to N shards in the section
  above are arithmetic on the measured rows.

`result.json` is the driver's record, with the one-second resident-set samples and the table under
`classification`; `serve-log-excerpt.txt` is the server log in full, the fifteen lines every
timestamp above is read from.
