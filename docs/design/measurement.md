# Measurement — design

**Date:** 2026-08-07
**Status:** Provisional — under review, and **not approved**. **To become normative:** owner
sign-off, the §10 amendments landed in `bench/README.md` and `bench/matrix.toml`, and the schema-2
`Work` fields carried by at least one arm — the two reporting modes (§4) are the part a reader could
otherwise take as already in force.
**Reads against:** architecture §4 (I2, I7), §7.2, §11.3, Appendix A, Appendix C (C4);
conformance §6; [`write-path.md`](write-path.md) §14; [`compaction.md`](compaction.md) §6, §9, §14;
`per-point-attributes.md` §8; `hot-row-geometry.md` §7; `probes/optimisations.md` §0;
`probes/results.md` §1, §5, §6; decisions 0049–0056.
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

**Owns:** what the benchmark suite measures and why — the axes, the denominators, the reporting
conventions, and which figures may be published. **Does not own:** how to run it (that is
[`bench/README.md`](../../bench/README.md)), what any particular run found (the arms' module docs
and `docs/evidence/memos/`), or the correctness gate (`conformance.md`).

---

## 1. Summary

The suite measures a **frozen bundle under a varying request**. The completed ingest pipeline and
per-point attributes break that assumption from different sides: flush, merge and the fold make the
bundle move under a running reader, the attribute tail makes the row wider. The expansion is
therefore not a longer list of arms but **one new axis** — the bundle's own state — carried in the
record beside the container counts that already make a latency portable.

The three publication kinds are not one mechanism at three sizes. Flush and merge are frequent and
invisible to a resident session; the fold costs a full rebuild per resident session and can run for
minutes (spec §2.3). An arm calibrated on the first two will read the third as a stall.

Two conventions follow, and they are the part most easily got wrong:

- `min_ns` over 3–5 repetitions is correct for A/B of a code path and **cannot express a
  percentile**. A merge is a scheduled perturbation on a minority of requests, and a fold is a large
  one; min-of-N is constructed to absorb exactly that.
- Throughput has no single unit here. Requests, served marks and masked rows examined scale on
  three different denominators, and the middle term — the masking arithmetic, which is the product —
  is invisible in the two units a reader reaches for first.

---

## 2. The two missing axes

Every arm today holds the bundle constant and varies the request. Nothing varies the bundle under a
constant request: the arms were written when `tessera build` produced one segment and it stayed.

With the pipeline complete, a reader's cost depends on quantities no existing record carries. A tile
resolves to one contiguous range **per live segment** (§11.3), so a viewport's latency is a function
of the merge policy's recent history. A regression and "the merge policy left more segments live"
are the same number without that count beside it, and it is the most likely false alarm of the next
campaign.

The other half of that expectation is **refuted, and must not be reinstated**. A fragment build does
union across every live delta postings tier, but it is **measured flat in tier count** — 199 ms at
one tier, 198 ms at 512, at 10⁹ with a 25% grant (`probes/2026-08-04-refresh-ladder/`, P2), against
the seconds the write-path design had modelled. A tier count therefore explains an operand count and
not a latency, and an arm that reports one as the other will attribute a stall to the wrong pass.

This is the identical argument [`report.rs`](../../crates/tessera-bench/src/report.rs) already makes
for `containers`, applied to the write path. It is why `work` is not an `Option` there, and the new
fields join it on the same terms.

### 2.1 `Work` gains the write-path denominators

Schema bumps to **2** — a field's meaning does not change, but a reader that assumes `segments` is
absent rather than zero would misread every pre-flush record.

| field | what it explains |
|---|---|
| `segments` | ranges resolved per tile; the merge policy's whole justification |
| `delta_tiers` | operands in a fragment build — an operand count, not a latency term (spec §2) |
| `buffered_items` | `compose_ns` — F2's measured denominator |
| `overlay_entries` | `compose_ns`'s other half, and the deny path's depth |
| `hot_row_bytes` | bytes per served mark, once the tail is variable (spec §5) |
| `attribute_columns` | per-point-attributes §3.6's *assumed* claim that code width is free |

`ack_to_visible_ns` is **not** a `Work` field. It is a latency, and it belongs in `timing` on the
arms that can observe it.

### 2.2 The soak arm

One new arm shape carries the axis: continuous ingest at a configured rate for hours, a reader
drawing a fixed density battery throughout, reporting **time-bucketed** read latency alongside the
§2.1 counts, ack→visible, and RSS.

It is the only place some of write-path §14's obligations are observable at all — §14.11 (the
ack→visibility gap bounded by `flush_max_age_secs`) is a soak property by construction, and §14.13
and §14.18 fit here for the same reason. §14.10 — segment, tier, run and dictionary-extent counts
bounded under sustained ingest — is not one of them: it is already asserted at engine scale by
`crates/tessera-engine/tests/soak.rs`, where forty flushes leave two segments, five delta tiers, two
external-id runs and six dictionary extents, with a control showing each grows one per flush when
the maintenance passes are stopped. What the arm adds there is the same axis under hours of load
rather than forty ticks, with read latency beside it.

> ⊘ **The arm does not exist.** Flush, both halves of merge and the fold all do (write-path §4, §7;
> compaction §4), so the gates in spec §8 have something to bound: the ack→visible gap is
> `flush_max_age_secs` per view rather than unbounded, and G5's counts are the ones `soak.rs`
> already holds. What is missing is only the arm — until it runs, nothing measures read latency
> under sustained ingest at all.

### 2.3 The fold is a third publication kind, and it is not a bigger merge

Flush and merge are cheap, frequent and invisible to a resident session. The fold is none of those,
and treating it as one more point on the merge curve is the mistake this section exists to prevent.

**The flip is measured, and the cost is per resident session, not per byte.** Probe P2
(`docs/evidence/memos/2026-08-05-compaction-flip-and-io.md`): the refresh's per-entry cost on the
flush and merge path is **11.1–24.2 ms**, and the full rebuild a fold forces is **267–352 ms** — a
**15–24×** ratio, linear in the resident population. Scaled to 10⁹ that is ≈12.8 s per entry and a
flip of **≈3 minutes** for the last session, confirmed independently by
`probes/2026-08-04-refresh-ladder/`.

Three settled rulings shape what an arm may assert about it:

- **The aftermath is a cache miss, not a refusal** (decision 0053). A session whose projection is
  missing after the flip rebuilds inline; there is no 429. So the fold's cost appears in the arm as
  a **latency tail on a minority of sessions**, and an arm that only counts errors will report a
  clean run through a three-minute event.
- **The schedule is a gated window, not a timer** (decision 0056). A fold fires on work gauges
  within a window, so a soak that runs for a fixed wall-clock duration may see zero folds or
  several. The count is an outcome to report, never an assumption — and forcing one is the only way
  to measure it deterministically.
- **The IO mitigation is `MADV_SEQUENTIAL`, not a throttle** (decision 0052). The fold's inputs are
  all mappings, so there is no read to rate-limit. The *harm* P3 measured stands (below); the rate
  arms in that memo measure a mechanism that was refuted and **must not be quoted as a throttle
  setting**.

**What an arm must therefore report** is the flip as an interval with a session population attached
— entries refreshed, entries left to miss, and the latency each class saw — rather than a duration.
A fold that took three minutes while every session was idle cost nothing.

### 2.4 Residency, measured

Appendix A's residency arithmetic justifies several shipped decisions, and until probe P3 none of it
had been measured — hot-row-geometry §7 records the reason: the bench had no memory-pressure
mechanism, and a residency saving becomes latency only under contention.

**P3 closed that, and it did so both ways** (`docs/evidence/memos/2026-08-05-compaction-flip-and-io.md`,
extended to the >RAM regime 2026-08-06). A corpus-scale streaming read costs a concurrent viewport up
to **2.03× at a 45.57 GiB bundle against 36.9–38.2 GiB of RAM** — global reclaim, the real thing —
and up to **15.67× at a 7.84 GiB bundle under a 4 GiB cgroup cap**, a harsher 1.96:1 ratio standing
in cheaply for the same effect. So both of the following are now established rather than proposed:

- **Outgrowing RAM is reachable on this hardware.** An earlier revision of this section said it was
  not, reasoning from the 10⁹ bundle's ~14 GB of hot columns against 47 GB of RAM. That was wrong:
  a whole bundle is far larger than its hot columns, and P3 built one at 45.57 GiB.
- **The cgroup cap works as a proxy** and is much cheaper per trial, at the cost of a ratio that is
  chosen rather than natural. Both regimes agree in direction and disagree in magnitude by ~8×, so
  the cheap proxy establishes *shape* and the real regime establishes *scale* — quote them apart.

The mechanism, for a controlled sweep: cgroup v2 is delegated on this box and drives unprivileged,
`systemd-run --user --scope -p MemoryMax=<N> -p MemorySwapMax=0`. `MemorySwapMax=0` is not optional —
with swap available the experiment measures swap thrash rather than page-cache reclaim.

Three mechanisms fail, and the reasons are recorded so they are not retried:

- **`drop_caches`** needs root, is global, and is one-shot. It evicts the binary along with the
  bundle, can only run between cells, and the first request re-warms everything — one cold sample
  per invocation, and none thereafter.
- **`madvise(MADV_DONTNEED)`** on the mappings does not do what the name suggests here. The store
  mmaps its files, and on a file-backed mapping `DONTNEED` drops the page-table entries while
  leaving the pages in page cache, so the next touch is a **minor** fault. That measures fault and
  TLB cost, not IO. Genuine eviction needs unmap → `posix_fadvise(DONTNEED)` → remap, which means an
  engine re-open per trial — and re-open digest-verifies the bundle, reading it all back in.
- **`MADV_SEQUENTIAL` is a hint, not a lever** (decision 0052). It is the fold's mitigation and it
  is not a rate control; an arm cannot use it to set a read rate, and P3's rate arms measure a
  mechanism the r5 review refuted.

**What remains is the read arms, not the mechanism.** P3 measured a *concurrent streaming read*
against a viewport. Nothing yet sweeps the residency ratio *r* = memory available to the page cache
÷ bytes the workload touches, across `viewport`, `gather` and `session` at *r* ≥ 1, 0.5, 0.25, 0.1 —
which is the deployment question, never "is the cache cold" but "the working set exceeds RAM and the
kernel is reclaiming continuously".

Three consequences:

- **The limit counts anon and page cache together**, so a limit low enough is an OOM kill rather
  than an eviction. The floor is set by the anon working set — masks, row projections, session
  caches — which is `load`'s Arm A memory question. The floor is therefore itself a measurement, and
  each cell reports the anon/cache split from `memory.stat` rather than only the limit it was given.
- **`major_faults` inverts.** Today a cell taking major faults is flagged suspect and excluded from
  gating (`Env::bundle_fits_in_ram`), because its latency is a page-cache artefact. In this arm the
  major faults *are* the measurement. The flag must be arm-aware or the arm excludes itself.
- **min-of-N is fatal**, and more directly than anywhere else in spec §4: minimum-over-repetitions
  selects the warmest sample by construction, which is exactly what it was chosen to do. Cells are
  first-touch, or a distribution over independent trials at steady-state pressure.

What a swept read arm would unblock is the comparison hot-row-geometry §7 records as open. The
fixed-row saving — **18 → 12 B/row**, the `x`/`y` pair having become `residual` and `priority` having
been cut (decision 0046) — is arithmetic against Appendix A and **must not be quoted as a
measurement**. P3 does not close it: it establishes that residency has a price, not that this
particular 6 B/row buys any of it. A `variant` field (spec §7) plus the sweep is still the first
construction in which it could.

---

## 3. What the ingest arms measure now

[`bench/README.md`](../../bench/README.md) §9's governing caveat — *"Ingested rows never become
visible"* — is dead: flush ended it, and `crates/tessera-engine/tests/scale.rs` has since carried
10,000,000 rows through 32 rounds, 9 merges and 4 coalesces with every masked total exact after
every round.

**Three readings this document previously carried are withdrawn by the ingest-rate campaign**
(`docs/evidence/memos/2026-08-05-ingest-rate.md`), and none of them should be re-derived from the
older arms:

- **"Ingest throughput is an fsync amortisation story" — withdrawn.** fsync is **11–24%** of a
  serial caller's per-row cost, against `apply_window`'s **38–50%**. The old reading described a
  batch of one and was generalised past its evidence.
- **"~1.37 M items/s at batch=10,000" — withdrawn.** It was a `min` over a rising series, at a term
  density and buffer depth where the costs that dominate a deployment are invisible.
- **F3 is no longer an open question, and it was never an fsync question.** `apply_window`'s
  `B²/2W` clone term is real and is now the axis that matters; the earlier "buried under a ~3.2 ms
  fsync floor" reading held only at depths nobody deploys at.

**There is no single ingest rate.** The spread across plausible deployment shapes is **4.8×** —
250,000–465,000 rows/s submitted for a bulk loader, 150,000–270,000 once the flush that makes those
rows visible is counted, and 97,000 for a single caller sending small batches into a deep buffer.
Which end a deployment gets is decided by three properties, and `ingest-rate` is the arm that sweeps
them: term density (up to **43%** on per-row cost, and only when the buffer is deep), `B/W` (four
commit windows between publications is optimal; flushing every window is 30–42% worse and buffering
twenty-four is 20–36% worse), and caller concurrency.

Two arms therefore keep narrower jobs than their names suggest. `ingest-batch` measures a **ramp,
not a rate** — nothing in it flushes, so every repetition lands on a deeper buffer — and its own doc
now says so. `ingest-continuous` measures `compose` over entries that a flush would have resolved,
so F2's ~10 ns per buffered item is a **lower bound over the cheap branch**; re-baseline it against
a flushing engine rather than inheriting it.

The attribute-tail warning stands and is unaffected: a sweep that stops at batch=1000 will report
attribute cost as free, because per-row costs only separate from the fixed ones above it.

---

## 4. Two reporting modes

`Timing::from_samples` computes `p99` by nearest rank. At `repeat = 5` — [`matrix.toml`](../../bench/matrix.toml)'s
setting for `viewport` and `gather` — that resolves to the last index, so **`p99_ns` is exactly
`max_ns` in every in-process record the suite has produced.** Gate G2 (battery p99−p50 within +15%)
is consequently gating on max-minus-median of five samples: it catches a gross stall and cannot see
a tail.

That is not a defect in the convention. `probes/results.md` §6's min-of-N was chosen to absorb
scheduler noise and page-cache warmth without a warm-up phase, and it does. It is the wrong
convention for a percentile, and the fix is to name the two modes rather than to raise `repeat`
everywhere — the matrix is ~5,600 cells against a possible ~27,000 and that trim is well argued.

**A/B mode** — unchanged. `min_ns` headline, `repeat` 3–5, normalised per container / row-visible /
tile / mark. This is what gates, and what compares two implementations.

**Distribution mode** — duration-bounded (`--duration`) rather than repetition-bounded, collecting
N ≥ 1,000 per cell. Applies to the arms whose output is user-facing or tail-related, at the declared
operating points only. `load` is already in this mode in all but name: it issues hundreds of
thousands of requests per cell and computes percentiles over the complete set before thinning to
`MAX_RETAINED_SAMPLES`.

**Below 100 samples, `p99_ns` is suppressed rather than emitted.** A number that reads as a
percentile and is a maximum is worse than an absent field. This is the same discipline as the
existing `low_container_resolution` flag, which already refuses to report `ns_per_container` below
32 containers.

The soak arm inherits distribution mode by construction, and reports over buckets rather than over a
cell, with flush and merge publications marked on the timeline.

---

## 5. What throughput means here

A viewport's cost has three terms, on three denominators:

| term | scales with | visible in |
|---|---|---|
| per request | — | requests/s |
| **masking** — count pass, compose, projection | `rows_in_ranges`, `sigma_visible`, containers | **neither** |
| output | `points_gathered`, bytes | marks/s, MB/s |

The middle term dominates, and both units a reader reaches for first are blind to it. Arithmetic
from [`load.rs`](../../crates/tessera-bench/src/arms/load.rs)'s recorded Arm B figures — 48,588 rps
at k=30 — puts the server at ~1.46 M served marks/s and, at the points batch's per-point width, tens
of MB/s, while burning 8.2 of 12 cores. Neither rate is near a bottleneck. **Arithmetic, not a
measurement**, and quoted only to locate the cost.

**MB/s served is rejected as a headline, and not merely as uninformative.** It inverts the
incentive: a viewport that counts a billion masked rows and returns thirty marks is the system
working, and by bytes served it is the worst cell in the run — while a degenerate 100%-coverage
principal is the best. Bytes/mark is also a constant today, so the figure is marks/s times a
constant and carries no information at all. It becomes load-bearing exactly when the attribute tail
lands, and then as a **saturation check** — has the serialiser or the wire become the bottleneck —
rather than as a performance metric.

**Masked rows examined per second** is the unit the other two lack. `rows_in_ranges` and
`sigma_visible` are the throughput of the masking arithmetic, and the only unit that does not
collapse when selectivity changes.

### 5.1 The load arm's `work` is a stub

It fills `points_gathered` and `bytes_touched` and leaves the rest at `Default`. The tile stream it
already decodes carries `visible`, `matched` and `served` per tile, and it sums `served` alone.
Summing all three is near-free and gives the arm a real `work` block — without which no cell it
produces can be normalised the way every other arm's can.

---

## 6. The published figures

Nine, and the constraint on them matters more than the list.

1. **Ack→visible — three intervals, not one.** "Visible" has three answers depending on who asks,
   and `scale.rs` has measured all three at 5M and 10M: `ack` (durable and invisible) 1.8–4.6 s for
   a 250k batch; `publish` (visible to a session authorised after it) **0.42–0.58 s**; `refresh`
   (live sessions brought forward) **5–142 ms**. Publishing one number here would be a choice about
   which reader to mislead. The headline claim is the one that dominates: **visibility is ~99%
   tick** — the wait is `flush_max_age_secs`, default 90 s, and the mechanical terms are noise
   beside it.
2. **Rows/second ingested, against the write's *shape*.** The earlier form of this figure —
   against concurrent writers — is **refuted as a headline**: concurrency is worth 3× only to a
   caller sending small batches (97,000 → 286,000 rows/s at eight callers), and buys nothing
   measurable for one sending maximal batches, because `commit_window_max_items` equals
   `ingest_max_batch_rows` by design and one maximal batch fills a window. **Concurrency is not a
   multiplier on the bulk-loader figure**, which is exactly what it was suspected of being. Publish
   the two deployment shapes — bulk loader and small-batch caller — with `B/W` and term density
   named, and publish it **over the wire**: `ingest-rate` is in-process, so it excludes
   serialisation, the handler and the queue.
3. **Requests/second at a stated SLO**, both session arms. Not peak: Arm A at c=1000 sustained
   18,599 rps with a **1.04 s p99** (the F4 projection-lock finding), which a peak-throughput
   headline reports as healthy.
4. **Served marks/second**, which survives a change in `k` where requests/second does not.
5. **Latency against corpus scale** at a fixed viewport, k and coverage held.
6. **Latency against points-in-view**, never against viewport area — the geometry is UMAP output and
   heavily concentrated, so area is not a workload.
7. **The fold's flip, as an interval with a population** (spec §2.3) — entries refreshed, entries
   left to take a cache miss, and the latency each class saw. A deployment needs to know what its
   worst-served session experiences during the most expensive operation in the system, and decision
   0053 makes that a latency rather than an error count.
8. **Build wall-clock, source to `CURRENT`.** End to end — source parquet to a bundle a server has
   *opened*, so "queryable" is demonstrated rather than assumed — never a stage sum, and never
   without the label set named: the signature sort is the stage that bends (2.8 s → 53.4 s for 10×
   the items, 45% of the 25M build —
   [`ingest.rs`](../../crates/tessera-bench/src/arms/ingest.rs)'s module doc), so build time is a
   property of the policy shape, not of the row count alone. Measured at 2.4M and 25M; ⊘ **the 10⁹
   point must be run, not extrapolated** — a 19× stage under 10× of items forbids the straight
   line. The claim this figure exists to carry is comparative and pipeline-vs-pipeline: one build
   emits the whole serving artifact — geometry, permutation, postings, dictionary, tile structure —
   where the alternative assembles an indexer, a tile generator and ACL wiring as separate builds.
   Its honest unit is *time from raw data to the first correctly-masked map*, and a tuned bulk
   indexer's rows/s may match or beat this build's — so the comparison is measured like-for-like on
   this hardware, or it is not made.
9. **A rotation under load — ops guidance, never a headline.** Scheduled rotation is a maintenance
   window; the unscheduled one is the incident path, and it arrives under whatever load exists.
   Decision [0025](../decisions/0025-rotation-is-a-session-invalidation-event.md) makes a rotation
   a session-invalidation event, so its shape is figure 7's worst case: the whole population rather
   than a minority, and a re-authorisation each — a rotation drops the session, not only its
   projection. One distribution-mode campaign per release, not a per-regression arm: rotate at
   c=N mid-run and report the interval to the population re-established, per class as figure 7
   does. The published form is the operations guide's — "a rotation at c=N clears in X s" — sizing
   the maintenance window and the incident runbook. (Today's rotation is revoke; if roll and
   revoke later split, the figure follows revoke.)

Figure 5 has a standing caveat now that merge is built: **on-disc bytes are 2.0–2.6× the live
working set and only grow** until a fold reclaims, so a scale figure quoted without saying whether a
fold has run is quoting one of two very different numbers.

### 6.1 A concurrency number is not a user count

Figures 3 and 4 are the two most likely to be read as *"the system serves N users"*, and nothing in
the suite today measures a user. `viewport`'s `Pan` and `Zoom` are geometry generators — a box
translated across the extent, and nested boxes about a centre — measured one position at a time with
`metrics::repeat`, which repeats each position **in place**. Every sample is therefore warm on
itself, and the locality a real pan has (warmth from the *adjacent* viewport, not the same one) is
the one thing the measurement removes.

`load`'s closed loop compounds it from the other side: N workers each issuing the next request the
instant the last returns is not N viewers, it is N viewers with no think time, and the gap is about
two orders of magnitude. The open-loop `--rate` mode is the honest one and exists; what does not
exist is a per-user arrival model to set the rate from.

**A `session` arm closes both.** A scripted trajectory — cold first request, then interleaved pans,
zooms and dwells at a realistic zoom distribution, with think time — replayed by N virtual users
open-loop, each starting cold. It reports per-verb latency, and the two session-level figures a
viewer actually feels: **time to first map**, and **p99 within a session** rather than across a
population.

Three consequences worth stating, because each is a convention this document otherwise sets:

- **A step cannot be repeated.** Repeating it destroys the locality being measured, so N comes from
  users × sessions, never from `repeat`. The arm is distribution-mode by construction (spec §4), and
  it is the one arm where min-of-N is not merely uninformative but actively wrong.
- **The cold/warm ratio is a reported axis**, not a constant. It is what decides how much of the
  population meets the F4 projection lock.
- ⊘ **A superseded geometry stamp across a flush, and mid-session filter toggles, are trajectory
  verbs the arm should carry and cannot yet.** The stamp case is write-path §14.18 — pins are
  deleted (decision [0041](../decisions/0041-pins-become-a-staleness-stamp.md)), so what a session
  presents is advisory and never selects geometry; a filter toggle needs the attribute tail.

The zoom distribution and think-time model are **assumed, not measured** — this project has no
telemetry and cannot produce them. State the assumed parameters beside every figure the arm
produces, and treat a change in them as re-baselining rather than as a regression.

**Every one carries its coverage.** A figure taken at 100% coverage measures the absence of masking;
the suite already flags such a cell `degenerate`, but a rendered plot does not inherit a flag.
Whatever renders these must **refuse** a `degenerate` or `generator_bound` cell rather than footnote
it. Appendix C's C4 is the reason this is a design rule and not a presentation preference: coverage
is the axis along which a timing channel is quantified, so publishing latency without it is
publishing half of C4.

---

## 7. Arms

**New.**

- **`soak`** (spec §2.2), now also carrying the fold: a run long enough to contain one is the only
  place the flip's population term (spec §2.3) meets a real session mix. Its fold count is an
  outcome, not a setting (decision 0056), so the arm needs a forced-fold mode to be deterministic.
- **`session`** (spec §6.1) — the only arm that represents a viewer rather than a request. Shares
  `load`'s generator and ceiling calibration; adds the trajectory script, think time and the
  cold-start arc.
- **`residency`** (spec §2.4) — any read arm re-run inside a memory-limited scope, swept over *r*.
  Not a new measurement so much as a new environment for existing ones, so it is a wrapper plus an
  `Env` extension rather than an arm with its own axes. P3 proved the environment works; what is
  missing is the read arms inside it.
- **`filter`** — vocabulary-filter cost against vocabulary size × **principal sparsity** × overlay
  depth, at cold and cached fingerprints. per-point-attributes §3.3 predicts sparse principals are
  cheapest; an arm that does not vary sparsity cannot check the prediction it most needs to. Second
  output: latency against filter selectivity, where I3/I12 give a checkable expectation — a filter
  may move the frontier up, never down.
- **`ingest-wire`** — `load`'s generator pointed at `/control/ingest`, open and closed loop, reusing
  its `/healthz` ceiling calibration and `generator_bound` flag.
- **`rotation`** (spec §6 figure 9) — the load arm's population with a key rotation fired mid-run.
  Shares `load`'s generator and ceiling calibration; distribution mode by construction. A
  per-release campaign rather than a standing arm — the event is rare and its figure is ops
  guidance, so it earns a run when the session machinery moves, not per regression.

**Extended.**

- `gather`, `viewport` — attribute-tail column axis; bytes per served mark split geometry ÷ tail;
  varying the **number** of category columns, not only their width.
- `ingest-build` — an attribute-column stage in the eleven-stage decomposition; a verify-open
  close, so figure 8 ends at a bundle demonstrated queryable rather than merely written; and the
  10⁹ cell, which figure 8 needs and extrapolation cannot supply.
- `ingest-rate` — **exists** (density × `B/W` × submitters), and is the arm to quote a throughput
  from. What it lacks is the wire: it drives `accept_ingest`, not `/control/ingest`.
- `ingest-batch` — a ramp, not a rate; keep it and stop reading throughput off it (spec §3).
- `ingest-continuous` — re-derived against a flushing engine, per spec §3.

**Infrastructure**, and the cheapest items here.

- **A `variant` field on `Record`**, so two implementations write into one run directory and
  `bench_collate.py` pairs cells. Today an A/B is two run directories and a hand diff.
  hot-row-geometry §7 hit this exactly — *"measuring this shape against the previous one needs a
  second column source and a build able to emit both"*. Generalise once rather than per comparison.
- **A fixture attribute-tail knob** in `bench_build_fixtures.sh` (width and type mix, declared and
  discovered). per-point-attributes §8 opens by saying no arm can see any of this without it; it
  blocks the whole attributes half.

---

## 8. Gates

G0–G3 are unchanged. Two are added, and both are **soak-only** — per-PR cannot afford hours, so
they belong to conformance §6's nightly tier, which does not exist (⊘).

| gate | checks | on failure |
|---|---|---|
| **G4** | soak p99 shows no monotonic drift across the window beyond +15% | steady-state regression — the class a frozen-bundle suite structurally cannot catch |
| **G5** | segment, delta-tier, external-id-run and dictionary-extent counts, and ack→visible, within configured bounds | write-path §14.10/§14.11 violated |
| **G6** | on-disc bytes fall at a fold, and the flip's missed-session latency stays within its measured band | the fold reclaimed nothing, or its aftermath became a refusal rather than a miss (decision 0053) |

G5 and G6 are assertions about the system, not about its speed, and they fail the run rather than
reporting a regression. G6 needs the soak arm's forced-fold mode: on a gated-window schedule
(decision 0056) a run can legitimately contain no fold, and a gate that passes because nothing
happened is worse than no gate.

---

## 9. Sequencing

The `variant` field, the load arm's `work` block and the `p99` suppression are unblocked now, and
the first two block comparisons the other work will want. The fixture knob blocks every attributes
arm. The soak harness was always independent of flush, and its gates no longer wait on anything but
the arm itself (spec §2.2).

The state axis is expensive in wall clock, not in cell count — one soak cell is hours. It is
declared outside `matrix.toml`, as `load` and `ingest-build` already are, for the reason that file
gives: putting an hours-long cell beside a two-microsecond one makes the default run useless.

---

## 10. Amendments

- **`bench/README.md` §8** — the two reporting modes beside the existing `min_ns` rationale, and the
  `p99` suppression rule; the §9 caveat *"Ingested rows never become visible"* is false as of flush
  and retires, and the arms it governs are re-derived rather than edited.
- **`bench/README.md` §7** — G4 and G5 in the gate table, marked nightly-tier.
- **`bench/matrix.toml`** — the attribute-tail axis on `gather` and `viewport`; `filter` declared;
  `soak` and `ingest-wire` noted as deliberately absent, with the reason.
- **`report.rs`** — `SCHEMA_VERSION` to 2 and the spec §2.1 fields.
- **`conformance.md` §6** — the nightly tier gains G4/G5 when it gains anything at all.

---

## 11. What this does not do

- **No new corpus.** Every axis here is constructible within the existing fixtures plus an attribute
  tail. Signature-sorted contiguity still cannot be synthesised and still costs a rebuild.
- **No residency figure for the read arms.** P3 measured what a concurrent streaming read costs a
  viewport (spec §2.4); nothing sweeps *r* across `viewport`, `gather` or `session`, so
  hot-row-geometry §7 stands unchanged and the 18 → 12 B/row saving stays arithmetic against
  Appendix A.
- **No claim that the residency sweep is representative.** A cgroup limit reclaims by the kernel's
  LRU, not by a deployment's access pattern, and the ratio *r* is chosen rather than observed. It
  establishes a curve's **shape**; it does not predict a given deployment's point on it.
- **No published figure for anything unbuilt.** ⊘ Of the nine spec §6 figures, four now have
  measurements behind them — ack→visible's three intervals and the scale figure from `scale.rs`,
  the ingest rate from `ingest-rate`, and the build figure at 2.4M and 25M — but none over the
  wire, and none under a session mix. The soak curve needs the soak arm; the flip figure needs it
  too, with a forced fold; the attribute split needs the tail; the wire ingest number needs
  `ingest-wire`; the 10⁹ build needs its run; the rotation figure needs its campaign; and the build
  figure's like-for-like comparison has never been run.
- **No fold throttle, and no figure that implies one.** Decision 0052 refuted the mechanism; P3's
  rate arms measure something that cannot be set. The harm they establish is real, the lever is not.
- **No replacement for the correctness gate.** A soak that stays fast while leaking passes every
  gate here. `conformance.md` owns that, and this document does not weaken the split.

---

## Appendix R — review record

**Updated 2026-08-04 — restated against a built write path.** Flush and both halves of merge exist,
so spec §2.2's marker is the arm's absence rather than flush's, spec §3's and spec §10's caveat is
retired rather than pending, and spec §11's unpublishable list names the missing *measurement*. Two
figures changed class: P2 measured the fragment build at ~200 ms and **flat in tier count**, which
refutes the tier-count-drives-latency expectation spec §2 carried, and §14.10's bound is now
asserted by an engine test rather than only by a soak. No convention changed.

Not yet reviewed. Drafted 2026-08-02 from the benchmark obligations recorded in
`write-path.md` §14, `per-point-attributes.md` §8 and `hot-row-geometry.md` §7, plus two
findings from reading the harness: `p99_ns` is identically `max_ns` at the matrix's own repetition
counts (spec §4), and the load arm emits a stubbed `work` block despite already decoding the
columns that would fill it (spec §5.1).

Spec §6.1 was added after drafting, on the observation that `Pan` and `Zoom` repeat each position in
place — so the sequential locality a real pan has is precisely what the repetition removes, and no
arm represented a viewer rather than a request.

Spec §2.4 was added on the objection that cold-page effects cannot be measured naively, which is
correct: the three mechanisms a reader would reach for first each fail for a different reason.
`MemoryMax` under an unprivileged `systemd-run --user --scope` was verified to apply on this box
before being specified.

**Revised 2026-08-07 against the completed ingest pipeline.** Flush, merge and the fold are built,
and the measurements that followed them overturned four claims this document carried: that residency
had never been measured and could not outgrow this hardware's RAM (P3 did both — spec §2.4); that
ingest throughput is an fsync amortisation story (fsync is 11–24%, `apply_window` 38–50%); that
concurrent writers are the axis a published ingest rate should be plotted against (they are not a
multiplier on the bulk-loader figure); and that ack→visible is one number (it is three). Spec §2.3
is new — the fold is a third publication kind whose cost is per resident session rather than per
byte, and the three rulings that shape what an arm may assert about it (decisions 0052, 0053, 0056)
all post-date the original draft.

**Extended 2026-08-07, from the audience review** (developers, deployments, publication). Two
figures added to spec §6. Build wall-clock: the arm and its stage decomposition already existed and
carried the finding — the signature sort is superlinear — but no published figure was defined over
it; the 10⁹ point is a run, not an extrapolation, and the comparative claim waits on a
like-for-like pipeline measurement. Rotation under load: rare and mostly scheduled, but the
unscheduled rotation is the incident path and cannot choose its load — published as operations
guidance, never as a headline. The §6 count read "Six" while listing seven; it now says nine and
matches.
