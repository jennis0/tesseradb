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

**The correctness spine holds exactly, at every cell it was asked.** 35 of 35 census cells at 10⁷
points and 35 of 35 at 10⁶ — five layer shapes (overlapping enumerated, the partition relation
stored, the same relation as an attribute predicate, a spatial predicate, a `nested` lineage) by
seven principals from 93.75% of the corpus down to a single term, compared **artifact for artifact,
both directions, exact equality** against `tessera corpus artifact-census`. The two spellings of one
relation agree at every rung, which is Stage 6's twin-equality check taken over HTTP at a hundred
thousand artifacts.

**The design's serving claims are reproduced end to end — and only after the first fold.** A
freshly built bundle serves its enumerated layers `ArtifactMajor` whatever their locality; the fold
re-evaluates and flips them. Measured on the same fixture, rebuilt and folded once with nothing
ingested, the worst cells fall **2 430 → 221 ms** (flat, → `RowMajorList`) and **1 464 → 139 ms**
(the partition twin, → `RowMajorLabel`) — eleven and ten and a half times — and the shape changes as
well as the size: post-fold both grids are monotone in the mask and in the viewport, the cost
inversion is gone, and the enumerated twin at 132.9 ms and its predicate spelling at 135.5 ms are
the same number, which is what one relation served two ways should cost.

**The envelope, at 10⁷ points and 10⁵ artifacts, 45 s of sustained panning per level:**

| sessions | throughput | p50 | p99 | server cores (of 12) | peak RSS | shed |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 6.7 rps | 145 ms | 185 ms | 1.00 | 2.06 GB | 0 |
| 8 | 28.9 rps | 354 ms | 553 ms | 4.84 | 2.75 GB | 0 |
| 32 | **29.1 rps** | 1 535 ms | 2 072 ms | **9.39** | 5.19 GB | 0 |
| 128 | 16.5 rps | 9 821 ms | 13 691 ms | 7.36 | **11.76 GB** | 7 |

The knee is at 32 sessions and what binds is the machine. Past it throughput regresses and latency
is pure queueing; the compute gate sheds seven requests at 128, which is its admission control
working. Memory is the resource that will bind first at a larger tier: **~76 MB per principal**,
because masks are not shared. A 131 072-term grant authorises in 0.06 s, so the million-term corpus
does not make session establishment the cost it looked like it would.

**The named gap — serving during a fold — has two answers, and they point opposite ways.** The
fold's own duration is **unaffected by load**: 82.7 s with 32 sessions panning against 84.1 s
unloaded (**0.98×**, against an acceptance bar of 2×), and its artifact pass 22.8 s against 23.0 s.
Its memory is not: `staircase_rss` 7.56 GB loaded against 3.86 GB unloaded. But **a live session's
tail moves by 35×** — p99 goes 1 683 ms → **58 924 ms** during the fold and back to 1 714 ms
immediately after, with p50 flat to within 1% across every window and nothing shed or errored. The
requests waited.

**Freshness is correct both ways and the two ways differ.** A predicate layer's count moves at its
flush — 12.2 s after the `POST` with 32 sessions live, 5.6 s unloaded — and an enumerated
membership's growth counts at the fold that makes its rows base rows, understating until then, which
is fail-closed and `annotation-write-cycle.md` §4.1's posture. **Ingest during serving costs the
read path nothing**: p99 1 682 → 1 696 ms, while 1 000 000 rows landed at 47 421 rows/s with batch
p50 21.4 ms, and the write queue refused the other half of the offered load with `429` and a
`retry_after_s` rather than degrading.

---

## Three things to decide, and one to fix

**1. The build records a layout its own report contradicts.** It reports 108.0 and 73.3 blocks per
artifact — decision 0092's figure — and records `ArtifactMajor` for both layers, which is what
`layout::choose` returns for neither.
[0094](../../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
says the layout is chosen at the build *and* re-evaluated at every fold; on this evidence only the
second half happens. The cost of the gap is the 10–11× above, paid by every deployment between its
build and its first fold. `generator/treed` staying artifact-major is the rule working correctly:
153 blocks per artifact but **998** artifacts, against `ROW_MAJOR_MIN_ARTIFACTS` of 1 000.

**2. ⊘ A cold request is truncated mid-body and nothing says so.** The first `/v1/viewport` after
boot naming a level whose row form is not built is aborted when that build outruns the 60-second
whole-stream deadline: the build happens *after* the response's first flush, so the deadline fires
on the next send. The client gets a `200` with a truncated body and no trailer; neither of the two
`viewport stream aborted` log sites fires. Reproduced deterministically at 9.83M artifacts — 111 s
and a truncation at the shipped deadline, 110.7 s and success at ten minutes, 4 ms on the second
pass — and it recurs after every fold that rebuilds a row form. Fail-closed at the client, silent at
the server. **The reproduction is in the probe's §8; the campaign wrote no engine code.**

**3. Nothing bounds a wide response over a large flat layer.** `artifact_budget` is accepted and
inert on a flat layer — the wire contract says so, a budget being met by serving ancestors — and at
`artifact_budget = 100` the same request still returns 254.6 MB. At 9.2 million served artifacts the
body is 954 MB and 2.8 s of the 12.5 s is Arrow encoding. Whether such a request should be
answerable at all is an owner question.

**4. The build's automatic memory budget does not bound its peak.** `tessera build` at
2.5×10⁸ points over this declaration, with no `--memory-budget`, was **OOM-killed at 47.3 GB after
24 minutes** on a 47 GB box. The derivation takes 80% of `MemAvailable` and the residency model it
feeds covers the batch loop's own structures; the real peak ran past it. `--memory-budget 12g`
builds. Recoverable and disclosing nothing — an operator sets the flag — but a build that is killed
rather than refused is the failure the pre-flight exists to prevent.

---

## What was not run, and why

**⊘ The 10⁹ tier: a disk refusal, recorded rather than attempted.** Inputs and bundle both scale
linearly and both are measured at 10⁷ — 1.46 GB and 1.21 GB — so at 10⁹ they are **146 GB and
121 GB**, and the build reads every input while writing the bundle. The transient requirement is
their sum, **267 GB, against 147 GB free**. Dropping the three member files leaves 214 GB;
`--no-oracle-pairs` leaves 192 GB. Nothing available brings it under, and starting a build that
would die two hours in with a full disk costs the tier twice and tells nobody anything. It is a
*different* wall from the one the probe campaign hit at the same corner — that one was memory, 45 GB
resident with all 12 GB of swap gone — and the two are independent.

**⊘ 10⁷ artifacts over 10⁹ points was therefore not reached either.** The artifact axis was reached
separately: a predicate over a keyed `u32` mints 9 832 352 artifacts over 10⁷ points, and that
probe is §7 of the record. Both axes large at once needs a larger machine, which is where the scale
investigation left it too.

**⊘ A whole-corpus principal is not expressible.** At the campaign's 1 048 576-term space one would
hold every term, and `/session/authorise` buffers `auth_data` under a 2 MB body limit — about
150 000 seven-digit descriptors. The broadest grant that fits is two whole term levels, **131 072
terms and 93.75% of the corpus**, and that is the campaign's broad rung. The design's §7 grid has a
100% row this campaign cannot ask for.

**⊘ And one measurement was thrown away.** The first concurrency sweep reported 4.5 rps at 128
sessions with the server at 1.6 of 12 cores. Both halves were the harness: a Python driver with a
thread per session, decoding every 11 MB artifacts frame into a hundred thousand tuples under the
GIL. It was caught by sampling the **driver's** own CPU beside the server's — pinned at 1.4 cores,
which is Python's ceiling — and the load arm was rewritten in Rust. Every load figure in this memo
comes from the Rust arm; the wrong ones are in the git history at `6631ec8`, deliberately.

---

## Acceptance, clause by clause

The delivery record's Stage 7 check: *"10⁹ points carrying 10⁶ artifacts, with 10⁷ as targeted
probes, serve inside budget for many concurrent principals, fold under load, and leave the write
path correct at every interleaving."*

| clause | verdict |
|---|---|
| **10⁹ points carrying 10⁶ artifacts** | **not met** — disk, quantified above. The largest tier reached is 2.5×10⁸ points carrying 2.5×10⁶ artifacts |
| **10⁷ artifacts as targeted probes** | **met on the artifact axis, not on both at once** — 9 832 352 artifacts over 10⁷ points, measured; a three-percent principal's whole-map request is 395 ms and a broad one's is 12.5 s |
| **serve inside budget for many concurrent principals** | **met at 10⁵ artifacts, not at 10⁷** — the envelope above; the one-core one-second budget holds at every 10⁷-tier cell post-fold and at no wide cell of the ceiling probe |
| **fold under load** | **met** — 0.98× the unloaded duration, artifact pass 0.99×, nothing shed. With the tail caveat above, which the clause does not cover and the record does |
| **leave the write path correct at every interleaving** | **met for what this campaign tests** — freshness exact on both membership kinds across ingest, flush and fold, the census exact after them. The constructed interleavings are the battery that landed on `artifacts/interleavings`, not this campaign |
| **the row-major counts match the generator census exactly** | **met** — 35 of 35 at 10⁷, including both row-major layouts after the fold |
| **the fold completes inside `plan_fold`'s memory ceiling under a live serving load** | **met** — `staircase_rss` 7.56 GB under 32 sessions, against the ~9–10 GB anonymous peak `plan_fold` budgets |

---

## Appendix R — review trail

| revision | date | what changed |
|---|---|---|
| r1 | 2026-08-22 | Created — the Stage 7 campaign's closing memo |
