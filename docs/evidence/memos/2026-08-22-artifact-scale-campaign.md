# The artifact scale campaign — what the built engine does at scale

**Date:** 2026-08-22 · **Status:** Evidence, not normative. It closes the campaign
[the plan](2026-08-21-artifact-scale-plan.md) §8 set out, validates
[`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md) against the engine that
now implements it, and reports against [`artifact-delivery.md`](../../artifact-delivery.md)'s
**Stage 7** check. The raw record is
[`probes/2026-08-22-artifact-serving-e2e/`](../../../probes/2026-08-22-artifact-serving-e2e/README.md),
where every figure below has a CSV beside it.

**What is new about it.** The scale investigation measured the design *probe-side*, in a bench
binary that owned its own control flow over synthetic structures. This measures the built engine
through the **real request path**: `tessera build` over a materialised corpus, `tessera serve` over
HTTP, sessions established as a client establishes them, wire frames decoded by the reference
oracle's own decoder. Nothing between the socket and the store is stubbed.

---

## Results

**The correctness spine holds exactly, at every cell it was asked** — 98 cells in total, none
inexact. 35 of 35 at 10⁷ points and 35 of 35 at 10⁶ over five layer shapes (overlapping enumerated,
the partition relation stored, the same relation as an attribute predicate, a spatial predicate, a
`nested` lineage), 28 of 28 at 5×10⁷ over four, each by seven principals from 93.75% of the corpus
down to a single term, compared **artifact for artifact, both directions, exact equality** against
the generator's closed form. The two spellings of one relation agree at every rung, which is
Stage 6's twin-equality check taken over HTTP at half a million artifacts.

**The design's serving claims are reproduced end to end — and only after the first fold.** A freshly
built bundle serves its enumerated layers `ArtifactMajor` whatever their locality; the fold
re-evaluates and flips them. Measured on the same 10⁷ fixture, rebuilt and folded once with nothing
ingested, the worst cells fall **2 430 → 221 ms** (flat, → `RowMajorList`) and **1 464 → 139 ms**
(the partition twin, → `RowMajorLabel`) — eleven and ten and a half times. The shape changes as well
as the size: post-fold both grids are monotone in the mask and in the viewport, the cost inversion
is gone, and the enumerated twin at 132.9 ms and its predicate spelling at 135.5 ms are the same
number, which is what one relation served two ways should cost. The same flip, with larger numbers
on both sides, at 5×10⁷.

**Scale in the artifact count is what costs, and the corpus behind it is nearly free.** Post-fold
whole-map at the broad principal: **135 ms** at 10⁵ artifacts over 10⁷ points, **830 ms** at
5×10⁵ over 5×10⁷ — 6.1× for 5× the artifacts and 5× the corpus. The spatial predicate is **1 ms flat
at every one of its 98 cells**, at both tiers, under every mask and every viewport.

**The envelope, and the resource it has.**

| | 10⁷ points, 10⁵ artifacts | 5×10⁷ points, 5×10⁵ artifacts |
|---|---|---|
| peak throughput | **28.9 rps** at 8 sessions | **7.8 rps** at 8 sessions |
| where cores saturate | 32 sessions, 9.4 of 12 | 32 sessions, 10.3 of 12 |
| p50 at 32 | 1 535 ms | 6 047 ms |
| p99 at 128 | 13 691 ms | 60 303 ms |
| RSS at 128 | 11.8 GB | **40.6 GB** |
| errors at 128 | 0 (7 shed) | **170** |

**Memory binds, and it scales with the corpus.** A principal's own `M_auth` and its cached row
projections cost about **76 MB per session at 10⁷ points and 221 MB at 5×10⁷**. Masks are not
shared — the scenario the owner set — so the concurrency ceiling *falls* as the corpus grows: at
5×10⁷ the box runs out at 128 sessions. Extrapolated, 128 principals over 10⁹ points want several
hundred gigabytes of mask alone. Session establishment is not the cost the million-term space made
it look like: a 131 072-term grant authorises in **0.06 s**.

**The named gap — serving during a fold — has two answers, pointing opposite ways.** The fold's own
duration is **unaffected by load**: 82.7 s with 32 sessions panning against 84.1 s unloaded
(**0.98×**, against an acceptance bar of 2×), its artifact pass 22.8 s against 23.0 s. Its memory is
not: `staircase_rss` 7.56 GB loaded against 3.86 GB unloaded. But **a live session's tail moves by
35×** — p99 goes 1 683 ms → **58 924 ms** during the fold and back to 1 714 ms immediately after,
with p50 flat to within 1% across every window and nothing shed or errored. The requests waited.

**Freshness is correct both ways and the two ways differ.** A predicate layer's count moves at its
flush — 12.2 s after the `POST` with 32 sessions live, 5.6 s unloaded — and an enumerated
membership's growth counts at the fold that makes its rows base rows, understating until then, which
is fail-closed and `annotation-write-cycle.md` §4.1's posture. **Ingest during serving costs the
read path nothing**: p99 1 682 → 1 696 ms, while 1 000 000 rows landed at 47 421 rows/s with batch
p50 21.4 ms, and the write queue refused the other half of the offered load with `429` and a
`retry_after_s` rather than degrading.

**The design ceiling is reached, and it is not the count that binds there.** An attribute predicate
over a keyed `u32` mints **9 832 352 artifacts** over 10⁷ points. A three-percent principal's
whole-map request is **395 ms** — the row-major route working at ten million artifacts — and a
93.75% principal's is **12.5 s**, because the response *is* 9.2 million artifacts in a 954 MB body,
2.8 s of which is Arrow encoding. `artifact-serving-at-scale.md` §7.1 models the unmeasured 10⁹/10⁷
cell at ~550–900 ms; that model is about the count, and this says the count is not what a request at
that size pays for.

---

## Five things for the owner, and one of them is a defect

**1. The build records a layout its own report contradicts.** It reports 108.0 and 73.3 blocks per
artifact at 10⁷ (178.9 and 93.7 at 5×10⁷) — decision 0092's figure — and records `ArtifactMajor` for
both layers, which is what `layout::choose` returns for neither.
[0094](../../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
says the layout is chosen at the build *and* re-evaluated at every fold; on this evidence only the
second half happens. The cost of the gap is the 10–11× above, paid by every deployment between its
build and its first fold. `generator/treed` staying artifact-major is the rule working correctly:
153 blocks per artifact but **998** artifacts, against `ROW_MAJOR_MIN_ARTIFACTS` of 1 000.

**2. ⊘ A cold request is truncated mid-body and nothing says so.** The first `/v1/viewport` after
boot naming a level whose row form is not built is aborted when that build outruns the 60-second
whole-stream deadline: the build happens *after* the response's first flush, so the deadline fires
on the next send. The client gets a `200` with a truncated body and no trailer; neither
`viewport stream aborted` log site fires. Reproduced deterministically at 9.83M artifacts — 111 s
and a truncation at the shipped deadline, 110.7 s and success at ten minutes, 4 ms on the second
pass — and twice more at 5×10⁵ artifacts, so the trigger is a row-form build over about half a
million artifacts rather than an extreme one. It recurs after every boot and every fold that
rebuilds a row form. Fail-closed at the client, silent at the server. **The reproduction is the
probe's §8; the campaign wrote no engine code.**

**3. ⊘ A 5×10⁷-member enumerated membership is refused by a path documented as unreachable.**
`generator/treed`'s root holds the whole corpus, and at 5×10⁷ the build stops with *"1 membership(s)
of generator/treed did not survive their own encoding"* — `store.apply` returning a non-zero refusal
count at the site whose own comment reads *"Unreachable: the memberships were serialised from
bitmaps two calls ago."* The refusal is `deserialise_members` returning `None`: a Roaring bitmap
failing its own `Portable` round trip in the process that wrote it. Fail-closed, with 21 GB of
headroom, and the same layer at 10⁷ builds and censuses exactly.

**4. ⊘ `tessera build`'s peak resident size is not bounded by its memory budget.** At 2.5×10⁸ points
it is OOM-killed at **47.3 GB** with the budget auto-derived, **47.5 GB** with `--memory-budget 12g`
and **47.55 GB** with the same budget and the two largest member sources dropped. Three runs, three
kills, one number: the peak is the machine. At 10⁸ the same kill lands at 47.6 GB. Recoverable and
disclosing nothing — but a build that is *killed* rather than *refused* is the failure the
pre-flight exists to prevent, and the flag that exists to prevent it does not.

**5. Nothing bounds a wide response over a large flat layer.** `artifact_budget` is accepted and
inert on a flat layer — the wire contract says so, a budget being met by serving ancestors — and at
`artifact_budget = 100` the same request still returns 254.6 MB. Whether a wide request over a
ten-million-artifact flat layer should be answerable at all is an owner question.

---

## What was not run, and why

**⊘ The 10⁹ tier: a disk refusal, quantified rather than attempted.** Inputs and bundle both scale
linearly and both are measured at 10⁷ — 1.46 GB and 1.21 GB — so at 10⁹ they are **146 GB and
121 GB**, and the build reads every input while writing the bundle. The transient requirement is
their sum, **267 GB, against 147 GB free**. Dropping the three member files leaves 214 GB;
`--no-oracle-pairs` leaves 192 GB. Nothing available brings it under. It is a *different* wall from
the one the probe campaign hit at the same corner — that one was memory, 45 GB resident with all
12 GB of swap gone — and the two are independent.

**⊘ The 2.5×10⁸ and 10⁸ tiers: OOM, three times and once.** Finding 4. The largest tier this box
builds is **5×10⁷**, and only without the layer finding 3 refuses.

**⊘ 10⁷ artifacts over 10⁹ points was therefore not reached.** The artifact axis was reached on its
own — 9 832 352 artifacts over 10⁷ points — and that probe is the record's §7. Both axes large at
once needs a larger machine, which is where the scale investigation left it too.

**⊘ A whole-corpus principal is not expressible.** At the campaign's 1 048 576-term space one would
hold every term, and `/session/authorise` buffers `auth_data` under a 2 MB body limit — about
150 000 seven-digit descriptors. The broadest grant that fits is two whole term levels, **131 072
terms and 93.75% of the corpus**. The design's §7 grid has a 100% row this campaign cannot ask for.
Separately, `tessera corpus artifact-census` takes its grant as one argv string, which Linux caps at
128 KB, so a bench arm reads it from a file instead.

**⊘ `tessera corpus materialise` panicked once in four runs at 2.5×10⁸**, inside the parquet crate's
dictionary encoder (`dict_encoder.rs:48`, `index is 4294914058` — a `u32` sentinel used as an
index). Flaky rather than reproducible, on the fixture path, recorded because it happened.

**⊘ And one measurement was thrown away.** The first concurrency sweep reported 4.5 rps at 128
sessions with the server at 1.6 of 12 cores. Both halves were the harness: a Python driver with a
thread per session, decoding every 11 MB artifacts frame into a hundred thousand tuples under the
GIL. It was caught by sampling the **driver's** own CPU beside the server's — pinned at 1.4 cores,
which is Python's ceiling — and the load arm was rewritten in Rust. Every load figure in this memo
comes from the Rust arm; the wrong ones are in the git history at `6631ec8`, deliberately.

---

## The bracket re-run: the anomaly reproduces and reverses the question

The 2026-08-20 campaign's layout sweep broke its own trend at `blocks = 8` and queued a targeted
re-run. Re-run at 6, 8, 10 and 12 blocks per artifact with the iteration count doubled and three
processes per point, the whole-map cell reads **104.1 → 42.8 → 44.3 → 36.2 ms** and the worst cell
**122.3 → 81.7 → 67.4 → 68.1 ms**, with run-to-run ranges that do not overlap between 6 and any of
the others.

So the point at 8 is not a break in a rising trend: **the trend reverses there and keeps falling.**
Artifact-major cost stops rising at about six blocks per artifact and declines thereafter. Every
other fixture statistic is held exactly across the four points — 0.960 members per row, 32 distinct
containment expressions over 2 000 000 pairs, a 2.0 MB tile index — and the one quantity that moves
with the cost is the **`everywhere` set**, the artifacts too wide for any node of the tile index,
rising 1.6% → 2.3% → 2.9% → 3.6% as the cost falls. Widening a membership past the point where any
node contains it appears to move it onto a cheaper path; that is a correlation over four controlled
points, not a mechanism this campaign instrumented.

⊘ **`layout::ROW_MAJOR_BLOCKS_PER_ARTIFACT` is 10.0 and provisional pending exactly this run. The
run does not give it a better number — it says there is no crossover to find in this band on the
artifact-major side**, because at 10.0 the heuristic flips away from a layout that is improving.
Whether the constant should move, and which way, is an owner question with four measured points
behind it now.

**Two of the twelve runs were not this campaign's own.** The first bracket process was killed by the
environment after blocks 6 and 8 completed; blocks 10 and 12 were resumed with the identical command
line. Every point is three processes at six iterations.

---

## Acceptance, clause by clause

The delivery record's Stage 7 check: *"10⁹ points carrying 10⁶ artifacts, with 10⁷ as targeted
probes, serve inside budget for many concurrent principals, fold under load, and leave the write
path correct at every interleaving."*

| clause | verdict |
|---|---|
| **10⁹ points carrying 10⁶ artifacts** | **not met** — the largest tier this box builds is 5×10⁷ points carrying 5×10⁵ artifacts. Four independent walls above it, quantified above; only the last is disk |
| **10⁷ artifacts as targeted probes** | **met on the artifact axis, not on both at once** — 9 832 352 artifacts over 10⁷ points, measured; a three-percent principal's whole-map request is 395 ms and a broad one's is 12.5 s |
| **serve inside budget for many concurrent principals** | **met at 10⁵ artifacts, not above** — the one-core one-second budget holds at every post-fold cell of the 10⁷ tier; at 5×10⁵ artifacts the widest cells are 0.8–1.2 s, and at 10⁷ artifacts the widest is 12.5 s. "Many concurrent" is bounded by memory, not by the budget: 8 sessions is the throughput knee and 32 the core knee at both tiers |
| **fold under load** | **met** — 0.98× the unloaded duration, artifact pass 0.99×, nothing shed. With the tail caveat above, which the clause does not cover and the record does |
| **leave the write path correct at every interleaving** | **met for what this campaign tests** — freshness exact on both membership kinds across ingest, flush and fold, the census exact after them, the write path refusing with `429` rather than degrading. The *constructed* interleavings are the battery on `artifacts/interleavings`, not this campaign |
| **the row-major counts match the generator census exactly** | **met** — 98 of 98 cells across three corpus sizes, including both row-major layouts after the fold |
| **the fold completes inside `plan_fold`'s memory ceiling under a live serving load** | **met at 10⁷** — `staircase_rss` 7.56 GB under 32 sessions, against the ~9–10 GB anonymous peak `plan_fold` budgets |
| **both axes swept through their middles** | **met** — six principal breadths by seven viewports at every tier, and the ridge the plan warned of is real: on a pre-fold artifact-major layer the worst cell is at the **25%** viewport, 3× its whole-map neighbour, which a grid sampled at its extremes would miss entirely |

---

## Appendix R — review trail

| revision | date | what changed |
|---|---|---|
| r1 | 2026-08-22 | Created — the Stage 7 campaign's closing memo |
