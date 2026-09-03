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
| [`ingest_cycle.py`](ingest_cycle.py) | the ingest cycle — split, build the complement, ingest the hold-out, flush, fold, and decision 0091's equivalence test |
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

`schema_version` is `1`. A renderer refuses a file whose version it does not know rather than
reading its fields under a schema they were not written to.

```jsonc
{
  "schema_version": 1,
  "rung": "medcpt",              // the directory under test_corpora/
  "rows": 35920666,              // the corpus's row count, as the rung's own README states it
  "binary_commit": "…",          // the git commit of the tessera binary every figure was taken with
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
| `content_removed` | list | supplied content kinds the base could not carry, one record each. Only ever non-empty at *f* = 100%: a kind declaring `require_member_visibility = "all"` is served only to a viewer who can see every document it was generated from, and an artifact with no members names an empty generating set, which is satisfied by everyone and is refused at **both** entry points. The kind is dropped from the measurement's own declaration and from the roster that supplied it, so the layer census differs on that surface by design |
| `base_build`, `base_build_stages` | — | `tessera build` over the complement, same fields as §1 |
| `layers` | — | which layers the wire carried, which of those were read off a column of the rows parquet (`from_rows`) rather than inverted from the member table, which it declined and why. **Not patched around**: a layer the wire cannot express means the ingested rows carry no membership on it and every later count on it differs by design. Only `nested` is declined for its kind; a `dag` cell is a set (decision 0125) |
| `items_per_s` | rows/s | rows **acked** ÷ the wall of the whole hold-out, at that concurrency |
| `ack_p50`, `ack_p99` | ms | per-batch ack latency, nearest rank over the batches |
| `statuses` | — | every HTTP status seen, counted. 429 is backpressure and is retried, not an error |
| `flush_s` | seconds | from `POST /control/flush` to the executor's `flushes` counter moving |
| `visibility_s` | seconds | from the same request to a zoom-0 viewport reaching the expected count. **The number a viewer experiences**, and not the same as `flush_s` |
| `fold_s` | seconds | the server's own `compaction.last_secs`. `POST /control/compact` answers 202 immediately, so an outside timer would measure the request |
| `fold_peak_rss` | bytes | the server's own `compaction.last_rss_bytes` |
| `equivalence` | — | decision 0091's test, split by surface — see below |
| `write_cycle` | — | deletes, suppressions, re-ingests, a second fold, and the census again; each with its latency to visibility |

**`equivalence` is split into three surfaces because they are not equally comparable.**

* `zoom0_equal` — the whole extent under each principal. Frame-independent, so this is the
  surface on which "the two deployments agree" is a claim about the data alone.
* `boxes_equal` — a box at zooms 3, 6 and 9. **Frame-dependent**: a rung declaring
  `extent = "auto"` computes its frame from the rows the build saw, so a base built from the
  complement quantises onto a slightly different grid and the margins of a box disagree by a
  handful of rows. `frames` carries both quantisations and `frames_equal` says whether they are
  the same, so a box difference is attributable rather than mysterious.
* `layers_equal` — the artifact frames served with the viewport, by frame kind.
