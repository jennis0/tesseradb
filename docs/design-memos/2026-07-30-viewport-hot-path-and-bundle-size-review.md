# Viewport hot-path and bundle-size review

*2026-07-30. Status: complete — analysis and 10⁹ measurements. Every figure is either
measured in this campaign (raw data in §1.4), established by code inspection (file
references given), or cited from the design corpus / previously committed measurements.*

**Findings in one paragraph.** The rebuilt 10⁹ bundle (47.02 GB, built in 2 h 37 m at
27.5 GB peak RSS) serves a representative viewport in **135–164 ms**, of which **83–89% is
the §7.2 selection scan** at **4.0–4.5 ns per visible row** — request latency correlates
with Σvisible at **0.999**. That cost is the recorded price of the density-correct
selection that replaced the placeholder sampler one day after the old baseline was
committed; the old baseline no longer measures this engine. The per-row cost is decoder
and heap machinery, **not** column bytes (the id read is ≤10% of it), which reorders the
candidate list: the batch/run-decode rework (B9) and per-session tile memoisation (B10)
are the high-yield low-risk moves; the sub-Σvisible evaluation route (B1) is the
asymptotic lever worth a design memo; and the priority-column prefix scan (B2) is
**declined by measurement** — r22's trigger has not fired, which also strengthens the case
for dropping the 2 GB priority column (D3). On disk, position storage should be decoupled
from the sort key (D2b: u16 sub-cell refinement, −4 to −8 GB, more precision than f32),
the external-ID family stays as a deliberate realism cost (D1), and the extent clamp needs
build-time validation (D6). Two capability sketches — client-cut delta streaming (B11) and
the occlusion drill-down (F1) — share one request primitive and should be specified
together.

This memo answers two questions posed by the owner: how does the system actually perform
against the 10⁹ corpus as built today, and what are the available optimisations of the
viewport serving hot path — algorithms, caches, data structures, memory — presented with
enough detail to decide on each. Threading and parallelism are **out of scope** (owned by a
separate workstream); findings that belong to that workstream are recorded in §5 and not
acted on. A bundle-size review (§3) and two capability sketches that emerged from the
discussion (§4) are included. **No hot-path code was changed in producing this memo.**

Candidate labels (B1–B11, D1–D6, F1) are stable and referenced from the discussion that
produced them.

---

## 1. The benchmark context

### 1.1 The baseline that no longer measures this engine

The committed baseline (`docs/superpowers/plans/bench-baselines/2026-07-29-1e9-k-sweep.json`)
predates, by one day, essentially the entire selection subsystem it would now be compared
against. Landed **after** it was recorded: the real §7.2 selection (floor ∪ threshold ∪ cap,
263ba72), the density underlay and `served` column (84c840d), the k-default and cap change
(`k_max_marks` 128→500, `max_k` 200→1000, 1bd8cfc), the selection trim (d61b74d), and the
`tile_ranges` galloping sweep (7abc249).

The consequence is qualitative, not incremental. The 07-29 engine's sampler was the
placeholder first-k: it **stopped reading after k rows per tile**. The current engine's
direct-evaluation route — deliberately, per I7 and §7.2 — reads **every visible row in every
non-empty tile** to obtain the exact threshold count `C_θ`. The old baseline's per-row cost
(0.46–1.29 µs per 1,000 rows visible, from `probes/work-correlation.json`) therefore
measures an algorithm that no longer exists. Today's run establishes a **new baseline**, and
where it is slower than 07-29 at high-visibility viewports, that is the recorded price of
density-correct selection — the price §7.2 knowingly accepted and the candidates in §2
exist to buy back.

For scale, the workload facts that survive from the 07-29 measurements (same corpus, same
box): a random ~300-tile viewport at 10⁹ has **mean Σvisible ≈ 25.4M rows, max ≈ 144.8M**
(w=10⁴ grant, 41.6% coverage), and server latency correlates with Σvisible at **+0.83**
(k=50). Those numbers are what make the O(Σvisible) pass the headline target.

### 1.2 Measurement setup

- Bundle rebuilt from `data/scaled/geometry.parquet` (6.6 GB, 10⁹ rows) +
  `categories-subclass` pairs (1.6 GB), identity key `000102…0e0f`, epoch 1 — the fixed
  bench-fixture key, so future fixture comparisons are like-for-like.
- Box: WSL2, 12 cores, 47 GB RAM, ext4 (`/dev/sdd`). Same box as every prior figure; the
  standing instruction from `probes/results.md` §1 applies — *treat ratios as evidence and
  absolutes as a starting point*.
- Headline latency from the **uninstrumented** release binary over HTTP
  (`scripts/bench_k_sweep.py`, `bench_fixed_viewport.py`, `bench_work_correlation.py`);
  stage attribution separately from a `bench-timing` build (the probe is not free) via the
  22-field `x-tessera-stage-ns` header and the in-process `tessera-bench` viewport arm.
- **Deviation from plan:** no sampling profiler exists on this box (no `perf`, no
  passwordless sudo). Attribution rests on the stage probes — which split the request into
  the exact units the candidates target (`count_ns` / `select_ns` / `gather_ns` /
  `arrow_serialise_ns` / `tile_ranges_ns`) — plus the in-tree micro-benches
  (`crates/tessera-engine/examples/route_saving.rs`, criterion). Hardware-counter claims
  (cache-miss rates) are therefore modelled, not measured, and are flagged as such.

### 1.3 Results

`[PENDING — filled from the run directory when the 1e9 build completes]`

| | |
|---|---|
| Build wall time / peak RSS (`/usr/bin/time -v`) | **2 h 36 m 39 s / 27.5 GB** (first recorded figures; streaming pipeline, 12-core WSL2 box) |
| Bundle size on disk | **47,017,099,359 B = 47.02 GB** — §3.1's projection was exact |
| Boot to ready | **31.6 s** (07-29 bundle: 110.2 s — external-ID digests now deferred per §0.3 dev 9, page cache warm from verify) |
| Authorise, w=10⁴ | **262 ms** (07-29: 264 ms — unchanged, as expected: authorise never touches selection) |
| Warm-up viewport (row-projection build) | **10.7 s** (07-29: 9.5 s) |
| k-sweep, n=500/k, server p50/p99/max (ms) | k=50: **8.6 / 561 / 592** · k=1000: **19.3 / 707 / 770** · k=2500: **20.7 / 709 / 747** · k=5000: **20.4 / 764 / 777**. Points mean **4,381** for every k ≥ 500 (θ-driven, cap-clamped at 500); response mean **79 KB** (07-29: 1.1–7.7 MB) |
| Fixed viewport ×500 (zoom 6, representative), server p50/p99 (ms) | k=50: **135.1 / 158.0** · k=1000: **163.6 / 191.3**. Same geometry under the 07-29 placeholder: 9.9/15.1 and 29.2/42.2 — the real §7.2 selection costs **13.6× / 5.6×** on identical work, ≈ **4–5 ns per visible row**. This is the pool B1/B2/B9 drain. *(Owner note: **k=500 is the deployment operating point** — cap = min(k, k_max_marks=500) and k defaults to the cap. The 50/1000 pair brackets it; k=500 is added to the fixed-viewport instrument for all subsequent measurement, and the legacy criterion k=30 group is retained only for baseline comparability.)* |
| Work correlation, 800 random viewports | **corr(server_us, Σvisible) = 0.999 (k=50) / 0.994 (k=1000)** — up from 0.83/0.33 under the placeholder; per-row constant **4.0–4.5 ns/visible row**; points-returned and response-bytes uncorrelated (−0.08/−0.11). Request cost at 10⁹ *is* the visible-row scan. |
| Stage shares (tessera-bench viewport arm, battery+pan, 44 cells) | **select 83.2–88.8%** · gather 5.1–6.5% · count 3.4–5.8% · tile_ranges 2.6–4.4% · compose ≈0% — stable across modes and k. **B1/B2/B9's target is ~87% of the request**; B5/B6's targets are confirmed negligible. |
| ns per row visited | **4.0–4.5 ns** (work-correlation slope; corroborated by fixed-viewport arithmetic) |
| Gather arm (full-scan, warm): per-visible-row cost | xy 0.6–1.7 ns · xy+id 1.0–2.1 ns · +priority 1.1–2.3 ns across 1k–1M-row ranges at 6.2% coverage. **Marginal cost of the 8-byte id read: ~0.2–0.4 ns/row** — ≤10% of the engine's 4–4.5 ns/row total. The per-row cost is decoder/heap machinery, not column bytes. |

### 1.3b Label-set × grant-width campaign (2026-07-31, post-B9)

With the reworked build pipeline (main 8c671f1/659452d: **~7 min per 1e9 bundle**,
~28 GB RSS — vs 2 h 37 m the day before), 1e9 bundles were cycled per label set
(build → verify → A/B → delete; external IDs minted for the categories sets, omitted for
surnames on disk grounds — never on the serving path). A/B = main (659452d) vs
`perf/b9-run-decode`, fixed viewport, server p50 at the cap-500 operating point:

| Label set | w=100 | w=1,000 | w=10,000 |
|---|---|---|---|
| categories-subclass (07-30 campaign) | — | — | 163.6 → 123.3 ms (**−25%**) |
| categories-archive (max contiguity) | 10.4 → 7.6 (−26%) | 79.1 → 60.8 (−23%) | 262.5 → 181.9 (**−31%**) |
| hash-flat (scattered control) | 39.9 → 28.5 (−29%) | 76.8 → 69.5 (−9.5%) | 357.7 → 120.2 (**−66%**) |

Reading: the win holds at every density measured — floor −9.5% (hash-flat w=1k, ~10%
coverage, array containers), ceiling −66% (hash-flat full-vocabulary grant, ~63% uniform
coverage → dense bitmap containers, `next_many`'s best case and the retired per-value
iterator's worst). No regression anywhere. Raw data:
`probes/2026-07-31-label-campaign/`.

**Surnames at 1e9: blocked by a build-pipeline limitation** (finding for the build
workstream): the reworked pipeline OOM-killed at **47.6 GB RSS after 3 h 11 m** on the
116.9M-term / 4.37B-pair set — build memory evidently scales with term cardinality, which
the 48k-term sets never exposed (they build in ~7 min at 28 GB). The serving-side
questions surnames would have probed (dictionary scale, ultra-sparse masks) are largely
covered elsewhere: Phase 0 measured authorise-path dictionary scale as free, and the
w=100 columns above cover near-sparse masks. A 25M-scale surnames A/B remains available
cheaply if wanted.

### 1.4 Raw data

`probes/2026-07-30-1e9-rebuild/`: `k-sweep-1e9.json`, `fixed-viewport-1e9.json`,
`work-correlation-1e9.json`, `bench-1e9-viewport/` (44 cells, per-stage ns),
`bench-1e9-gather/` (36 cells), `build-time-rss.txt`. Bundle at `/tmp/tessera-1e9`
(fixture symlink `/tmp/tessera-bench/fixtures/1000000000/categories-subclass`).
Not run: `bench_collate.py --gates` (no prior 1e9 tessera-bench baseline exists to gate
against — these run dirs *are* the baseline for the next campaign), the tiles/authorise
arms (authorise measured via the scripts at 262 ms), and criterion at 2.4M (fixtures were
wiped with /tmp; rebuild via `scripts/bench_build_fixtures.sh` when next needed).

---

## 2. Hot-path candidates

Ordering within this section is by expected impact. Each entry states the proposal, where
the cost lives today, the expected win and how it was derived, the trade-offs — including
every known interaction with the invariants, the contracts spec, and the leak register —
and what adoption would require. "Adoption requires" is deliberately explicit: several of
these are one-afternoon changes; several are owner decisions with design-memo obligations;
one (B2) engages a decision the design has already made once.

### B1 — Exact sub-Σvisible C_θ via the storage order

**POST-CAMPAIGN VERDICT — refuted by measurement of its premise.** The cell-occupancy
scan (tool: `crates/tessera-store/examples/cell_histogram.rs` on branch
`probe/cell-occupancy`; data: `probes/2026-07-30-1e9-rebuild/cell-histogram-1e9-subclass.json`)
shows the corpus stores **1.55 rows per occupied cell globally and 1.40 in the reference
viewport** (62.9M cells / 87.9M rows; max cell 186; 0.2% of rows in cells ≥ 32), against a
per-cell break-even of 25–50 rows. The quantisation grid is fine enough that clustering
spreads points across more cells rather than deeper ones; the per-cell search route would
regress ~15× where it was meant to win. Cell structure is label-set-independent (the
morton column is shared), so this verdict covers every label set. What survives: a
**single-cell-tile fast path** — the global tail holds ~5% of rows in cells ≥ 1024 (max
3.88M), and a tile whose whole range lies inside one such cell is fully id-sorted, so
C_θ is one binary search and the served set a prefix; the gate is two loads
(`morton[start] == morton[end−1]`). Candidate fourth tier for the B9 framework; costs
nothing where it does not fire. **Perf-ledger item, explicitly not in the stage-2.1 plan**
(2026-07-31, Track A Task 2): it is a fourth decode tier over the direct route — the
identical served set from the identical mask, not a candidate list — and it is unscheduled
alongside B10 and B3/B4 rather than folded into a stage whose subject is the write path.
The section below is retained as the design record.

**Proposal.** Evaluate the threshold count `C_θ(T)` — "how many visible rows in tile T have
`tessera_id` below the cut `P_d`" — without reading every visible row. Storage order is
`(morton, tessera_id)` (`crates/tessera-spatial/src/tiler.rs`), so within a single leaf
Morton cell the identity column is **sorted**: rows below the cut form a prefix of the
cell's row range. One binary search per occupied cell finds the prefix boundary; the
visible count below the cut is then `mask.count_range(cell_start..boundary)` — an
O(containers-touched) bitmap operation, not an O(rows) scan. A tile at depth d is a
concatenation of its occupied leaf cells' runs, so `C_θ(T)` is the sum over runs. The
selection of the m served rows is restricted the same way: only runs' prefix regions can
contain servable candidates, so the heap pass walks candidate regions rather than the whole
tile. The code already records this route and its premise —
`crates/tessera-engine/src/select.rs:146-154` — and §7.2 records the trigger for revisiting.

**Where the cost lives today.** `Selection::of` (`select.rs:224-321`) iterates every
visible row of every non-empty tile (bitmap decode), loads its 8-byte id from the mmap'd
column, tests it against the cut, and maintains a peek-reject heap of the `cap` smallest.
**Measured: this stage is 83–89% of request time at 10⁹** (44 bench cells, stable across
battery/pan modes and k=50/1000), at **4.0–4.5 ns per visible row**, and request latency
correlates with Σvisible at **0.999** — the request cost *is* this pass. On the
representative fixed viewport that is 135–164 ms per request.

**Expected win.** Replaces O(Σvisible) id reads with O(runs × log(run length)) searches +
O(runs) bitmap range-cardinalities, *in dense tiles*. Density decides everything: at 10⁹
over 2³² Morton codes the mean occupancy is 0.23 rows/code, so in sparse regions runs
degenerate to a handful of rows and the plain scan is faster; in the dense clusters — where
Σvisible actually accumulates — runs reach thousands of rows and the route wins by orders
of magnitude. With the measured 4–4.5 ns/row scan constant against a ~100–300 ns
warm-cache search, **the warm crossover sits near run length ≈ 25–75 rows** — most of a
clustered corpus's Σvisible lies in runs far longer than that, so the dense-viewport
ceiling is a 5–20× reduction of the 87% pool. The cold-cache caveat below still applies.

**Page-miss economics** (the owner asked): a binary search over a run spanning P pages
costs ~log₂(P) *dependent* probes — the last ~9 halvings land inside one 4 KB page (512
u64 ids) and are nearly free. A sequential scan touches all P pages but rides hardware
prefetch and OS readahead. Modelled crossovers on this box: L2-resident runs ~50–200 rows;
DRAM-resident ~1–3k rows (~100 ns per dependent miss); **page-cache-cold inverts the
result** — random probes get no readahead (~50–100 µs each from disk) while sequential
faulting batches ~16k rows per I/O, so cold favours the scan until runs span dozens of
pages. With the columns file at 18.5 GB against 47 GB RAM shared with morton, masks and
postings, residency is contested (the design's Appendix A residency correction is exactly
this concern) — which is why §3's reductions (D2b, D3) materially improve B1's odds: they
shrink the hot set toward fully-resident.

**Construction requirements.**
- *Monotone gallop, never independent searches.* Searches proceed left-to-right across a
  tile's runs, so each search brackets from the previous boundary (the `tile_ranges_all`
  trick, `crates/tessera-store/src/read.rs:1063-1082`). Short runs then degrade to
  near-sequential page order (readahead-friendly); long runs skip pages. The catastrophic
  pattern — independent full-range cold probes — never occurs.
- *Hybrid threshold.* B9's run decoding (below) yields the visible rows as contiguous
  ranges; per run, **scan if short, search if long**, threshold set from the measured
  crossover, with the scan as the always-correct fallback. This also preserves behaviour on
  sparse corpora automatically.
- *Run enumeration is the real cost centre*, not the searches. Occupied-run boundaries come
  from galloping the morton column within the tile range. The corpus's run-length
  distribution (not extracted in this campaign — it needs a small offline scan of the
  morton column) decides whether enumeration is cheap enough inline or wants a build-time
  run index (a contracts-level addition; deliberately NOT proposed here). That scan is the
  first task of any B1 implementation plan.

**Trade-offs.**
- *Reviewability.* This adds genuinely subtle code to the most invariant-sensitive loop in
  the system. CLAUDE.md's rule — an optimisation that costs reviewability needs an
  argument, not just a benchmark — applies at full strength. The argument would be: the
  route is *semantically invisible* (identical `Selection` output, bit for bit; the
  differential oracle and the §7.2 property tests pin it), the fallback is the current
  code, and the win lands precisely where the current cost concentrates.
- *Timing channel (C19).* A data-dependent fast path widens per-tile timing variance —
  response time now leaks run-structure, not just cardinality. Appendix C's C19 already
  covers the per-tile selection route in kind; the quantified widening should be measured
  (the coverage-sweep bench arm is the C4 instrument) and the register entry updated.
- *I7 is untouched* — sampling still happens after masking, on the same masked rows; only
  the evaluation order changes. But the review must check this claim, not assume it.
- *Effort.* Days, not hours: implementation + equivalence tests (positional-equivalence
  pattern from `bundle_read.rs:474-594`) + oracle runs + the design memo §7.2 asks for.

**Adoption requires.** A short design memo arguing the route against §7.2's recorded
trigger; independent review of the plan before code (the working method); no contract or
bundle change in the inline-enumeration form.

### B2 — Priority-column prefix scan

**Proposal.** During the C_θ pass, test the 2-byte `priority` column (the leading 16 bits
of `tessera_id`, one and only definition `crates/tessera-types/src/identity.rs:104-106`)
against the high 16 bits of the cut, falling through to the full 8-byte id only when the
prefixes tie. The scanned column on the dominant pass narrows from 8 B/row to 2 B/row.

**Where the cost lives today.** The same `Selection::of` pass as B1. The design has already
quantified this exact choice: the implemented comparator reads the full id, so "the column
a viewport scans under direct evaluation goes from 2 GB to 8 GB at 10⁹" (architecture
design r22, Appendix A). The `priority` column is **written and unread at query time** —
verified by grep: the only reader in the repo is the bench gather arm.

**Measured result — the trigger has not fired.** The gather arm puts the marginal cost of
the 8-byte id read at **0.2–0.4 ns per visible row** (full-scan pattern, warm cache, 6.2%
coverage) against the engine's 4.0–4.5 ns/row total: **the raw column read is ≤10% of the
selection cost**. The other ~90% is bitmap decode, threshold/branch, and heap machinery —
which a narrower column does not touch. B2's warm-path upper bound is therefore a
single-digit-percent improvement, and r22's decision to prefer the obviously-correct
comparator stands *confirmed by measurement* on latency grounds. The one ground this does
not settle is **residency under memory pressure** (the design's 4× page-traffic figure is
about cold/contested page cache, not warm scans); but the D-family reductions (§3.7)
shrink the hot set to ~12 GB and largely dissolve that pressure on this class of box.
Fall-through volume, for completeness, is ≈ V/2¹⁶ (prefix ties) — negligible.

**Trade-offs.**
- *This decision has been made once already, the other way.* r22 explicitly declined the
  prefix-scan path — "the construction that is obviously correct is preferred to the one
  that is fast" — while deliberately keeping the column on disk so the choice stays
  **reversible without a bundle rebuild**, and recording the revisit trigger
  (w ≈ log₂(V_max/k)). This memo does not relitigate that; it supplies what the trigger
  asks for: measured evidence of what the choice costs at 10⁹. If the measured id-load
  share is small, r22 stands confirmed and B2 dies; if it is the bulk of `select_ns`, the
  trigger has fired and the owner re-decides with numbers.
- *Two streams instead of one.* The pass touches priority (2 B) always and tessera_id
  (8 B) on ties and for heap candidates — two page working-sets, though the second is
  touched sparsely. Net page traffic still falls ~4× in the counting-dominated case.
- *Tie-boundary correctness.* The fall-through comparator must be exactly "the k lowest by
  tessera_id" — the design notes there is no composite comparator to get wrong *because*
  the prefix is a prefix, but the implementation still owes a property test.
- *Relation to B1/B9.* B1 subsumes B2 on runs it takes the search route for; B2 still
  covers the scan-route runs, and composes with B9's contiguous-slice scanning (a 2-byte
  sequential compare-and-count vectorises better than an 8-byte one).
- *Storage coupling (D3).* If B2 is rejected permanently, the 2 GB column is dead weight
  and §3/D3 argues for dropping it — one decision should settle both.

**Adoption requires.** Owner decision against r22's standing instruction, informed by the
`[PENDING]` figures; property test for the fall-through; no format change (the column is
already there — that was r22's point).

### B9 — Batch and run decoding of visible rows in `Selection::of`

**Proposal.** Replace the per-tile bitmap materialisation + per-value iteration with
croaring 2.7's cursor API. Today (`compose.rs:145-154`, `select.rs:242,263,290`):
`rows_in_range` builds `Bitmap::from_range(r)`, ANDs it with base, applies the overlay
diffs — three to four temporary bitmaps allocated per non-empty tile — and the selection
then decodes it one value at a time. croaring exposes `BitmapCursor::reset_at_or_after`
(seek), `next_many` (batch decode into a caller buffer), and `read_many_ranges` (decode
**as contiguous ranges**). Two independent improvements:

1. *Steady state, zero allocation:* when the overlay diffs are empty (`minus`/`plus` empty
   — the common case between change events), iterate `base` directly:
   `cursor.reset_at_or_after(range.start)`, `next_many` into one reused buffer, stop at
   `range.end`. No `from_range`, no AND, no temporaries — per-tile allocations drop from
   3–4 bitmaps + 2 vecs to zero.
2. *Run decoding:* `read_many_ranges` returns the visible rows as row-ranges. Dense mask
   regions (run/full containers — the norm under high-coverage grants at 10⁹) come out as
   a few ranges covering thousands of rows. The C_θ test then runs over **contiguous
   slices of the id column** — sequential, prefetched, auto-vectorisable — instead of a
   strided per-bit gather. This is also exactly the shape B1's hybrid needs (runs to
   scan-or-search) and doubles B2's effect (sequential u16 compare-count).

**BUILT AND VALIDATED (branch `perf/b9-run-decode`, two commits).** The story has a
lesson in it. The first form (run decode as the sole route) was **refuted by
measurement**: +7.8% at 2.4M — mask-run length is a property of the *grant*, and a
42%-coverage mask over morton-ordered rows has ~1.7-row runs, too short to amortise
per-run machinery. The landed form is a **three-tier adaptive decode**, gated per tile on
`(visible, range_len)` — both already in hand and both §7.1-disclosed:
tier 0 `visible == range_len` → the whole range is one id-slice, no decode at all (the
signature-partition / saturated-grant endgame); tier 1 density ≥ **95%** (measured
crossover ∈ (0.90, 0.95) at cap 500, constant carries its evidence in the comment) → run
decode; tier 2 default → `next_many` value-batch, which beats the retired per-value path
at **every** density (12.8 vs 14.6 ns/row even at 30%).
**Measured end-to-end**: 2.4M criterion −3% (both k30 and k500 groups); **1e9 fixed
viewport 163.6 → 123.3 ms server p50 at the k=500 operating point (−25%)**, quiet-box,
mins shifting identically (156 → 120 ms). Output bit-identical (property tests with
per-tier route counters; differential oracle passed twice). Landing awaits the owner's
C19 register note: *the decode tier is a function of (visible, range.len()) only — both
already disclosed — so the tier-choice timing channel reveals nothing a viewer does not
hold; the diffs-empty/diffs-present route choice remains the C19-adjacent residual.*

**Trade-offs.**
- Two decode routes (diffs-empty vs diffs-present) in an audited loop. The route choice is
  driven by `minus.is_empty() && plus.is_empty()` — observable in timing (C19-adjacent,
  same note as B1) but not in output. The diffs-present fallback is the current code,
  unchanged.
- The reused decode buffer is per-request state — plumbing, not design.
- Equivalence tests owed (identical `Selection` across routes over the property-test
  corpus and the differential oracle).

**Adoption requires.** No owner decision — this is an implementation-quality change inside
the existing semantics. Plan review + tests per the working method.

### B10 — Per-session tile memoisation

**Proposal.** For a fixed (session, generation — including overlay version, zoom,
`SelectParams`), a tile's `(visible, C_θ, served rows)` is a pure function. Pan workloads
re-request mostly-overlapping tile sets; a small per-session LRU keyed
`(generation identity, overlay_version, zoom, params, tile)` returns the previous result
and skips counting, selection and (optionally) gather for revisited tiles.

**Where the cost lives today.** Every request recomputes every tile from scratch; two
consecutive viewports one pan-step apart share ~80–90% of their tiles.

**Expected win.** Hit-rate × per-tile cost. Measured per-tile cost on dense viewports:
**~405–450 µs/tile** (work-correlation, µs_per_tile over ~264-tile requests). A one-step
pan on a 17×17 tile grid shares 16/17 of its columns (~94%); realistic drag sequences
overlap 80–94%. At those rates the memo saves the full count+select+gather for the
overlapped tiles — **5–10× on pan-step latency** for dense viewports (e.g. 135 ms →
~15–30 ms), which no other candidate reaches without touching the scan itself. The
standard benchmarks are structurally blind to it: the random sweep never revisits a tile
(win invisible), a fixed-viewport repeat is a 100% hit (win overstated) — a pan-trace
measurement is the honest instrument for the realised hit rate. Real interactive sessions
are pan-dominated, so this is likely the largest *practical* win per unit of risk after
B9.

**Trade-offs.**
- *The two safety edges are absolute.* (1) **Never shared across sessions** — two sessions'
  masks differ; a shared entry is a disclosure across the trust boundary, not a bug but a
  breach. Key by `token_id` exactly as the existing row-projection cache does
  (`viewport.rs:383-407`). (2) **Overlay version in the key** — a suppression must be
  reflected by the very next request (lifecycle §2.3; pins fix geometry, never
  authorisation). Keying on the generation's overlay version makes staleness structurally
  impossible rather than policed.
- *Memory.* Bounded LRU per session (result rows are small — `served ≤ cap` row ids +
  two counts); the existing caches' no-eviction posture (§5) should not be copied.
- *Serves stale θ?* No: θ's anchor is generation-scoped and in the key via generation
  identity; an overlay swap moves the anchor and invalidates, which is the accepted §7.2
  behaviour for swaps.
- *Consistency.* Cache must store the *selection output*, never the gathered payload, if
  scalars can change per generation; storing row ids only keeps it trivially correct.

**Adoption requires.** No spec change (response bytes identical to recomputation — that is
the definition of the cache being correct); plan review with the two keying edges as
explicit review items; an eviction policy, which the neighbouring caches currently lack.

### B11 — Client-supplied per-tile cuts (delta streaming)

**Proposal.** The served set is an id-prefix per tile, so a client that already holds a
tile's marks can describe them exactly: "all served ids ≤ X". A request field carrying
per-tile cuts (plus the overlay version its state was built at) lets the server skip
serving rows the client holds: rows with id ≤ cut are still **counted** (all disclosed
counts unchanged) but not heaped, gathered, serialised or sent.

**Flows it accelerates.** Pan overlap (~80–90% of tiles resent today); zoom-in (nesting —
k non-decreasing on descent, contracts §3 — makes the parent's marks a prefix of each
child's served set, so the client derives child cuts locally); k-increase (progressive
render: k=50 first paint, then k=500 sends only the delta — a deliberate UX pattern).

**Expected win.** Eliminates most of `gather_ns + assembly + arrow_serialise_ns` and most
response bytes on revisit-flows. Measured under today's θ=16 policy those pools are small
(gather 5–7%, responses 79–108 KB), so B11's value is **policy-dependent**: it grows
directly with the mark budget (a θ/k configuration that serves the 07-29-scale 1–8 MB
responses is where it pays), and its progressive-render flow (k=50 first paint → k=500
delta) is a UX capability no server-side candidate provides. Does **not** touch the count pass —
complements, never replaces, B1/B9. Equivalent to B10 with the memo moved to the client,
where the state already lives: no server memory, no eviction, at the price of still paying
count+select server-side.

**Trade-offs.**
- *Deny convergence is the sharp edge.* Today a re-pan replaces tile contents, so a
  freshly-suppressed item vanishes on the next request. A merging client would keep it
  forever. Fail-closed shape: cuts are honoured **only** when the request's overlay
  version matches the generation's; on mismatch the server ignores the cuts and answers
  full. Overlay swaps are rare against pans, so the value survives.
- *Leak direction is benign but must be argued, not asserted:* a delta response is a strict
  subset of the full response for the same request; the client-supplied cut is an id the
  client was already served (I10 unaffected — ids are the wire identity in Phase 1). A
  dishonest cut starves only the dishonest client. Still: new request field ⇒ leak-register
  entry, oracle models the parameter, conformance canaries extended.
- *Spec surface.* This is a contracts change (§3 viewport request; a client obligation
  — merge only inside the version gate — of the same kind as the existing k-nesting
  obligation). Cheap in code; real in specification and audit surface.

**Adoption requires.** Contracts spec revision + leak-register entry + oracle/conformance
extension; server implementation is small (one comparison in the selection emit path + the
version gate). Shares its request-primitive design with F1 (§4) — designing them together
halves the spec cost.

### B3 — Per-request scalar resolution and SoA gather

**Proposal.** Hoist per-row work out of `row_to_point` (`viewport.rs:608-635`). Today,
**per point**: one `HashMap<String, usize>` lookup **per declared scalar** (string-hash on
a name that is constant for the whole request), one `Vec<ScalarOut>` allocation, and for
Utf8 scalars an owned `String` copy. Resolve the `ScalarSlice`s once per request before the
tile loop; emit columnar output (structure-of-arrays) — `ids: Vec<u64>, xs: Vec<f32>,
ys: Vec<f32>` + per-scalar columns — instead of `Vec<PointOut>`.

**Where the cost lives.** `gather_ns`, and it scales with points served: 07-29 responses
carried 12k–520k points (k=50→5000). At 143k points (k=1000) with s scalars this is 143k×s
string-hash lookups and 143k vector allocations per request, all avoidable.

**Expected win.** Measured: `gather_ns` is 5.1–6.5% of the request — a real but secondary
pool; worth taking for its simplicity, not its size. Structural note: the 1e9 corpus
declares **no scalars**, so the scalar term is invisible in this benchmark — the HashMap
and per-point-Vec claims are code-evidenced and grow with every declared scalar a real
corpus adds; the numeric SoA saving (see B4) is what the 5–6% measures.

**Trade-offs.** Changes the engine→server type (`ViewportOut.points: Vec<PointOut>` →
columns): touches the hand-written `PartialEq`, the engine tests, and the server assembly —
mechanical but wide. No semantics, no wire change (the wire is already columnar — that is
the irony: the engine builds rows from columns, and the server immediately rebuilds
columns from rows). No invariant interaction.

**Adoption requires.** Nothing but the refactor and its test updates. Natural to do
jointly with B4.

### B4 — Response assembly de-duplication

**Proposal.** Remove the redundant copies between engine return and wire bytes
(`crates/tessera-server/src/viewer.rs:183-262`):
(a) `state.engine.meta()` is called per request and **clones** the manifest's slice list
and declared-scalar list (`viewport.rs:209-224`) to use only the scalar names — cache the
`EngineMeta` (or an `Arc` of the names) per generation;
(b) the point transpose (`point_ids`/`xs`/`ys` re-walk) and `build_scalar_columns`
(`viewer.rs:343-389`, which clones every Utf8 cell **again** — the third copy of each
string between Arrow mmap and wire) both disappear once B3 makes the engine emit columns;
(c) minor: per-response `PinDto` JSON, four `Vec<u64>` tile-column re-walks (≤289 rows,
cheap).

**Expected win.** Bounded by the non-engine remainder of server time (assembly +
serialisation sit inside the 11–17% of the request that is not `select_ns`). Under
today's θ=16 defaults responses are small (79–108 KB measured), so the copy waste is
modest; it scales directly with `theta_target_marks`/`k_max_marks` policy — a deployment
that raises the mark budget re-inflates this pool (the 07-29 engine at k=5000 shipped
7.7 MB responses). **Notably, `matched` is byte-identical to `visible` in
Phase 1** (no filters; `viewport.rs:547-553` sets `matched: visible`) — the wire sends the
same column twice by design; noted, not proposed for change (it is the contracts §3 shape).

**Trade-offs.** None of substance; the meta-cache must invalidate on generation swap
(ArcSwap already provides the hook). No invariants touched — this is after all
authorisation decisions.

**Adoption requires.** Nothing; pairs with B3.

### B7 — Underlay sub-cell sweep

**Proposal.** The §3.3 underlay evaluates `4^offset` sub-cells per non-empty tile, each via
`tile_ranges_within` (two gallops) + `count_range` (`viewport.rs:565-591`) — on a clustered
corpus, mostly to discover emptiness (the `underlay_cells_evaluated` vs `sub_cells` gap
exists precisely to measure this). Two routes:
(a) **one monotone gallop over the parent's range** resolving all sub-cell boundaries in a
single sweep — sub-cells partition the parent contiguously in Morton order, so this is the
`tile_ranges_all` construction applied one level down, with the same correctness lemma and
the same positional-equivalence test pattern;
(b) when `visible(tile) ≪ 4^offset`, decode the tile's visible rows once (B9's cursor) and
bucket their morton codes into sub-cells — O(visible) instead of O(cells × searches).

**Expected win.** Not exercised in this campaign — the underlay was off in every headline
run (it is opt-in per request), so no share is quoted; the `underlay_cells_evaluated` vs
`sub_cells` counters exist to measure the gap when a client starts requesting it. Zero
effect on the default path.

**Trade-offs.** Route (a) is mechanical reuse of a proven construction — low risk. Route
(b) introduces a second observable route (timing-only; output identical — the oracle's
underlay tests already pin sub-cell counts). Choose (a) alone unless the measured gap
justifies both.

**Adoption requires.** Tests per the `tile_ranges_all` pattern; no design interaction.

### B8 — Allocation hygiene

**Proposal & inventory.** Small, riskless, additive:
- `tile_counts`/`points`/`sub_cells` built with `Vec::new()` despite known bounds
  (`viewport.rs:503-505`) — pre-size from `tiles.len()` and Σserved estimate.
- The row-projection cache key allocates `slice.to_string()` + `generation.prefix.clone()`
  **per request** (`viewport.rs:383-387`) — intern or restructure the key.
- Selection allocates a fresh `BinaryHeap` + two `Vec`s per non-empty tile
  (`select.rs:289,307-310`) — hoist and reuse across the tile loop.
- `range.clone()` per sub-cell in the underlay loop (cheap; tidy with B7).

**Expected win.** Hundreds of allocations per request removed; individually sub-µs.
`[PENDING: before/after not separately measured — bundled into B9's allocation
accounting]`. **Explicitly not proposed:** re-decorating the serve-all sort — measured a
regression (−3% to −6%) and reverted once already (2c25be1, rationale preserved at
`select.rs:248-258`); listed so it is not rediscovered.

**Adoption requires.** Nothing.

### B5 — `count_range` empty-diff fast path — **investigated, rejected**

The proposal was to skip the `minus`/`plus` `range_cardinality` calls per tile when the
overlay diffs are empty (`compose.rs:94-99` makes three calls unconditionally). On
inspection: `range_cardinality` on an **empty** bitmap is a container binary search over
zero containers — nanoseconds. The saving is ≈ 2 × ~ns × tiles ≈ single-digit µs per
request at best. Recorded so the "obvious win" is not re-proposed; superseded by B9, whose
steady-state route removes these calls as a side effect. Measured: `count_ns` is 3.4–5.8%
of the request — closed.

### B6 — `EffectiveMask` (compose) caching — **conditional, deferred**

`compose()` runs per request and walks the **entire overlay + buffer** (`compose.rs:237,
254`), `row_of`-ing each entry, sorting two vecs, building four bitmaps — even when both
are empty (steady state, ~µs) or unchanged since the last request (always, between
generation swaps). A cache keyed `(token_id, slice, segments_version, overlay_version,
buffer_version)` beside the row-projection cache would make it once-per-change.

**Why deferred:** in steady state the walk is microseconds (measured: `compose_ns` ≈ 0.0%
of request time across all 44 bench cells); the
cost grows with overlay depth (the changes arm measures checkpoints to 20k entries), so
this matters under sustained churn, which Phase 1 benchmarks do not model. **The danger is
the key**: omit `overlay_version` and a suppression stops applying to the next request —
fail-open against lifecycle §2.3. If adopted, the key is the review item; and it must not
copy the neighbouring caches' no-eviction posture (§5).

---

## 3. Bundle size review

### 3.1 Where the bytes go

Measured ground truth for the 07-29 1e9 bundle (`probes/tail-discrimination.json`; per-file
`du -sb`), projected forward to the current format (post external-ID rework). The overhead
model reproduces measured sizes to the byte: payload B/row + (n_columns/8) B/row validity —
arrow-rs writes a validity bitmap per column even for non-nullable columns.

| Component | GB @1e9 | Read on |
|---|---|---|
| `columns.arrow` — id 8 + x 4 + y 4 + priority 2 + validity 0.5 | 18.50 | **viewport (hot)** |
| `external-ids-*.arrow` (16.25) + `ext-locator.u32` (4.0) | 20.25 | `/v1/items`, control plane — never viewport |
| `morton.u32` | 4.00 | **viewport (hot, page-sparse)** |
| `permutation.bin` | 4.00 | session authorise (projection) |
| `postings.arrow` | 0.145 | session authorise |
| `terms/pairs.parquet` | 0.121 | nothing in `crates/` — oracle only |
| dictionary, manifests | ~0.001 | boot / authorise |
| **Total (projected)** | **47.02** | |

Corrections to folklore, from measurement: the bundle's `pairs.parquet` is **0.121 GB**,
not the ~6 GB the contracts spec §2.4 estimated (48× over; the spec should be annotated) —
it is delta-packed + snappy and read by nothing in the serving system. And `verify` cost is
mostly *digesting*: every `open_bundle` SHA-256s ≈ 26.8 GB (columns + morton + permutation
+ postings) plus a full morton sortedness scan and an O(N) permutation bijection check —
the measured 110 s boot (§D5).

### 3.2 D1 — External-ID family: **ruled — representative cost, keep; add a conformance flag**

The 20.25 GB family is minted by the build from the synthetic corpus's own entity ids
(8-byte LE values; `crates/tessera-build/src/lib.rs:897-899`). The owner's ruling: this is
**deliberate** — real deployments will supply caller external IDs, and the fixtures should
carry the cost realistically. Consequences accepted into this memo:
- The family is a fixed, representative cost, **not a reduction target**. Real IDs
  (UUIDs/strings, 16–36 B, high entropy) would be larger and barely compressible, so the
  earlier compression idea (dense integers compress well) does not transfer to production
  and is withdrawn from the headline.
- One residue stands: contracts §2.4 *forbids* manufacturing external IDs for items that
  have none, and the build currently mints **unconditionally** (no flag exists;
  `pipeline.rs:377-382`). Recommendation: `--mint-external-ids` opt-in, so bench fixtures
  keep realism and a production build with no caller namespace stays spec-conformant.
  Cost: one flag.

### 3.3 D2/D2b — Position storage: decouple the sort key from the stored precision

**The two roles must be separated** (this resolves the discussion's false start):
1. **Sort/tile key** — `morton.u32`, the 2¹⁶ × 2¹⁶ grid. Fixed: tiles are code prefixes
   (contracts §2.5), the ascending-u32 load lemma anchors the galloping sweep, and the
   `(morton, tessera_id)` tiebreak gives B1 its sorted runs. Making a finer (64-bit) code
   the *sort key* would order rows within a cell by sub-position instead of by id —
   shredding B1's runs to length ~1 while buying nothing queryable (no tile ever goes
   below depth 16). Not proposed at any precision.
2. **Stored position** — free choice of column format, any precision, because it is
   payload, never sorted or range-queried. Today: `x,y: f32` **as supplied** — the format
   does *not* quantise position to the grid; this corpus's positions are cell corners only
   because the Phase-0 generator emitted morton codes and nothing finer existed at ingest.

| Position storage | B/row | Precision (per axis) | Δ disk @1e9 |
|---|---|---|---|
| f32 pair (today) | 8 | ~2⁻²⁴ relative — non-uniform, 1/256 cell near extent max | — |
| u8 sub-cell pair | 2 | uniform 2⁻²⁴ extent — 1 px at max zoom | −6 GB |
| **u16 sub-cell pair (recommended)** | 4 | uniform 2⁻³² extent — 0.004 px at max zoom | **−4 GB** |
| u32 sub-cell pair | 8 | uniform 2⁻⁴⁸ extent — strictly beats f32 | 0 |
| u64 interleaved data column | 8 | = u32 pair | 0, plus deinterleave at gather; dominated |

Supporting facts: a cell renders at 2^(z−8) px (256 px/tile convention, display-
independent), so refinement step at max zoom = 256/2ⁿ px; pairwise distance error ≤ ~1.4 ×
2⁻(16+n) of the extent. The wire currently carries f32, whose 24-bit mantissa holds a
(16+n)-bit coordinate exactly only for n ≤ 8 — stored n = 16 is future-proofing that a
later wire field or server-side distance computation would realise (F1 is the first
concrete consumer). **Per-segment elision when the refinement is all-zero** (manifest-
flagged) unifies the pre-quantised case: this corpus stores nothing (full −8 GB), a
continuous corpus stores 4 B/row (−4 GB versus today). Requires a contracts §2.6 revision
(it currently mandates f32 "as supplied") — a §0.3-deviation-sized change.

**D6 — the boundary clamp (independent finding).** `cell()` silently clamps out-of-extent
coordinates onto boundary cells 0/65535 (`morton.rs:49-65`) while the stored position keeps
the true value: tile membership and rendered position disagree for outliers, and **edge
tiles absorb every outlier into their counts** — a data-integrity artifact in a system
whose product is the counts. Latent today (the morton input branch requires the identity
extent, so this corpus cannot clamp); bites the first real corpus with a mis-declared
extent. Fix belongs at build time — out-of-extent input is an explicit decision (reject, or
clamp-and-report a count) — independent of any storage choice.

### 3.4 D3 — `priority` column: one decision with B2

2.0 GB. Three coherent positions: **use it** (B2 fires), **drop it** (−2 GB and a build
simplification, but forecloses B2 without a full rebuild — contradicts r22's recorded
intent), **keep as option premium** (status quo; costs 2 GB per 1e9 bundle for B2's
reversibility). The measurements now weigh in: B2's warm-path win measured at ≤10% of
selection (§2/B2), so the option is worth little on latency grounds — **the case for
dropping the column is now stronger than the case for keeping it**, unless the owner
values the residual residency argument. Decide with B2, as one decision.

### 3.5 D4 — Drop `permutation.bin`: **investigated, rejected**

The permutation (4 GB) is fully derivable: the manifest carries the identity key, and
`invert(tessera_id)` recovers each row's entity in ~8 Feistel rounds, so one O(N) pass at
open rebuilds `row_of`. Rejected because it trades 4 GB of *disk* for 4 GB of *RSS* plus an
O(N) boot pass — and RAM, not disk, is the binding constraint on the measurement box (and
the residency margin is what B1/B2's economics depend on). Recorded so the redundancy is
known and the trade understood. (Micro-observation en passant: the Feistel round keys are
recomputed per row inside `f` — hoisting them is a build-time-only nicety.)

### 3.6 D5 — Boot-time digest scope (flag, not proposal)

Boot digests ≈ 26.8 GB → ~110 s at 1e9. The sidecar family already has a recorded precedent
for narrowing (§0.3 deviation 9: defer, verify per-extent at first touch). Extending
defer-and-verify-on-touch to columns/morton/permutation would cut boot to seconds at the
cost of weakening the fail-closed open protocol — an owner-level trade against the §2.3
reader protocol, flagged here because the 90–180 s boot tax recurs on every serve/bench
cycle, but **not recommended** by this memo.

### 3.7 Combined effect

With D1 ruled (keep) and D2b + D3-drop taken: 47.0 → **37.0 GB** on disk — but the
consequential number is the **viewport-hot set: 22.5 → 12.1 GB** (id column 8.1 + morton
4.0), comfortably resident beside masks and postings on a 47 GB box. That residency shift
feeds directly back into §2: it moves B1's scan-vs-search crossover toward search and makes
B2's traffic reduction land on DRAM rather than page-cache misses. The size and speed
reviews are one decision surface, not two.

---

## 4. Capability sketches (costed, not proposed)

### F1 — Occlusion drill-down ("what's under this mark")

On mouse-over of a rendered mark: report the count and a sample of the **authorised-but-
unserved** points within radius d of it, d ≈ the client-computed distance to the next
rendered mark. Owner-refined semantics: the k returned points are the **continuation of the
local sample**, not the k nearest — i.e. the next k lowest `tessera_id`s in the disc above
a client-supplied cursor (the client's local max served id). Because ids are
Feistel-scrambled, lowest-k-by-id *is* a uniform sample — the §7.2 sampling order applied
to a disc; deterministic, oracle-testable, consistent with the rendered prefix (which is
the same ordering), and zoom-graceful (deep zoom ⇒ few unserved candidates ⇒ naturally
exhaustive; zoomed out ⇒ fixed-k uniform draw — no mode switch).

**Cost model** (existing structures, no new index): decompose the disc into ~9–20 Morton
cover ranges at the depth where cell ≈ d (gallop; pages warm from the surrounding
viewport); count = Σ `range_cardinality` (~µs); sample = the same peek-reject heap over
the cover ranges (~tens of µs; candidate population ≈ the overplot share V/m). **~10–100 µs
per query; zero contact with the viewport path**; 20 Hz mouse-move is negligible load.

**Gates.** (1) *The security line*: "invisible" means inside `M_auth` and unserved — a
count of masked-out points at any radius is exactly the I2 disclosure this system exists to
prevent; the safe variant is an aggregate over `M_auth`, which I2 permits and the
Non-Truman stance endorses. (2) A **sixth viewer verb**: the leak register is exhaustive
because the surface is enumerable — this widens it, and C18's "a deeper zoom already
returns sub-cell counts" argument does **not** cover arbitrary-centre, arbitrary-radius
discs; a new register entry and owner sign-off are required even though the I2 analysis
looks clean. (3) A radius/cover cap (as `max_tiles_per_request`) so d cannot demand a
whole-extent scan. (4) The distance filter below one cell needs the D2b refinement bits
(n ≥ 8) — F1 is the first concrete consumer of sub-cell precision.

**Unification.** F1's cursor and B11's per-tile cuts are the same request primitive
("continue the id-prefix past what I hold, within this region"). Two features arriving at
one primitive independently is the usual sign it is the right primitive; specifying them
together halves the contracts cost.

---

## 5. Findings handed to the threading workstream

Recorded, deliberately not acted on here:
1. `row_projection_cache` is a `std::sync::Mutex` held **across** `RowProjection::new` —
   seconds at 10⁹ on a session's first viewport — blocking every other session's viewport
   for the duration (`viewport.rs:388-406`).
2. `SessionRegistry`: one global mutex + `Arc` clone per request on every plane
   (`server/src/state.rs:81-85`).
3. Neither the row-projection cache nor the fragment cache evicts anything, ever; B10/B6
   must not inherit that posture, and the existing caches deserve one.

---

## 6. Recommendations in one table

| # | Candidate | Win (measured basis) | Risk | Needs owner? | Verdict |
|---|---|---|---|---|---|
| B9 | Three-tier adaptive decode (built: `perf/b9-run-decode`) | **Measured: −25% at 1e9 op point (163.6→123.3 ms), −3% at 2.4M, no regression at any density** | Low — output bit-identical, oracle ×2 | C19 note sign-off | **Validated; awaiting landing** |
| B10 | Per-session tile memo | **5–10× on pan steps** (405–450 µs/tile × 80–94% overlap) | Medium — two absolute keying edges | No (review edges) | Do after B9 |
| B1 | Sub-Σvisible C_θ | **Refuted by cell-occupancy measurement** (`probes/2026-07-30-1e9-rebuild/cell-histogram-1e9-subclass.json`): 1.55 rows/cell global, 1.40 in the reference viewport, 0.2% of rows in cells ≥ 32 vs break-even 25–50 — the per-cell route would regress ~15×. Residual kept: **single-cell-tile fast path** (a tile whose range lies in one cell is fully id-sorted — 2-load gate, C_θ by binary search; fires on the mega-cell tail, ~5% of rows in ≥1024-row cells) | — | — | **Rejected as a route; residual = candidate 4th tier** |
| B3+B4 | SoA gather + assembly dedup | 5.1–6.5% (`gather_ns`) + assembly; grows with scalars/policy | Low — mechanical, wide | No | Do together, second wave |
| B2 | Priority prefix scan | **≤10% — r22's trigger has NOT fired** (id-load marginal 0.2–0.4 of 4.5 ns/row) | — | **Yes** | Decline; decide D3 with it |
| B11 | Client-cut deltas | Policy-dependent (small at θ=16; large at big mark budgets) + progressive render | Spec surface; deny-convergence gate | **Yes** (contracts) | Design with F1, adopt when policy needs it |
| B7 | Underlay sweep | Underlay-only (off in all headline runs) | Low (route a) | No | When underlay ships to a client |
| B8 | Allocation hygiene | Small, free | None | No | Fold into B9 PR |
| B5 | count_range fast path | `count_ns` measured 3.4–5.8%; empty-diff calls ~ns | — | — | **Rejected** |
| B6 | Compose caching | `compose_ns` measured ≈0% steady-state; churn-only | Fail-open key risk | Review key | Deferred |
| D1 | External-ID flag | conformance only | None | Ruled | Flag only |
| D2b | u16 sub-cell pair + elision | −4 to −8 GB; hot set −8 GB; enables F1 | Contract §2.6 rev | **Yes** | Recommended |
| D3 | Drop/keep priority | −2 GB or B2 | Couples to B2 | **Yes** | With B2 |
| D4 | Drop permutation | −4 GB disk / +4 GB RSS | — | — | **Rejected** |
| D5 | Digest deferral | −100 s boot | Weakens fail-closed open | **Yes** | Flagged only |
| D6 | Extent clamp validation | Count integrity at borders | None | No | Recommended |
| F1 | Occlusion drill-down | New capability, ~100 µs/query | 6th verb; register entry | **Yes** | Sketch ready |
