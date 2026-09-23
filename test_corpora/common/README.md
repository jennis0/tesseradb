# The measurement schema, and the three drivers that fill it

Every rung of the ladder can be measured for the same three things: build cost per stage, view
cost across a principal ladder, and what online ingest sustains through flush, merge and
compaction.

| | |
|---|---|
| [`deployment.py`](deployment.py) | boots a `tessera serve` over an existing bundle, on its own ports and scratch state, inside a transient cgroup scope |
| [`serve_battery.py`](serve_battery.py) | the view-latency battery — a principal ladder, density-decile locations, three conditions |
| [`ingest_cycle/`](ingest_cycle/) | split, build the complement's points and declarations, ingest the hold-out, publish every layer's artifacts, flush, fold, and an equivalence census |
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
export TESSERA_LADDER="$PWD/data/ladder"
python3 -m test_corpora.common.workload --rung arxiv --work /tmp/wl [--quick]
```

This builds `tessera` from the checkout unless `--binary` names one, runs `tessera check`, builds
the all-in bundle into `<work>/<rung>/bundle`, runs `tessera verify --deep` over it, then drives
the battery and the cycle against it under a 16 GiB cap. It prints a correctness line per check,
any failure making the exit code non-zero, and a cost table beside the most recent earlier run of
the same rung and shape. `--quick` is three zooms, three deciles, ten samples a cell and a 2%
hold-out — about seven minutes on arXiv; full mode is every driver's own default and a 10%
hold-out.

The principal ladder is built from the rung's `<axis>-ranks.json`. A rung without one has it
derived into `<work>/<rung>/derived-ranks.json`: one pair per (entity, label) of each view's
`point_visibility` field across every view's points, a missing label counted under the view's
default. The result's `ranks` says which it was.

Each run writes `$TESSERA_LADDER/<rung>/workload-results/<timestamp>-<commit>.json`, holding both
driver results whole. These are not committed: `data/` is git-ignored and they are one box's
figures.

Ports run from `--port0`, default 8171: the battery takes three, the cycle six. 8111–8143 are not
available; other sessions on this checkout use them.

The drivers also run on their own, for a sweep of one knob:

```bash
# the battery against a server it boots itself
python3 -m test_corpora.common.serve_battery --boot-rung <rung> --boot-bundle <bundle> \
    --boot-scratch <scratch> --boot-binary <tessera> --boot-port0 8151 \
    --cap-bytes 17179869184 --out serve-6g.json

# the cycle, one cell per (fraction, concurrency)
python3 -m test_corpora.common.ingest_cycle --rung-dir <rung> --work <scratch> \
    --binary <tessera> --fraction 0.10 --concurrency 8 --write-cycle --state-extent \
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

## The workload's own result file

Holds both driver results whole, under `serve` and `ingest`, beside:

| field | what it is |
|---|---|
| `rung`, `quick` | the run's arguments |
| `started_at`, `host` | when and where |
| `commit`, `dirty` | the checkout's git commit, and whether it had uncommitted changes |
| `binary` | the path to the `tessera` binary measured |
| `minted_credentials` | credential and identity-key environment variables minted for this run, sorted |
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
  "binary_commit": "…",          // the tessera binary's git commit; a cell may override it
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
| `stages[]` | — | `tessera build --stage-timings-json`, one object per pipeline stage in report order |
| `stages[].stage` | — | the stage's name, from `tessera_build::observer::BuildStage` |
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
entity's rows in every view are held back and ingested together. A group's views read either
their own file or one file shared by the group, picked out by its `fields.view` column. An
attribute read from a file of its own is joined on entity id onto the batches of every view it
applies to, picked by view key for a group-scoped one, as the build reads it beside the points.
The fields below are the anchor view's figures; the driver's own result file also carries every
view's, under `ingest_by_view` (below, under "Beyond the schema").

| field | unit | how it was measured |
|---|---|---|
| `fraction` | fraction | entities held back, seeded and uniform |
| `concurrency` | count | concurrent callers on `/control/ingest` |
| `seed` | — | the split's seed |
| `base_rows`, `holdout_rows` | count | the split |
| `blocked` | object/absent | the cell did not run: where, and the refusal, verbatim |
| `base_build`, `base_build_stages` | — | `tessera build` over the complement alone, on §1's fields |
| `publish` | — | the publication, per layer — below |
| `items_per_s` | rows/s | rows acked ÷ the hold-out's wall |
| `accepted` | count | rows the route answered 200 for |
| `ack_p50`, `ack_p99` | ms | per-batch ack latency, nearest rank |
| `statuses` | — | every HTTP status seen; 429 is retried backpressure, not an error |
| `max_body_bytes` | bytes | the byte cap bodies were kept under, from `/control/status` |
| `bodies_split` | count | bodies over the cap, halved and re-encoded until each half fits; eight-way counts 7 |
| `largest_body_bytes` | bytes | the largest body sent |
| `bodies_over_cap` | count | single-row bodies over the cap, sent as they are and refused 422 |
| `flush_s` | seconds | `POST /control/flush` to the `flushes` counter moving, or the buffer already empty |
| `visibility_s` | seconds | the same request to a zoom-0 viewport reaching the expected count, not `flush_s` |
| `fold_s` | seconds | the server's own `compaction.last_secs`; compact answers 202 at once, and the wait ends when a fold lands or the server counts one discarded |
| `fold_peak_rss` | bytes | the server's own `compaction.last_rss_bytes` |
| `driver_peak_rss` | bytes/`null` | the driver's own `VmHWM` at the cell's end, not the server's |
| `equivalence` | — | the masked-count equivalence test, by surface — below |
| `<phase>.phase_s` | seconds | one phase's own wall — `flush`, `fold`, `equivalence`, `write_cycle`, `restart` — whether it held or failed |
| `write_cycle` | — | deletes, suppressions, re-ingests, a fold and census again, each with its latency to visibility |

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

`equivalence` compares masked counts, per view, on four surfaces that are not equally comparable:
`zoom0_equal`, the whole extent under each principal, frame-independent; `boxes_equal`, a box at
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

`equivalence.views` holds each view's own comparison, keyed by name, with the four flags above,
`frames`, `frames_equal`, `census_coverage`, `differences` and `differences_by_surface`.
`census_coverage` is what the folded deployment's census reached, read from the principal that
sees the most: `zooms`, the artifacts and parent edges compared at each census zoom, and `layers`,
each layer's `declared_levels` against the levels an artifact was served at, with
`levels_compared`, `levels_declared`, `levels_missing` and the layer's own parent edges. A level in
`levels_missing` is a failure sentence, since a census that compares no artifact at a level proves
nothing there. The top-level `equal` is every view's `equal`, `incomplete` is a sentence per census
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
| `minted_credentials` | credential and identity-key environment variables minted for this run, sorted |
| `restart` | the deployment stopped and reopened, compared against itself rather than the all-in build (a write cycle suppresses rows the all-in build still serves): `open_s`, `visible`, `visible_before`, per-view comparisons, `census_equal` |
| `failures` | plain sentences for what did not hold — a blocked build or serve, a batch not fully accepted, a publication failure, an unequal census surface, a declared level the census compared no artifact at, a census request that did not arrive whole, a fold failure, a restart answering a different count. Empty means the cycle held |

The exit code is 1 if `failures` is non-empty; the result file is written first regardless.
