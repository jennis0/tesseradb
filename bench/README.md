# Running the benchmarks

How to run the suite at whatever scales and label sets you want, and how to read what comes out.

Findings and methodology live elsewhere: each arm's module doc in
[`crates/tessera-bench/src/arms/`](../crates/tessera-bench/src/arms/) records what it measured and
what it means, and the design memos in [`docs/evidence/memos/`](../docs/evidence/memos/) carry the
individual findings. This file is operational.

---

## 0. Prerequisites

```bash
cargo build --release -p tessera-cli     # the build/serve binary
cargo build --release -p tessera-bench   # the harness (bench-timing is ON by default here)
```

The corpus must be present under `data/scaled/` — `geometry.parquet` (10⁹ rows) plus one
`<label-set>.pairs.parquet` per label set. See [`probes/dataset.md`](../probes/dataset.md) for what
it is and how it was built.

The concurrency arm additionally needs the Python environment at `reference/.venv`.

---

## 1. The two configuration axes

**Scale is a prefix filter, not a separate corpus.** `tessera build --limit N` keeps source rows
with `entity_id < N`. Any N works — 5,000,000 is as valid as the defaults — because there is one
geometry file and a scale is a view onto it. Never take "the first N rows" of `geometry.parquet`
instead: it is stored in Morton order, not entity order (`probes/dataset.md` §5 rule 1).

**Label set is the pairs file.** Geometry is label-independent and shared; only the
`(entity_id, term_id)` relation changes. But the label set *does* decide entity-ID assignment via
the signature sort, so it cannot be varied without a rebuild — see §6.

| label set | terms @10⁹ | character |
|---|---|---|
| `categories-archive` | 15,694 | maximal entity-space contiguity |
| `categories-subclass` | 47,968 | realistic baseline |
| `hash-flat` | 10,000 | maximal scatter — **the G0b control arm** |
| `surnames` | 116,946,544 | dictionary scale, near-unique signatures |
| `hiterms`, `hiterms-ov0.5`, `hiterms-ov0.9` | 1,000,000 | high terms-per-item |

> **The `hiterms` trio covers `entity_id < 10⁷` only.** Above that its tail carries no terms at
> all, so those items are invisible to every principal — a corpus with a hole in it rather than a
> smaller corpus. `bench_build_fixtures.sh` skips those combinations rather than building them.

---

## 2. Build fixtures

Defaults are 250k / 2.42M / 25M across all seven label sets (~7 GB, ~40 min):

```bash
./scripts/bench_build_fixtures.sh
```

Both axes take comma-separated flags; `--help` lists them all:

```bash
# Just the two contiguity extremes, at one scale
./scripts/bench_build_fixtures.sh --scales 2422486 --label-sets categories-archive,hash-flat

# A scale that isn't one of the defaults — any integer works, it's a --limit prefix filter
./scripts/bench_build_fixtures.sh --scales 5000000 --label-sets categories-subclass

# Somewhere other than /tmp, and preview instead of building
./scripts/bench_build_fixtures.sh --fixtures /data/fixtures --list
```

`--list` prints the plan and builds nothing — worth running first, since a full default build is
~40 minutes. Idempotent otherwise: a bundle whose `CURRENT` exists is reported `HAVE` and skipped,
so a killed run resumes by re-invocation. Per-build logs go to `--log-dir` (default
`/tmp/tessera-bench/logs`).

Budget roughly **47 B/item** of disk (measured flat across 100× of scale), plus the label set's
postings — `hiterms` is ~10× the others because it carries ~130 terms per item.

Check what you have:

```bash
./target/release/tessera-bench fixtures
```

---

## 3. Run one arm

Every arm takes `--scale` and `--label-set` as comma-separated filters. Omit them for every
fixture found. A filter matching nothing is an error, not an empty run.

```bash
# Mask construction across every fixture
./target/release/tessera-bench authorise --run-dir /tmp/run1

# One scale, two label sets, custom axes
./target/release/tessera-bench gather \
    --scale 25000000 --label-set categories-archive,hash-flat \
    --pattern contiguous,scattered --k 1000 --coverage 0.05 \
    --repeat 7 --run-dir /tmp/run1

# Whole-viewport latency with per-stage attribution
./target/release/tessera-bench viewport \
    --scale 2422486 --mode battery --k 30 --coverage 0.05 --run-dir /tmp/run1

# Design §7.2's density rule, swept over its own clause parameters
./target/release/tessera-bench viewport \
    --scale 2422486 --mode battery --k 1000 --coverage 0.05 \
    --k-max-marks 30,128,500,1000 --theta-target 16,128 --run-dir /tmp/selection
```

**`--k-max-marks` and `--theta-target` are not free axes.** §7.2's clause parameters are resolved at
`Engine::open`, not per request, so each value costs a full open — which digest-verifies the bundle.
Both default to the server's own single value, so an ordinary run pays nothing; pass a list only
when the sweep is the point. What they move that `--k` does not: the cap bounds the selection heap
and the output gather, and θ decides how many tiles skip the counting pass entirely. A
`--theta-target` large enough to saturate (e.g. `1099511627776`) makes every tile take the serve-all
branch, which is the control arm for the other values.

`--help` on any subcommand lists its axes and defaults. Global flags: `--fixtures`, `--run-dir`,
`--repeat`, `--scale`, `--label-set`.

Arms available: `authorise`, `tiles`, `gather`, `viewport`, `changes`, `ingest-batch`,
`ingest-continuous`, `ingest-build`, `load`, plus `fixtures`, `calibrate` and `matrix`.

Run `calibrate` before believing anything — it reports clock overhead (the instrumentation's own
perturbation), core count, free memory, and whether `bench-timing` is actually on.

---

## 4. Run a campaign

[`bench/matrix.toml`](matrix.toml) declares which arms run at which scales against which label
sets. Preview before committing to a long run:

```bash
./target/release/tessera-bench matrix --plan
```

It reports **cell counts**, which is the number that matters — the obvious every-axis matrix is
~27,000 cells; the shipped one is ~6,900. Then:

```bash
./target/release/tessera-bench matrix --run-dir /tmp/tessera-bench/runs/$(date +%F)
./target/release/tessera-bench matrix --only gather --run-dir ...   # one arm
```

Resumable at cell granularity: re-invoking the same command skips what the run directory's ledger
records as done. A failing arm doesn't abort the campaign, and only successes are recorded, so a
re-run picks up exactly what's missing.

**To change the campaign**, edit `matrix.toml`. Empty `scales` or `label_sets` means every fixture
discovered. Per-arm `repeat` and `seed` override `[defaults]`. The file explains why it's trimmed
the way it is — the short version is that grants are free to vary within a bundle and label sets
are not, so the label-set axis only earns its cost where signature-sorted contiguity is under test.

> Keep `hash-flat` in at least one arm. It is gate G0b's control: its run ratio must come back at
> exactly 1.00 or the estimator is broken and no other ratio in the run is interpretable.

---

## 5. Concurrency, and blank-database ingest

Two arms don't fit the matrix.

**`load`** needs a booted server, N authorised sessions, and server-side RSS/CPU sampling, so it's
driven from Python:

```bash
reference/.venv/bin/python scripts/bench_concurrency.py \
    --bundle /tmp/tessera-bench/fixtures/2422486/categories-subclass \
    --scale 2422486 --concurrency 5,10,100,1000 --arms B,A --duration 8 --w 10
```

`--arms B` shares fragments across users (request-path contention); `--arms A` gives each user its
own grant set (per-session memory, and the F4 lock). It calibrates against `/healthz` first and
flags any cell within 3× of the generator's own ceiling.

**`ingest-build`** is minutes per cell and writes a whole bundle to temp:

```bash
./target/release/tessera-bench ingest-build \
    --scale 250000,2422486,25000000 --label-set categories-subclass \
    --data-root . --run-dir /tmp/run1
```

---

## 6. Adding a label set

The label set decides entity-ID assignment through the signature sort, so a new one needs a new
bundle — it cannot be synthesised from an existing one at query time.

1. Generate `data/scaled/pairs/<name>.pairs.parquet` with columns `(entity_id, term_id)`. The
   Phase 0 generators are `probes/gen_pairs.py` and `probes/gen_hiterms.py`.
2. `./scripts/bench_build_fixtures.sh --label-sets <name>`
3. Add it to `matrix.toml`, or pass `--label-set <name>` directly.

If it has an entity cap, add it to `CAP` in `bench_build_fixtures.sh` so out-of-range scales are
skipped rather than silently built with a hole.

**What you don't need a new label set for:** grant width, coverage, scatter, or posting shape.
Those are all constructible from grants within any existing bundle, for free — that's what
`--w`, `--shape` and `--coverage` sweep. The only thing a rebuild buys is a different entity-ID
assignment.

---

## 7. Collate and gate

```bash
# First run: record the baseline
reference/.venv/bin/python scripts/bench_collate.py --run-dir /tmp/run1 --tag tier1 --update

# Later runs: compare, and fail on regression
reference/.venv/bin/python scripts/bench_collate.py --run-dir /tmp/run1 --tag tier1 --gates
```

Exit codes: `0` pass, `1` a gate failed, `2` the run itself is invalid (G0/G0b). Baselines land in
`docs/archive/plans/bench-baselines/`.

Gates, most authoritative first:

| gate | checks | on failure |
|---|---|---|
| **G0** | container counts and Σvisible identical to baseline | **the run is void** — the corpus or grant construction moved, so no latency comparison means anything |
| **G0b** | `hash-flat` run ratio == 1.00 | the estimator is broken; abort before reporting |
| **G1** | normalised costs (ns per row-visible / point / tile / container) within +15% | regression |
| **G2** | battery p99−p50 within +15% | a per-request stall, with workload variance removed |
| **G3** | each stage's share of total within 10 points | one stage got slower while another got faster |

Raw random-sweep p99 is emitted for a dashboard but is **never** a gate: the
tail-attribution memo showed it mostly
measures the input distribution, not the system.

---

## 8. Reading the output

One JSONL record per cell, per arm, under the run directory. Every record carries `work` (container
counts, Σvisible, coverage, run ratio, pages touched) beside `timing` — deliberately, because the
cost model is O(containers touched) and a latency without a container count can't be told apart
from a workload that simply moved.

`min_ns` is the headline, following Phase 0: *"a single-shot first pass reported figures 5–10×
higher and was noise — repeat before believing."*

Flags worth knowing:

| flag | meaning |
|---|---|
| `degenerate` | coverage ≈ 100% — measures the *absence* of masking; never compare against a real principal |
| `low_container_resolution` | fewer than 32 containers, so `ns_per_container` is meaningless. A corpus of 250k spans **4** containers total; 2.42M spans 37; 25M spans 382. Excluded from G1 |
| `clamped_w=N` | the requested grant width exceeded the vocabulary. At 2.42M `categories-subclass` there are only 176 terms, so `w=1000` and `w=10000` both measure `w=176` |
| `selection_overdraw=Nx` | selection materialised N× more rows than it returned — finding F1 |
| `generator_bound` | throughput within 3× of the load generator's own ceiling; a floor, not a measurement |
| `major_faults` | the cell read from disk, so its latency is a page-cache artefact |
| `single_repetition` | a build, timed once — min-of-N is unaffordable at minutes per cell |

With `bench-timing` (on by default in `tessera-bench`), viewport records also carry `stages`: a
per-stage nanosecond breakdown plus `clock_overhead_ns`, the perturbation the instrumentation
itself introduced. Subtract it rather than assuming it's zero.

**Since the intra-request rayon tile sweep (D-D/D-F), `stages`' per-tile fields
(`count_ns`/`select_ns`/`gather_ns`/`underlay_ns` and the row counters beside them) stop
partitioning wall clock the moment `EngineConfig::compute_threads > 1`.** They become
cross-worker CPU-time sums, so their total can legitimately exceed the request's own wall time —
`unattributed_ns` correspondingly floors at zero rather than reporting idle time. Comparing a
`compute_threads = 1` cell against a `compute_threads > 1` cell on these fields is comparing CPU
time against CPU time, which is a meaningful comparison for *cost*, but not for *latency* — read
`min_ns`/wall clock for latency, and the per-stage breakdown for where CPU time went. See
`tessera_engine::timing`'s module doc for the full reasoning; nothing above this paragraph
changed.

**That cross-worker-sum behaviour only applies above the calibration serial fallback.** Below
`SERIAL_FALLBACK_MAX_ROWS` (200,000 rows-in-ranges), `Engine::viewport` folds the tile sweep
serially regardless of `compute_threads` — the `pool.install` fan-out never runs for those cells —
so for a request below that line, the per-tile stage fields still partition the request's own
wall clock exactly as before the intra-request rayon change, at ANY thread count. Read
`env`/`work` in the record (or the request's own row-range total) to tell which regime a cell fell
into before treating its per-tile fields as either a wall-clock partition or a CPU-time sum.

---

## 9. Things that will bite you

**Everything is WSL2 on a shared 12-core box.** `probes/results.md` §1 is the standing instruction:
*"treat ratios as evidence and absolutes as a starting point."* Check `env.load_avg` in the records
— a fixture build running in another terminal contaminates everything.

**The container axis has little range at these scales.** A Roaring container holds 2¹⁶ entities, so
the whole corpus spans 4 containers at 250k and 382 at 25M, against 15,259 at 10⁹. Phase 0's cost
model was fitted across 399 → 1.5M containers. Below 32 the normalisation is flagged and excluded.

**A uniform-random viewport usually contains nothing.** The geometry is UMAP output and heavily
concentrated. The `battery` mode probes the corpus for density deciles before measuring; an
unprobed random battery measures the cost of finding nothing, ten times.

**Ingested rows never become visible.** Phase 1 has no flush, so a buffered entity has no row in
the segment and `compose` skips it. `ingest-continuous` measures composition over *rejected*
entries. The `changes` arm measures resolved ones, which is why the two together bracket F2 rather
than duplicating each other.

**`ingest-build` at the smallest scale is contaminated if it runs first** — it reads the 6.6 GB
`geometry.parquet` cold while later builds hit the page cache. Compare adjacent scales, not the
first row.
