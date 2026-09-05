# The measurement schema, and the three drivers that fill it

Every rung of the ladder records the same three things (owner rulings, 2026-09-03): what the
**build** cost per stage, what a **view** costs across a principal ladder, and what **online
ingest** sustains through flush, merge and compaction. They land in one committed file per rung,
`test_corpora/<rung>/measurements.json`, and [`scripts/campaign_report.py`](../../scripts/campaign_report.py)
renders the campaign table in [`docs/ingest-campaign.md`](../../docs/ingest-campaign.md) from those
files. **The table is generated; do not hand-edit the marked block.**

| | |
|---|---|
| [`deployment.py`](deployment.py) | boots a `tessera serve` over an existing bundle, on its own ports and scratch state, always inside a transient cgroup scope |
| [`serve_battery.py`](serve_battery.py) | the view-latency battery — a principal ladder, density-decile locations, three conditions |
| [`ingest_cycle.py`](ingest_cycle.py) | the ingest cycle — split, build the complement's **points and declarations**, ingest the hold-out, publish every layer's artifacts, flush, fold, and decision 0091's equivalence test |
| `scripts/campaign_report.py assemble` | collates a rung's driver outputs into `measurements.json` on the schema below |

## Running one rung

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# 1. the build's own record — the flag is on `tessera build`
tessera build --stage-timings --stage-timings-json stage-timings.json

# 2. the battery, uncapped and capped, against a server you boot (see deployment.py)
python3 -m test_corpora.common.serve_battery --viewer … --session … --session-cred … \
    --bundle <bundle> --cache <scratch>/cache --ranks <rung>/branch-ranks.json \
    --server-pid <pid> --cgroup <scope cgroup> --out serve-nocap.json

# 3. the ingest cycle, one cell per (fraction, concurrency)
python3 -m test_corpora.common.ingest_cycle --rung-dir <rung> --work <scratch> \
    --binary <tessera> --fraction 0.10 --concurrency 8 --write-cycle --out ingest-10.json

# 4. collate and render
python3 scripts/campaign_report.py assemble --rung medcpt --rows 35920666 \
    --build stage-timings.json --bundle <bundle> \
    --serve serve-nocap.json --serve serve-6g.json --ingest ingest-10.json \
    --out test_corpora/medcpt/measurements.json
python3 scripts/campaign_report.py
```

Ports are the caller's to choose and **8111–8143 are not available**: other sessions on this
checkout use them.

## `measurements.json`

`schema_version` is `3`. A renderer refuses a file whose version it does not know rather than
reading its fields under a schema they were not written to.

```jsonc
{
  "schema_version": 3,
  "rung": "medcpt",              // the directory under test_corpora/
  "rows": 35920666,              // the corpus's row count, as the rung's own README states it
  "binary_commit": "…",          // the git commit of the tessera binary the figures were taken with.
                                 // A cell may carry its own and override it — a rung re-measured
                                 // after an engine change has figures from two binaries, and a
                                 // single top-level commit would be a claim about the file that is
                                 // not true of every row in it
  "built_at": "2026-09-03",
  "host": "…",                   // free text: this box is 12 cores, 47 GB, WSL2, local NVMe
  "build":  { … },               // §1
  "serve":  [ { … } ],           // §2 — a LIST, one entry per cap
  "ingest": [ { … } ],           // §3 — one entry per (fraction, concurrency) cell
  "notes":  [ "…" ]              // anything a number cannot carry: what was reduced, and why
}
```

⊘ **`serve` is a list where the brief that commissioned this said an object.** A rung is measured
uncapped *and* under a cap and both are results; one object could hold only one of them, and a
second file per rung would put two halves of one measurement in two places.

### §1 `build`

| field | unit | how it was measured |
|---|---|---|
| `stages[]` | — | `tessera build --stage-timings-json`, one object per pipeline stage in report order |
| `stages[].stage` | — | the stage's name, from `tessera_build::observer::BuildStage` |
| `stages[].wall_s` | seconds | the stage timer's own elapsed |
| `stages[].rows` | count | whatever the stage counted — items, pairs or terms, **stage-specific** |
| `stages[].peak_rss_kib` | KiB | the *process's* `VmHWM` when the stage ended, not the stage's own; it only rises |
| `stages[].started_at`, `ended_at` | Unix seconds | wall clock. ⊘ For the four stages `StageTimer::charge` reports, an interval of the measured length placed at the report, not a real boundary |
| `wall_s` | seconds | the sum of the stages, not a separately-timed total |
| `peak_rss_kib` | KiB | the largest `peak_rss_kib` any stage reported |
| `bundle_bytes` | bytes | every file under the bundle root, summed |
| `rss` | — | present only when a run was sampled with `probes/2026-09-02-text-peak-split/sample_rss.py`; `VmHWM` cannot split anonymous from file-backed and this is not derived from it |

### §2 `serve[]` — one run of the battery

| field | unit | how it was measured |
|---|---|---|
| `cap` | bytes or `null` | the scope's `MemoryMax`. `null` is uncapped — still inside a scope, so the battery can reclaim |
| `cgroup` | path | the transient scope's cgroup directory, read for `memory.events`, `memory.stat`, `memory.peak` |
| `total_rows` | count | the 100% principal's zoom-0 whole-extent `visible`. **The denominator of every coverage figure** |
| `candidates_per_zoom`, `samples_per_cell`, `zooms`, `deciles`, `cells_per_decile`, `conditions` | — | the run's own parameters, recorded because a reduced run must not look like a full one |
| `oom_kill_seen` | bool | `memory.events`' `oom_kill` moved at any point during the run |
| `ladder[]` | — | one entry per target fraction |
| `ladder[].target` | fraction | what the greedy term composition aimed at |
| `ladder[].terms`, `terms_n` | — | the composed term set. Greedy: fill descending by pair count under the target's budget, then take the one smallest overshooting term if that is closer **in ratio** |
| `ladder[].measured` | fraction | this principal's zoom-0 whole-extent `visible` ÷ `total_rows`. **Measured, never the target** — pairs overlap, so a pair budget is an upper bound on coverage |
| `ladder[].authorise_s` | seconds | one `session/authorise`, wall |
| `ladder[].first_viewport_s` | seconds | the *first* viewport of a fresh session, its own figure: it carries the `(view, principal)` fragment build, which is not a per-request cost |
| `ladder[].cells[]` | — | one per (zoom, decile, which) |
| `cells[].density_visible_100pc` | count | the cell's box's `visible` **under the 100% principal**, which is what makes the decile a property of the corpus rather than of the principal |
| `cells[].conditions.<name>` | — | `cold`, `cold_pages_warm_engine`, `hot` |
| `conditions.*.server_ms`, `wall_ms` | ms | p25/p50/p75/p95/p99/max/mean over the cell's **proven-cold** samples (all samples for `hot`), nearest rank |
| `conditions.*.all` | ms | the same over **every** sample, proven or not |
| `conditions.*.proven_cold` | count | samples whose `majflt` delta over the request was non-zero |
| `conditions.*.eviction_failed` | count | cold samples with a zero delta. ⊘ Under `cold_pages_warm_engine` a zero delta may mean the request needed no file page at all — see `condition_figures` |
| `ladder[].battery` | — | battery-level figures **over cells, not over pooled samples**: each cell contributes its own p50 and p99, and these are percentiles over those |
| `text_and_drilldown` | — | a `match` on the rung's text column with a common and a rare token, hot and cold, and a drill-down |

### §3 `ingest[]` — one cell

| field | unit | how it was measured |
|---|---|---|
| `fraction` | fraction | of **entities** held back from the build, seeded and uniform |
| `concurrency` | count | concurrent callers on `/control/ingest` |
| `base_rows`, `holdout_rows` | count | the split |
| `blocked` | object or absent | the cell did not run: where it stopped and the refusal, verbatim |
| `base_build`, `base_build_stages` | — | `tessera build` over the complement's **points and declarations alone**, same fields as §1 |
| `publish` | — | the publication, per layer — see below |
| `items_per_s` | rows/s | rows **acked** ÷ the wall of the whole hold-out, at that concurrency |
| `ack_p50`, `ack_p99` | ms | per-batch ack latency, nearest rank over the batches |
| `statuses` | — | every HTTP status seen, counted. 429 is backpressure and is retried, not an error |
| `flush_s` | seconds | from `POST /control/flush` to the executor's `flushes` counter moving |
| `visibility_s` | seconds | from the same request to a zoom-0 viewport reaching the expected count. **The number a viewer experiences**, and not the same as `flush_s` |
| `fold_s` | seconds | the server's own `compaction.last_secs`. `POST /control/compact` answers 202 immediately, so an outside timer would measure the request |
| `fold_peak_rss` | bytes | the server's own `compaction.last_rss_bytes` |
| `driver_peak_rss` | bytes or `null` | the driver process's own `VmHWM` when the cell ended, from `/proc/self/status`. The split, the hold-out's batches, the publication's buckets and the census are all in it; the server's memory is not. `null` where the cell was measured before the field existed |
| `equivalence` | — | decision 0091's test, split by surface — see below |
| `write_cycle` | — | deletes, suppressions, re-ingests, a second fold, and the census again; each with its latency to visibility |

**`publish` is its own block because publication is its own phase** (owner ruling, 2026-09-03): the
base bundle carries the built fraction's points and every layer's *declaration*, and each layer's
artifacts, memberships and supplied content are published through
`PUT /control/layers/{name}/artifacts` after every point they depend on has been ingested.

| field | unit | how it was measured |
|---|---|---|
| `layers.<name>.artifacts`, `.members`, `.generating_set_entries` | count | what the rung's roster and member table declared for that layer |
| `layers.<name>.published_artifacts`, `.published_members` | count | what the route answered 201 for. A difference from the two above is a refusal, and `first_refusal` carries it verbatim |
| `layers.<name>.requests` | count | requests sent, publications and growths together. A batch is split between artifacts to stay under `--publish-max-bytes` (clamped to the route's 64 MiB), the batch being the commit unit |
| `layers.<name>.grown_artifacts`, `.grow_requests`, `.grown_members`, `.grown_members_joined` | count | artifacts whose whole membership did not fit under the cap: each is published with its key, content, parents and as many members as fit, then grown through `PATCH /control/layers/{name}/artifacts` (decision 0127) in slices of at most the cap, after the batch that published it and in the same parent-before-child order. `grown_members` is what the slices carried and the route answered 200 to; `grown_members_joined` is the route's own `joined` summed, and `grown_members_unjoined` their difference — members the route already held or refused; the driver logs a line whenever it is not zero |
| `layers.<name>.read_path` | — | how the member table was read: `streamed` where its row groups' key ranges do not overlap and key order puts every parent before its children, `partitioned` otherwise (one counting pass, one pass into on-disk buckets under `--work`, then one bucket at a time), `no member table` for a roster without one |
| `layers.<name>.row_groups`, `.buckets`, `.count_s`, `.partition_s` | — | the member file's row groups; the partitioned reader's bucket count, counting-pass wall and partitioning-pass wall (`null` when streamed) |
| `layers.<name>.prepared_s` | seconds | the driver's share of the phase: time inside the body generator, which reads the member table and assembles each body. **Not** part of the throughput below: it measures pyarrow and NumPy, not the service |
| `layers.<name>.wall_s`, `.artifacts_per_s`, `.members_per_s` | — | the requests' round trips summed, one caller, serial: the service's cost. Bodies are sent as they are assembled, so the two shares interleave in wall-clock time |
| `layers.<name>.phase_s` | seconds | the layer's whole publication phase, end to end |
| `layers.<name>.edges_declared`, `.artifacts_with_several_parents` | count | the roster's `parent` lists, which under `dag` are where a layer's edges are spelled and the only place they are (decision 0125) |
| `layers.<name>.edges_published` | count | edges on artifacts the route answered 201 for. The route had no `parent` field until 2026-09-03 and a published `dag` layer came out flat; parents are published parent-before-child, which is why the roster is reordered by depth first |
| `layers.<name>.edges_dropped_to_declined` | count | a child's edges to a parent the driver declined (below). The route refuses an artifact whose parent the layer does not hold, so the edge is dropped before sending and the child is kept; without this one declined root refuses every descendant's batch and the census lists the whole tree as missing rather than the roots. A parent whose batch the route *refused* is different: its children's batches are refused too, each counted under `refusals` and logged, and the census then lists the subtree |
| `layers.<name>.refusals`, `.first_refusal_by_status` | — | publication requests the route did not answer 201, and the first refusal's level and detail per status. Each status is also logged with its detail the first time it appears, and the layer's log line carries the count |
| `layers.<name>.declined_artifacts[]`, `.declined_members` | — | artifacts whose key, content and parents alone would exceed the cap with no members at all, each with `key`, `members` and `body_bytes`; a membership is grown in slices, so size alone no longer declines an artifact and this should be empty. A layer census difference on such a layer is attributable to these keys |
| `on_column` | object | one entry per layer whose membership took the **column route** (decision 0128): no supplied content and no roster, so the base build read its member table over the base's rows and every hold-out row carried its member list as the ingest batch's column named for the layer. `member_rows` is the rung's whole table; `holdout` is what the hold-out carried — `rows_with_keys` and `rows_without` (a hold-out row the table does not name is in no artifact, as at the build). The same figures are under `ingest.membership_columns` |
| `declined` | object | one entry per declared layer that was **not** published, with a `reason`: an attribute-membership layer has nothing to publish (an ingested row joins it through the column its batch carries); a layer with supplied content and no roster cannot be minted from a column or published; a failure carries its exception. `member_rows` where there is a member file. **Every declared layer is either under `layers` or here.** A layer here is declared and empty on the folded deployment, so every count on it differs from the all-in build's by design, and `equivalence.layers` says so |
| `edges_declared`, `edges_published` | count | the two summed over the layers |

**`equivalence` is split into three surfaces because they are not equally comparable.**

* `zoom0_equal` — the whole extent under each principal. Frame-independent, so this is the
  surface on which "the two deployments agree" is a claim about the data alone.
* `boxes_equal` — a box at zooms 3, 6 and 9. **Frame-dependent**: a rung declaring
  `extent = "auto"` computes its frame from the rows the build saw, so a base built from the
  complement quantises onto a slightly different grid and the margins of a box disagree by a
  handful of rows. `frames` carries both quantisations and `frames_equal` says whether they are
  the same, so a box difference is attributable rather than mysterious.
* `layers_equal` — the kind-5 artifact frame, **per layer**: how many artifacts the principal is
  served on it, and the sum of their masked counts. Both, because an artifact count alone passes a
  defect that serves the right artifacts with the wrong memberships, and a masked-count sum alone
  passes one that moves members between artifacts of the same layer.
