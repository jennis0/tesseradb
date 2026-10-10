# The measurement schema, and the three drivers that fill it

Every rung of the ladder can be measured for the same three things: build cost per stage, view
cost across a principal ladder, and what online ingest sustains through flush, merge and
compaction.

| | |
|---|---|
| [`deployment.py`](deployment.py) | boots a `mosaica serve` over an existing bundle, on its own ports and scratch state, inside a transient cgroup scope |
| [`serve_battery.py`](serve_battery.py) | the view-latency battery — a principal ladder, density-decile locations, three conditions |
| [`ingest_cycle/`](ingest_cycle/) | split, build the complement's points and declarations, ingest the hold-out, publish every layer's artifacts, flush, fold, and an equivalence census; with `--churn-rounds`, [the churn](#the-churn) instead |
| [`workload.py`](workload.py) | one rung through all of it — build, verify, battery, cycle — and a report of what held and what it cost |
| `scripts/campaign_report.py assemble` | collates a rung's driver outputs into a committed `measurements.json`, on the schema below |

Two routes write a result, and they are not the same. `workload.py`'s own result file is
box-specific and is never committed. `campaign_report.py assemble` is the older route: four rungs
(`gbif`, `medcpt`, `paperseek`, `treeoflife`) have a committed `test_corpora/<rung>/measurements.json`
from it, and [`../../scripts/campaign_report.py`](../../scripts/campaign_report.py) renders the
campaign table in [`../../docs/ingest-campaign.md`](../../docs/ingest-campaign.md) from those
files. The table is generated and the marked block is not hand-edited.

## Running one rung

```bash
export MOSAICA_LADDER="$PWD/data/ladder"
python3 -m test_corpora.common.workload --rung arxiv --work /tmp/wl [--quick]
```

This builds `mosaica` from the checkout unless `--binary` names one, runs `mosaica check`, builds
the all-in bundle into `<work>/<rung>/bundle`, runs `mosaica verify --deep` over it, then drives
the battery and the cycle against it under a 16 GiB cap. It prints a correctness line per check,
any failure making the exit code non-zero, and a cost table beside the most recent earlier run of
the same rung and shape. `--quick` is three zooms, three deciles, ten samples a cell and a 2%
hold-out — about seven minutes on arXiv; full mode is every driver's own default and a 10%
hold-out.

The principal ladder is built from the rung's `<axis>-ranks.json`. A rung without one has it
derived into `<work>/<rung>/derived-ranks.json`: one pair per (entity, label) of each view's
`point_visibility` field across every view's points, a missing label counted under the view's
default. The result's `ranks` says which it was.

Each run writes `$MOSAICA_LADDER/<rung>/workload-results/<timestamp>-<commit>.json`, holding both
driver results whole. These are not committed: `data/` is git-ignored and they are one box's
figures.

Ports run from `--port0`, default 8171: the battery takes three, the cycle six and the churn
three. 8111–8143 are not available; other sessions on this checkout use them.

The drivers also run on their own, for a sweep of one knob:

```bash
# the battery against a server it boots itself
python3 -m test_corpora.common.serve_battery --boot-rung <rung> --boot-bundle <bundle> \
    --boot-scratch <scratch> --boot-binary <mosaica> --boot-port0 8151 \
    --cap-bytes 17179869184 --out serve-6g.json

# the cycle, one cell per (fraction, concurrency)
python3 -m test_corpora.common.ingest_cycle --rung-dir <rung> --work <scratch> \
    --binary <mosaica> --fraction 0.10 --concurrency 8 --write-cycle --state-extent \
    --all-in-bundle <bundle> --cap-bytes 17179869184 --out ingest-10.json

# collate and render
python3 scripts/campaign_report.py assemble --rung medcpt --rows 35920666 \
    --build stage-timings.json --bundle <bundle> \
    --serve serve-nocap.json --serve serve-6g.json --ingest ingest-10.json \
    --out test_corpora/medcpt/measurements.json
python3 scripts/campaign_report.py
```

`--all-in-bundle` is the bundle built from every row of the rung: the census reference an ingest
cycle compares against, and the frame `--state-extent` states into the base declaration.
`--cap-bytes` is `MemoryMax` on every served deployment's transient scope; absent is no cap. Every
deployment the cycle serves is a copy of a bundle on its own scratch state, since a bundle served
directly is a different bundle once a publication or flush writes to it.

## The churn

The ingest cycle's second mode measures what steady deletion and re-insertion does to a built
corpus: the first stage of [`../../docs/sharding.md`](../../docs/sharding.md) reuses a deleted
item's number, and its §8.4 names the figures that stage must show. The churn serves a copy of the
all-in bundle and, each round, deletes `--churn-fraction` of the items through `/control/changes`,
ingests the same rows again as new items through `/control/ingest`, flushes and compacts. Nothing
else of the cycle runs. There is no split, base build, publication or census, so a built rung is
all it needs.

```bash
python3 -m test_corpora.common.ingest_cycle --rung-dir <rung> --work <scratch> \
    --binary <mosaica> --all-in-bundle <bundle> --churn-rounds 10 \
    --cap-bytes 17179869184 --out churn-geonames.json
```

Every item gets a position in [0, 1) from `--seed`. Round `r` takes the items whose position falls
in a window `--churn-fraction` wide, starting `(r - 1) × fraction` along and wrapping at 1. At the
default of one half the rounds alternate halves, so every item is deleted and inserted again every
second round. The deletes are paged by `limits.changes.max_changes_per_request`. An item's rows go
back into every view it was in, the anchor's first, and the anchor's batches carry each
column-route layer's member list. Not built yet: a publication-route layer's memberships of a
deleted item are not published again, so after round 1 such a layer serves fewer members than the
fresh build. `churn.layers_not_republished` names those layers.

The run keeps one copy of the bundle, under `<work>/churn/bundle`, which each compaction rewrites
as a new version. Nothing is kept per round. Each row's `bundle_bytes` says what that directory
holds.

Round 0 is the copy as built, before any change: the fresh build of the same items, which every
later round is read against. Each round's row is appended to `--out` with the suffix `.jsonl` as
the round ends. `--out` holds the whole result once the run ends, the rows under `churn_rounds`
and the run's parameters under `churn`, and a table of the rows is printed.

| field | unit | how it was measured |
|---|---|---|
| `round`, `deleted` | count | the round, and the items it deleted and ingested again |
| `delete` | — | `status` of the last page sent, `wall_s` of every page, and a refusal's `body`; a refused page ends the churn |
| `live_after_delete` | count | the anchor view's zoom-0 `visible` under every term once the deletes were answered, since a deletion is in force from its answer |
| `ingest_by_view` | — | each view's pass, on the shape §3 gives `ingest` |
| `flush` | — | `POST /control/flush` until its publication lands, then the wait for `live` to return to round 0's |
| `sessions_ended` | — | `POST /control/sessions/end`'s status for each principal, sent before the compaction |
| `fold` | — | the cycle's fold: `fold_s` is the server's `compaction.last_secs`, `observed_s` the driver's wait, and `fold_refusals` with `last_refusal` a fold refused for want of memory or disc |
| `live`, `live_by_view` | count | zoom-0 `visible` under every term after the compaction, on the anchor view and, where there are several, on each |
| `entity_id_high_water`, `high_water_rise` | count | `/control/status`, and the rise since the row before |
| `free_ids`, `held_ids` | count | the newest side-manifest's `free_entities`, the freed ids the allocator issues before its high water, and `held_entities`, those held back until the log rotates; null where the side-manifest has no such field |
| `prefix` | — | the bundle version `CURRENT` names |
| `postings_bytes`, `fresh_postings_bytes`, `postings_vs_fresh` | bytes | the term index on disc, `partitions/<p>/terms/postings.arrow` and every delta tier the newest side-manifest lists, on the served copy and on the all-in bundle, and their ratio |
| `term_images_bytes`, `fresh_term_images_bytes` | bytes | the side-manifest's `term_image_extents`, on each |
| `bundle_bytes` | bytes | every file under the served copy, a version not yet removed included |
| `principals[]` | — | the narrowest and the broadest principal of `--targets`' ladder |
| `principals[].authorise_ms`, `.fragment_rebuilds` | ms, count | one `session/authorise`, wall, and `fragment_cache.rebuilds` on `/control/status` across it |
| `principals[].union_ms` | ms | `authorise_ms` where `fragment_rebuilds` is 1, which is an authorise that built the union of the principal's postings; null where the fragment cache already held it |
| `principals[].visible`, `.first_viewport_ms`, `.first_viewport_shed`, `.first_viewport_failed` | — | a fresh session's whole-extent viewport at the battery's budget depth |
| `principals[].wall_ms`, `.server_ms`, `.stream_ms`, `.served`, `.response_bytes` | — | percentiles over `--churn-samples` requests at each latency box, pooled over the boxes, as §2's `conditions.*` gives them for one cell |
| `principals[].shed`, `.failed` | count | of those requests, the streams the server cut and the requests that did not answer |

The latency boxes are chosen once, on round 0. At each `--churn-zooms` zoom, `--churn-candidates`
boxes are ranked by `visible` under the broadest principal, as the battery ranks them, and the
first box of each density decile is kept: the box the battery's first cell of that decile
measures. Every request is the battery's, at its budget depth with `k` the deployment's
`selection.max_k` and every layer.

The union cost is the authorise's wall, the session plane's work and the round trip included. An
authorise whose fragment the cache holds computes no union, and the refresh that follows each
publication rebuilds the fragment of every session still resident. So round 0 authorises each
principal before anything else asks for its fragment, and every later round ends both principals'
sessions before it compacts. `union_ms` is null wherever that did not leave the authorise to build
the fragment.

`failures` holds a sentence for each round whose deletes were refused or whose count after them
was not round 0's less the anchor rows deleted, whose ingest was refused or short, whose flush did
not land, whose count at its end was not round 0's, or whose compaction did not complete. A rising
high water is not a failure; the table shows it.

## The workload's own result file

Holds both driver results whole, under `serve` and `ingest`, beside:

| field | what it is |
|---|---|
| `rung`, `quick` | the run's arguments |
| `started_at`, `host` | when and where |
| `commit`, `dirty` | the checkout's git commit, and whether it had uncommitted changes |
| `binary` | the path to the `mosaica` binary measured |
| `minted_credentials` | credential environment variables minted for this run, sorted |
| `ranks` | the ranks file the ladder was built from; `derived` true, with the `fields` and the number of `terms`, where the rung had none |
| `steps` | wall time per top-level step: `binary`, `check`, `build`, `verify`, `serve`, `ingest` |
| `check`, `build`, `verify` | each step's `returncode`, `stdout_tail`, `stderr_tail`; `build` also carries §1's fields |
| `failures` | plain sentences: a refused check, a failed verify, an OOM kill, a dead server, a failed request, an unequal census or a rejected batch |

The exit code is 1 if `failures` is non-empty; a cost figure never affects it.

## `measurements.json`

`schema_version` is `3`. A renderer refuses a file whose version it does not know rather than
reading its fields under a schema they were not written to.

```jsonc
{
  "schema_version": 3,
  "rung": "medcpt",              // the directory under test_corpora/
  "rows": 35920666,              // the corpus's row count, as the rung's own README states it
  "binary_commit": "…",          // the mosaica binary's git commit; a cell may override it
  "built_at": "…",               // when the figures were taken
  "host": "…",                   // free text: cores, RAM, disk
  "build":  { … },               // §1
  "serve":  [ { … } ],           // §2 — a list, one entry per cap
  "ingest": [ { … } ],           // §3 — one entry per (fraction, concurrency) cell
  "notes":  [ "…" ]              // anything a number cannot carry
}
```

`serve` is a list because a rung is measured uncapped and under a cap, and both are results.

### §1 `build`

| field | unit | how it was measured |
|---|---|---|
| `stages[]` | — | `mosaica build --stage-timings-json`, one object per pipeline stage in report order |
| `stages[].stage` | — | the stage's name, from `mosaica_build::observer::BuildStage` |
| `stages[].wall_s` | seconds | the stage timer's own elapsed |
| `stages[].rows` | count | whatever the stage counted — items, pairs or terms, stage-specific |
| `stages[].peak_rss_kib` | KiB | the process's `VmHWM` when the stage ended, not the stage's own; it only rises |
| `stages[].started_at`, `ended_at` | Unix seconds | wall clock. A stage that reports a running total rather than a boundary places an interval of the measured length at the report, not at a real start and end |
| `wall_s` | seconds | the sum of the stages, not a separately-timed total |
| `peak_rss_kib` | KiB | the largest `peak_rss_kib` any stage reported |
| `bundle_bytes` | bytes | every file under the bundle root, summed |
| `rss` | — | present only when a run was sampled by a separate RSS sampler that splits anonymous from file-backed memory; `VmHWM` cannot make that split |

### §2 `serve[]` — one run of the battery

Every request draws a whole view, at a depth held under `max_tiles_per_request` and a `k` clamped
per tile to `k_max_marks`. A run with no `request` block did not record its shape, and the
renderer marks that run's columns rather than read them under an unknown shape. A stream the
server cuts mid-body is a sample, not a failed run: it can carry exact counts (first on the wire)
but no latency (its wall time is the shed deadline, not the request). Every block below counts
these under `shed` and percentiles the rest.

| field | unit | how it was measured |
|---|---|---|
| `cap` | bytes/`null` | the scope's `MemoryMax`; `null` is uncapped, still inside a reclaimable scope |
| `cgroup` | path | the transient scope's cgroup directory |
| `total_rows` | count | the 100% principal's whole-extent `visible`, the denominator of every coverage figure |
| `request.budget_depth` | depths | how much deeper than its own zoom a request goes |
| `request.grid_depth` | depth | the Morton grid's sixteen levels |
| `request.k` | count | `k` on every measured request |
| `request.rank_k` | count | `k` on a density-ranking request; 0 is counts-only |
| `request.max_k`, `.k_max_marks`, `.theta_target_marks`, `.max_tiles_per_request` | — | the deployment's selection constants; served count per tile is `min(k, max_k, k_max_marks)` |
| `request.whole_extent_*` | — | the anchor request: whole extent at the budget depth under the 100% principal; `_shed` if cut |
| `candidates_per_zoom`, `samples_per_cell`, `zooms`, `deciles`, `cells_per_decile`, `conditions` | — | the run's own parameters |
| `oom_kill_seen` | bool | `memory.events`' `oom_kill` moved during the run |
| `ladder[].target` | fraction | what the term composition aimed at |
| `ladder[].terms`, `terms_n` | — | the composed term set |
| `ladder[].measured` | fraction | whole-extent `visible` ÷ `total_rows` — measured, not the target, since pairs overlap |
| `ladder[].authorise_s` | seconds | one `session/authorise`, wall |
| `ladder[].first_viewport_s` | seconds | a fresh session's first request, carrying the fragment build and budget sweep |
| `ladder[].first_viewport_request_zoom`, `_k` | — | that request's depth and `k` |
| `ladder[].first_viewport_served`, `_occupied_tiles` | count | tiles-frame `served` summed, and its row count |
| `ladder[].first_viewport_bytes` | bytes | the response body, frames and all |
| `ladder[].first_viewport_server_ms` | ms | post-admission to the sweep's end |
| `ladder[].first_viewport_stream_ms` | ms | post-admission to the trailer; `null` on a shed stream |
| `ladder[].first_viewport_shed` | bool | that request's stream was cut mid-body |
| `ladder[].cells[]` | — | one per (zoom, decile, which) |
| `cells[].request_zoom` | depth | the depth the box was requested at |
| `cells[].density_visible_100pc` | count | the box's `visible` under the 100% principal |
| `cells[].conditions.<name>` | — | `cold`, `cold_pages_warm_engine`, `hot` |
| `conditions.*.server_ms`, `stream_ms`, `wall_ms` | ms | percentiles over proven-cold samples: the sweep, the stream, end-to-end |
| `conditions.*.served` | count | percentiles over each sample's `served` sum |
| `conditions.*.response_bytes` | bytes | percentiles over each sample's whole body |
| `conditions.*.request_zoom`, `.k` | — | the depth and `k` the samples carried |
| `conditions.*.all` | — | the same blocks over every sample, proven or not |
| `conditions.*.proven_cold` | count | samples whose `majflt` delta was non-zero |
| `conditions.*.eviction_failed` | count | cold samples with a zero delta, which can also mean no page was needed |
| `conditions.*.shed` | count | samples whose stream was cut |
| `conditions.*.failed` | count | samples whose request did not answer at all |
| `conditions.*.occupied_tiles` | count | the first sample's tiles-frame row count |
| `ladder[].battery` | — | percentiles over cells' own p50/p99, not pooled samples |
| `text_and_drilldown` | — | a `match` on the text column, common and rare, hot and cold, and a drill-down |

### §3 `ingest[]` — one cell

An ingest cycle ingests every declared view, the anchor view first, then each other plain view
and each view of every group. An entity is allocated on the first pass that holds it and joins
on each later one. The hold-out is a set of entities, drawn from every view's points, so an
entity held back is held back from every view, and its rows are ingested in each view's own pass.
A group's views read either their own file or one file shared by the group, picked out by its
discriminator column (`view`, or the name its `fields.view` gives). A roster is read in any of the
build's three forms: `[[view_group.view]]` blocks, a `[view_group.views]` table, or keys taken
from the discriminator's distinct values. Not built yet: a view whose `fields` give its geometry
as `morton` and `residual` rather than a coordinate pair stops the cycle with a driver failure,
and a layer whose `fields` rename its roster's columns fails its publication, since the roster is
read under the canonical names. An
attribute read from a file of its own is joined on entity id onto the batches of every view it
applies to, picked by view key for a group-scoped one, as the build reads it beside the points.
The fields below are the anchor view's figures; the driver's own result file also carries every
view's, under `ingest_by_view` (below, under "Beyond the schema").

| field | unit | how it was measured |
|---|---|---|
| `fraction` | fraction | entities held back, seeded and uniform |
| `concurrency` | count | concurrent callers on `/control/ingest` |
| `seed` | — | the split's seed |
| `base_rows`, `holdout_rows` | count | entities in the base and in the hold-out: the cycle's own `base_entities` and `holdout_entities` |
| `blocked` | object/absent | the cell did not run: where, and the refusal, verbatim |
| `base_build`, `base_build_stages` | — | `mosaica build` over the complement alone, on §1's fields |
| `publish` | — | the publication, per layer — below |
| `items_per_s` | rows/s | rows acked ÷ the hold-out's wall |
| `accepted` | count | rows the route answered 200 for |
| `ack_p50`, `ack_p99` | ms | per-batch ack latency, nearest rank |
| `statuses` | — | every HTTP status seen; 429 is retried backpressure, not an error |
| `max_body_bytes` | bytes | the byte cap bodies were kept under, from `/control/status` |
| `bodies_split` | count | bodies over the cap, halved and re-encoded until each half fits; eight-way counts 7 |
| `largest_body_bytes` | bytes | the largest body sent |
| `bodies_over_cap` | count | single-row bodies over the cap, sent as they are and refused 422 |
| `flush_s` | seconds | `POST /control/flush` to `/control/status`'s `publication` reaching the number the flush answered: the cycle carrying every buffered row has published |
| `visibility_s` | seconds | the same request to a zoom-0 viewport reaching the expected count, not `flush_s` |
| `fold_s` | seconds | the server's own `compaction.last_secs`; compact answers 202 at once, and the wait ends when a fold lands or the server counts one discarded or refused |
| `fold_peak_rss` | bytes | the server's own `compaction.last_rss_bytes` |
| `driver_peak_rss` | bytes/`null` | the driver's own `VmHWM` at the cell's end, not the server's |
| `equivalence` | — | the masked-count equivalence test, by surface — below |
| `<phase>.phase_s` | seconds | one phase's own wall — `flush`, `fold`, `equivalence`, `write_cycle`, `restart` — whether it held or failed |
| `write_cycle` | — | deletes, suppressions, re-ingests, a fold and every view's count again, each with its latency to visibility. A deleted item is re-ingested in every view it was in, the anchor's first; `by_view` holds each view's count before, the suppressed items it holds, and its count after against the expected one; `reingest.items_with_several_ids` counts items answered with different `mosaica_id`s by different views' passes |

`publish` covers one phase: the base bundle carries the built fraction's points and every layer's
declaration; each layer's artifacts, memberships and supplied content are published through
`PUT /control/layers/{name}/artifacts` after every point they depend on is ingested. A roster
may carry its members itself, as a `members` list column, and a group-scoped layer's rows name
their view in its `fields.view` column. A layer whose artifacts are written in the declaration
is carried by the base build and published nowhere.

| field | unit | how it was measured |
|---|---|---|
| `layers.<name>.artifacts`, `.members`, `.generating_set_entries` | count | what the roster and member table declared |
| `layers.<name>.published_artifacts`, `.published_members` | count | what the route answered 200/201 for; a shortfall is a refusal, in `first_refusal` |
| `layers.<name>.requests` | count | publications and growths together, split under `--publish-max-bytes` |
| `layers.<name>.grown_artifacts`, `.grow_requests`, `.grown_members`, `.grown_members_joined` | count | artifacts too large to publish whole: sent with as many members as fit, then grown by `PATCH` in slices, parent-before-child. `_joined` is the route's own `joined` summed; `_unjoined` is the rest, already held or refused |
| `layers.<name>.read_path` | — | `streamed` when row groups don't overlap and parents precede children; `partitioned` otherwise; `no member table` for a roster without one |
| `layers.<name>.row_groups`, `.buckets`, `.count_s`, `.partition_s` | — | row groups; the partitioned reader's bucket count and pass walls, `null` when streamed |
| `layers.<name>.prepared_s` | seconds | the driver's own time in the body generator |
| `layers.<name>.wall_s`, `.artifacts_per_s`, `.members_per_s` | — | the requests' round trips, one caller, serial |
| `layers.<name>.phase_s` | seconds | the layer's whole publication phase |
| `layers.<name>.edges_declared`, `.artifacts_with_several_parents` | count | the roster's `parent` lists, the only place edges are spelled |
| `layers.<name>.edges_published` | count | edges on artifacts answered 201; parents sent before children |
| `layers.<name>.edges_dropped_to_declined` | count | a child's edges to a declined parent, dropped so the child still sends; a refused parent refuses its children too |
| `layers.<name>.refusals`, `.first_refusal_by_status` | — | requests not answered 201, first refusal's level and detail per status |
| `layers.<name>.declined_artifacts[]`, `.declined_members` | — | artifacts whose key, content and parents alone exceed the cap |
| `on_column` | object | layers whose membership travelled on the ingest column, not a roster: `member_rows` the whole table; `roster` that layer's roster publication (below); `holdout` is `rows_with_keys`/`rows_without` a key |
| `declined` | object | declared layers not published, each with a `reason`: attribute membership, supplied content with no roster, or a failure. Every layer is under `layers`, `on_column` or here |
| `edges_declared`, `edges_published` | count | the two summed over the layers |

`equivalence` compares masked counts, per view, on five surfaces that are not equally comparable:
`zoom0_equal`, the whole extent under each principal, frame-independent; `filters_equal`, below;
`boxes_equal`, a box at
each census zoom, frame-dependent (`extent = "auto"` fits a base built from the complement onto a
slightly different grid, so a box's margins can disagree by a handful of rows — `frames` carries
both quantisations, `frames_equal` whether they match); `layers_equal`, the served-artifact frame
per layer — artifact count, summed masked count, and the same two by the level each artifact was
served at — since any one alone passes a different defect; `parents_equal`, a layer's parent edges
per artifact, compared as sets so reordering is not a difference.

Every census request carries `layers: "all"`, so each box compares the layers and the parents at
the levels its own zoom serves: a tiered layer answers only the levels whose declared zoom range
covers the request's zoom, and a served artifact names a parent only where the same response
carries that parent. The census zooms are 3, 6 and 9, and the lower bound of every declared level
range that none of those falls in, which reaches each level of a tiered layer and lands where
neighbouring levels overlap, the only place a parent edge between them can be compared. The boxes
are `--equivalence-boxes` per zoom, drawn from the seed, ranked by what the 100% principal sees at
that zoom on the all-in deployment and taken from the densest three deciles, then asked of both
deployments and every principal: a box with no artifacts in it compares nothing. A layers or
parents difference found inside a box names the box and counts under `layers` or `parents` all the
same.

Each view's census also asks, under the broadest and the narrowest principal, a filter or two per
filter operand `/v1/meta` offers on that view, a group-scoped family on the views of its keys
included: a numeric column's presence and upper half, a category or keyword column's three
commonest values, and a text column's two commonest words of four letters or more, and
`/v1/categories` for each category column, followed through every page. Each value is drawn from
up to 200,000 rows of the file the view's batches take the column from, kept to the entities the
view holds wherever that file is not the view's own points file, and a text column's words are
the tokens `mosaica tokenise` produces under the column's declared analyser. A matched count or
value list that differs counts under `filters`, and a probe answer that did not arrive whole is a
sentence under `incomplete`.

`equivalence.views` holds each view's own comparison, keyed by name, with the five flags above,
`frames`, `frames_equal`, `census_coverage`, `differences` and `differences_by_surface`.
`census_coverage` is what the folded deployment's census reached, read from the principal that
sees the most: `zooms`, the artifacts and parent edges compared at each census zoom, and `layers`,
each layer's `declared_levels` against the levels an artifact was served at, with
`levels_compared`, `levels_declared`, `levels_missing` and the layer's own parent edges. A level in
`levels_missing` is a failure sentence, since a census that compares no artifact at a level proves
nothing there. `census_coverage.visible` is the broadest principal's zoom-0 count and
`census_coverage.filters` counts the filter operands `/v1/meta` offers on the view (`offered`),
the probes compared, those matching something, and the category lists, and names each operand that
produced no probe (`unprobed`) and each probe whose answer on the all-in build matched nothing
(`unmatched_on_all_in`). Each unprobed operand and each unmatched probe is a failure sentence. `equivalence.views_compared_nowhere` names every view the all-in build serves that the
census did not reach or saw nothing in, each a failure sentence. The top-level `equal` is every view's `equal`, `incomplete` is a sentence per census
request that did not arrive whole, and the other top-level fields are the views' own summed or
concatenated.

## Beyond the schema

`measurements.json`'s `ingest[]` cell is a subset of one `ingest_cycle` run's own result — what
`workload.py` reads under `ingest`, and what `--out` writes when the cycle runs alone.

| field | what it is |
|---|---|
| `views` | every declared view's name, anchor first |
| `ingested_view` | the anchor view's name, the one entities are allocated on |
| `ingest_by_view` | one entry per declared view, on `ingest`'s shape; the anchor's is duplicated at the top level as `ingest` |
| `publish_rosters` | a column-route layer's roster (key, content, parents, no members), published before the ingest since a hold-out row's column names a key that must exist. Keyed by layer; a missing roster or failed publish carries `{failed: true, reason}` |
| `minted_credentials` | credential environment variables minted for this run, sorted |
| `view_recreate` | on a rung with a view group, before the write cycle: the last key of a group owning its views is dropped through `DELETE /control/views/{group}/{key}`, which drops it in every group sharing it, created again through `PUT` with its roster record, sent every row of that key's views and that key's artifacts of every layer scoped to one of their groups, and flushed. A column-route layer's roster rows for the key go before the rows, whose batches carry the member column of every column-route layer drawn on the view; `publish_rosters` and `publish` hold the two publications. `answers_after_drop` is each view's viewport status once dropped (404), and `census` each view's census against the all-in build's, which declared the view fresh |
| `restart` | the deployment stopped and reopened, compared against itself rather than the all-in build (a write cycle suppresses rows the all-in build still serves): `open_s`, `visible`, `visible_before`, per-view comparisons, `census_equal` |
| `failures` | plain sentences for what did not hold — a blocked build or serve, a batch not fully accepted, a publication failure, an unequal census surface, a declared level the census compared no artifact at, a census request that did not arrive whole, a fold failure, a restart answering a different count. Empty means the cycle held |

The exit code is 1 if `failures` is non-empty; the result file is written first regardless.
