# The performance suite for the per-item surface — design

**Date:** 2026-08-13
**Status:** **Provisional — under review, and not approved.** No arm named below exists and every
gate is a proposal; the ⊘ at each section head says so at the claim. **To become normative:** the
four questions in perf §10 ruled — the alignment flag as a Cargo profile decision, whether §6.4's
budgets bind an operand or a request, the 10⁹ tier's coupling to a release, and the fold's
window-occupancy threshold — and one adversarial review under a performance lens. The audit in
perf §2.2 is this document's load-bearing claim and the thing to attack first.
**Reads against:** [`records-and-search.md`](records-and-search.md) §3, §4.3, §6.2–§6.4, §7, §11
(**records §n**); [`measurement.md`](measurement.md) §2, §4–§8 (**measurement §n**);
[`filter-index.md`](filter-index.md) §2.2–§2.3, §5–§6 (**index §n**);
[`compaction.md`](compaction.md) §6, §9; [`write-path.md`](write-path.md) §2.3, §4.3;
[`conformance.md`](conformance.md) §6; decisions
[0013](../decisions/0013-mark-specified-vs-implemented.md),
[0052](../decisions/0052-the-folds-page-cache-mitigation-is-a-hint-not-a-throttle.md),
[0056](../decisions/0056-a-folds-schedule-is-a-gated-window-not-a-pure-timer.md),
[0063](../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
[0064](../decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md),
[0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md),
[0068](../decisions/0068-a-row-space-operand-bounded-by-the-requests-domain-is-admitted.md).
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **perf §n**.

**Owns:** what the per-item data surface must measure — the record blob, the row-space filter
route, the presence bitmap and the keyword family — at what scales, against which budgets, and what
happens when a budget is crossed. **Does not own:** the suite's conventions and denominators, which
are [`measurement.md`](measurement.md)'s and are inherited rather than restated; how to run
anything ([`bench/README.md`](../../bench/README.md)); what any run found (the arms' module docs
and `docs/evidence/memos/`); or correctness ([`conformance.md`](conformance.md)).

---

## 1. Summary

The per-item surface — the record blob, the row-space filter route over render columns, the presence
bitmap for absent numbers, and the keyword family — is measured for **storage** and barely for
speed. Bytes per entity, compression ratios and a dictionary's bytes per key are settled; the
performance figures are a handful of **scan constants** taken by bespoke probe binaries against
library entry points. Nothing has measured a flush, a coalesce or a fold for the new families;
nothing has measured how any of it moves with the number of live extents; and no figure for any of
it has ever been taken through a request.

This suite is organised around one existing artefact rather than around a list of arms.
**`records-and-search.md` §6.4 is a budget table — eleven rows of operator × shape → route → cost
at 10⁹ → budget met — and it is the suite's index.** Every row either names the arm that produces
it or says why no arm can and what stands in its place. Perf §2.2 audits all eleven, and the result
is the reason this document exists: **two rows have a figure taken from built engine code, none has
one taken through a request, and none has one at 10⁹**, because no 10⁹ fixture carrying an
attribute tail has ever been built.

Three things follow, and they are the design.

- **A measurement has a *level*, and the level decides what it may discharge** (perf §2.1). A
  constant timed at a library entry point does not meet a request budget; it becomes one by
  composition with a **measured** envelope, which is itself checked for being an addition rather
  than a factor.
- **Layer count is an axis, not a constant** (perf §3.2). A keyword query resolves per layer, a
  blob read locates its extent, and the fold collapses them — and nothing has swept it. The
  corpus has already been wrong about a layer-count expectation once, in the harmless direction
  (measurement §2), which is an argument for measuring this one rather than for assuming it
  inherits that result.
- **The write side is a deadline, not a latency** (perf §4). A flush that no longer fits its
  interval and a fold that no longer fits its window are the two failures an operator needs a
  number for, and neither has one.

The suite fails a run, not merely reports on it. The gate that matters most is the cheapest:
**a §6.4 row marked *measured* must have a cell in the run that produced it** (perf §7, G7). That
makes the corpus's own distinction between measured, modelled and assumed a machine-checked
property at the one place it is written down.

---

## 2. The budget table is the index

### 2.1 Three levels, and what discharges a request budget

Every figure this corpus quotes about the per-item surface sits at one of three levels, and the
corpus does not currently say which:

| level | what runs | example | what it can support |
|---|---|---|---|
| **component** | a library entry point, timed directly | `dictbytes`' `resolve`, `realscan`'s per-candidate constant | a per-unit constant, an A/B of two implementations |
| **engine** | one `Engine::viewport` call in process, stage-timed | `row_route_cost` reading `filter_cross_ns` | an operand's cost inside a real route, under the real candidate |
| **request** | over the wire through `tessera-server` | the `load` arm | a §6.4 budget |

**§6.4's budgets are request budgets.** The 0.5–1 s figure is index §2.2's owner ruling for a
filter and the 100 ms figure is records-and-search's interaction target; both are what a viewer
waits, not what a loop costs. So the composition rule, which is the honest answer to
component-versus-end-to-end:

> A §6.4 row is **met** when `engine figure + envelope ≤ budget`, where the **envelope** is the
> measured cost of everything in the request that is not the operand — parse, admission, mask
> composition, projection, serialisation. A component figure never discharges a row on its own; it
> supports an engine figure and explains it.

The envelope is measured once per family at one operating point, dated, and re-measured when the
handler, the compute-admission gate or the wire format moves. **The claim that it is a constant is
itself checked**: it is taken at two operand costs an order of magnitude apart, and if it moves
with the operand it is not an envelope and every row goes over the wire instead. That check is
cheap and it is the difference between a composition rule and an assumption.

One accounting fact the rule depends on, stated here because the code does not show it and a
reader would reasonably fear the opposite. The crossing stage — which for a row-routed leaf is the
whole of `evaluate_row_route`, including its internal `par_iter` — is lapped on the **request
thread**, so `filter_cross_ns` is wall clock and composes with the other stages. The *per-tile*
counters are different, and `bench/README.md` §8 records why: above one compute thread they become
cross-worker CPU sums whose total may exceed the request's wall time. **Filter figures compose;
per-tile figures do not.** An arm that adds the two has produced a number with no meaning.

### 2.2 The eleven rows, audited

⊘ **Nothing in the "discharged by" column exists.** The "today" columns are the current state, read
off the design and its cited evidence.

| §6.4 row | today's mark | provenance | level | scale | discharged by |
|---|---|---|---|---|---|
| 1 category/keyword `eq`, selective | measured | index §2.3's postings arms; the category half also 15–26 µs on built code | component (+ engine, category) | 10⁹ probe / 10⁸ engine | `filter`, category now; keyword waits on 0067's postings |
| 2 text `match`, common tokens | modelled | none — family unbuilt | — | — | **nothing** (perf §2.3) |
| 3 any viewport-bounded filter | probe-measured / modelled | 0.89–1.05 ms measured on built code, 1–2 byte codes only | engine | 10⁸ | `filter`, over width and run count (perf §3.3) |
| 4 number range, contiguous | measured | index §2.2's constant × 10⁹, layout-conditioned | component | 10⁸→10⁹ | `filter`, engine level, alignment declared |
| 5 keyword `eq`/`prefix`, no postings | modelled from a measured constant | index §2.2's fixed-width constant; family unbuilt | component | 10⁸ | `filter`, once the ordinal scan lands; fence first |
| 6 fixed-width scan, scattered 25% | measured at 10⁸ ×10 | index §2.2's scattered constant | component | 10⁸ ×10 | `filter` at 10⁸; the ×10 stays modelled to the 10⁹ tier |
| 7 keyword `contains`, broad | per-key at 2.4M, extrapolated ×400 | the dictionary campaign, which flags it | component | 2.4M | the promoted dictionary arm at ≥10⁸ keys — **extrapolation refused** (perf §5) |
| 8 phrase verify, selective | 169 µs/block measured | **stale**: the built reader measures 253–265 µs | component | 2.4M | `record` for the block half; phrase waits on text |
| 9 phrase verify, common | unbounded, result-bound | — | — | — | not a figure; the suite reports the class boundary (perf §3.4) |
| 10 CSR list scan, broad | measured constants ×10⁹ | index §2.2; lists unbuilt | component | — | waits on records §13 item 4 |
| 11 unselective predicate | measured, result-bound | index §2.2's 3.4–6.0 s and its three-term decomposition | component | 10⁹ | `filter` confirms the decomposition survives in the engine; the gap is not closed |

**The tally, stated plainly.** Of eleven rows: **five describe machinery that does not exist**
(2, 5, 7, 8's phrase half, 10); **six carry a figure from a probe harness rather than from built
code**; **two carry an engine-level figure** (1's category half, 3's row-space half) and both were
taken at 10⁸; **none carries a request-level figure**; **none carries a figure at 10⁹ on built
code**. The largest bench fixture with an attribute tail is 10⁸, and `data/bench-fixtures/1e9`
carries `declared_scalars = []`.

A probe measuring an approach before the code exists is the right order, so this is a state rather
than a failing. But a row that reads *measured* without saying at which level and at what scale
invites exactly the misreading perf §6's alignment rule exists to prevent.

### 2.3 The rows nothing can discharge, and what stands in their place

**Row 2 (text `match`) and the phrase half of row 8 cannot be measured, and will not be until the
text family exists.** Saying so is the whole obligation; the useful part is what the modelled
figure rests on. The text postings route is the *same construction* the category accelerator takes
— `postings ∩ candidate`, whose cost is O(containers touched) and which index §2.3 measured at
3.56 ms at 25% coverage and 49.5 ms in its worst cell at 10⁹. So the model's **shape** is borrowed
from a measured one and only the **term count** is modelled: `match` over *k* common tokens is *k*
intersections plus a union. That is why "tens–hundreds of ms" is a defensible model and not a
guess, and it is also why the measurement that discharges it is a term-count sweep rather than a
new cost model.

**Row 9 is not a budget and must not be given one.** A phrase common enough to match a large
fraction of the candidate is result-bound, which is index §2.2's known gap restated. The suite's
job there is to report the **class boundary** — the result cardinality at which the row leaves the
budget — so an operator can recognise the shape, not to publish a latency that depends on the
question asked.

**Two figures §6.4 carries that its own evidence has moved past**, listed because an unlisted
falsified claim is the kind someone later "fixes" in the wrong direction:

- Row 8 quotes **169 µs/block measured**. Through the built reader a random single-row blob read
  measures **253–265 µs**; the 169 µs was a decompression rate, not a read latency, and §3 has
  already been re-marked while §6.4 has not.
- The `÷ cores` in the "meets" column of rows 6 and 7 is **refuted as shorthand**: the measured
  divisor is 7.4–8.5× on twelve cores at 10⁸ and 2.8–3.6× at 2.4M. A parallel remedy is a measured
  divisor at the scale claimed or it is not a remedy.

Both are amendments §6.4 owes; neither is made here, this document owning no part of that file.

---

## 3. The read side

⊘ **No arm below exists.** Two are extensions of arms measurement §7 already declares and which are
themselves unbuilt; one is a promotion of an existing probe harness.

**New:** `record` — the blob's drill-down read, over block occupancy and extent count.
**Extended:** `filter` (measurement §7) gains family × operator × route × column width × candidate
shape × selectivity × layer count; `load` gains one filter operating point per family, which is
what calibrates perf §2.1's envelope. **Promoted:** the `dictbytes` harness becomes the dictionary
arm, records §11 item 4 needing it at ≥10⁸ keys and a probe tree being the wrong home for a
standing measurement.

### 3.1 The axes, and which are new

| axis | values | why |
|---|---|---|
| scale | 2.4M, 25M, 10⁸, 10⁹ | perf §5 |
| candidate shape | contiguous, scattered | the two constants differ by ~34× (index §2.2) |
| coverage | the principal's share | measurement §6 — a figure without it is half of C4 |
| selectivity | the share of the candidate the predicate matches | the axis §6.2's route rule structurally cannot see (#100), and where rows 4 and 11 diverge |
| **column width** | 1, 2, 4, 8 bytes | **new** — perf §3.3 |
| **run count** | runs in the domain, held apart from rows in it | **new** — perf §3.3 |
| **layer count** | live extents per column; live segments per slice | **new** — perf §3.2 |
| threads | 1 and 12, divisor measured | `÷ cores` is refuted (perf §2.3) |
| vocabulary shape | distinct keys, prefix sharing | a prefix-free key set front-codes to nearly its raw bytes; **no such column exists in this corpus** — a fixture gap, not a measurement gap |

The three new axes are new because the existing measurement varied corpus size and viewport size
and held everything else still. That was the right first sweep; it is not the axis a running
deployment moves along.

### 3.2 Layer count, as the axis a deployment actually moves along

A column's live extents grow with every flush and are bounded by the coalesce between folds
(index §5.2). Cost meets that count in three places, and the design models none of them:

- **A keyword operand resolves the needle per layer** and scans each layer's ordinal column. The
  scans partition the entities (I9), so the scan half should be **invariant** in the layer count
  while the resolve half is **linear** in it. Resolve measures ~1 µs — flat in the restart interval,
  being seventeen random accesses over a 10 MB structure — which is invisible at one layer and is a
  millisecond at a thousand.
- **A blob read locates its extent**, each extent carrying its own has-row bitmap, directory and
  blocks. The read is one block; finding which extent holds the entity is not obviously O(1) in the
  extent count, and records §3 does not say what it is.
- **The fold collapses them**, so the fold's input volume is the sum over layers while the
  *coalesce*'s work is a function of how many there are (perf §4.2).

**The hypothesis the arm exists to falsify, stated so a negative result is reportable: scan work is
invariant in layer count and per-request setup is linear in it.** ⊘ NOT confirmed — nothing has
measured it. If the scan half rises with the layer count too, the layered composition is doing work
the design does not describe — re-deriving presence, re-clipping the candidate per layer — and that
is a defect to fix rather than a curve to publish.

There is a precedent for measuring this rather than modelling it, and it cuts both ways. The
fragment build unions across every live delta postings tier and was **measured flat** in tier count
— 199 ms at one tier, 198 ms at 512 — against the seconds the write-path design had modelled
(measurement §2). So a layer-count expectation in this corpus has already been wrong once, in the
harmless direction. The keyword family's per-layer *resolve* is a different mechanism from that
union and inherits nothing from the result; the flatness there is the reason to sweep this, not the
reason to skip it.

The sweep runs from one layer to beyond the coalesce bound, the unbounded arm being the control —
the same construction `crates/tessera-engine/tests/soak.rs` already uses, where forty flushes leave
two segments with maintenance running and one per flush with it stopped. Synthesised extents make
this a nightly-affordable cell; a real soak is a release campaign (perf §8).

### 3.3 The row route: eleven widths built, two measured, presence never

The row-space route is built for every fixed width — `Bool`, `U8`…`U64`, `I8`…`I64`, `F32`, `F64`,
`TimestampUs` — refusing anything else as a malformed bundle. **Every published constant for it was
taken on `u8` and `u16` category codes**, and the memo that took them says so in terms: *"Do not
carry these constants onto a rendered number."* At 10⁹ an `i64` column is 8 GB against the 1 GB of
`u8` those constants describe, and the memo's own finding — that `u16` measured within a few
percent of `u8` in *both* directions — establishes only that the scan is not bandwidth-bound at one
and two bytes on this machine. It says nothing about eight, and the coarse-zoom cell §6.2 prices is
ultimately the eight-byte one.

**Absence has never been timed at all.** Decision 0064's presence bitmap is what let numbers,
datetimes and bools join the route, and its cost has a shape worth stating because it decides what
the arm varies: the bitmap is shifted into slice row space **once per segment** and intersected
**once per run**, outside the row loop. So presence should cost per run and per segment, not per
row — O(containers touched), the corpus's own cost model — and its price is therefore invisible in
a sweep that varies rows while holding the run count still. **Which is exactly what every
measurement so far has done.** A scattered principal is a domain of many short runs, and it is the
principal for whom this is not free.

Hence two axes rather than one: **width**, because the built surface is four times wider than the
measured one; and **run count held apart from row count**, because that is where the presence
intersection and the per-run predicate hoist both land. The hoist is the other reason: deciding the
width and the predicate once per contiguous run is what recovered 6.5–10.9× and produced the
0.22–0.45 ns constant, so a domain of very short runs pays that decision more often and the
constant should degrade toward the pre-hoist 2.5–3.4 ns. ⊘ Modelled — the run-count sweep is what
would show where between the two it lands.

### 3.4 What the arms report per row of §6.4

Each cell carries, beside measurement §8's existing `work` block: the route taken (`entity` /
`row`, from the engine's own counter), `rows_in_ranges`, the run count, the live layer count, the
column's width, the matched cardinality, and the alignment state of the binary (perf §6). A cell
that cannot name its route has measured a request, not an operand, and is reported as such rather
than attributed to a §6.4 row.

For rows 9 and 11 — the two result-bound classes — the arm sweeps the result cardinality and
reports the **crossing point**: the matched count at which the row leaves its budget. That is a
property of the system; the latency at any one selectivity is a property of the question.

---

## 4. The write side

⊘ **Entirely unmeasured, for every new family.** Records §11 item 7 owes it and records §7 prices
it — *"every figure modelled, per decision 0013"*. This is the largest gap in the surface, and the
three passes fail in different ways, so they get different numbers.

### 4.1 Flush is a duty cycle, not a latency

A keyword extent sorts and front-codes its batch, and the blob extent compresses; both run at flush
execution on the pool rather than in the serial group-commit section, so neither is in a caller's
acknowledgement latency. What they can do instead is **fail to keep up**: if a flush takes longer
than the interval between flushes, the buffer grows without bound and the ack→visible gap stops
being `flush_max_age_secs`, which is the write path's own bound and the thing measurement §6
publishes.

The figure is therefore the **duty cycle** — flush wall clock ÷ flush interval — reported per
family and per column, swept over the ingest rate and the batch shape `ingest-rate` already sweeps.
Above 1 the system is falling behind; the gate fires well below it (perf §7). The per-family
decomposition matters because the families differ by construction: a fixed-width extent is an
append, a keyword extent is a sort, and a blob extent is a compressor.

### 4.2 Coalesce, and the guard that nobody has priced

The keyword coalesce merges the windows' dictionaries, remaps each input's ordinals through the
merged dictionary, and merges postings per term, bounded by the existing 256 MiB input cap. Records
§7 models it as *"sub-second per window at memory-bandwidth-class merge rates"*, which is a claim
about the merge.

**The unpriced term is the content guard, and its cost depends entirely on how it is written.** The
merge verifies `merged_dict[remap[i]] == input_dict[i]` for every input key — O(keys) as stated,
and the check that makes an ordinal recolouring refuse instead of publishing. Written as a walk
over two sorted key sets in step, it is a linear scan and it disappears into the merge that
produced the remap. Written as a per-key *lookup* into the merged dictionary, it is a resolve per
key at the ~1 µs the dictionary campaign measured — a second per million keys, on a pass modelled
at sub-second per window. That is a difference of orders on the merge's total, decided by an
implementation choice no measurement would otherwise notice, and it is the reason the coalesce arm
reports the guard as **its own line** rather than inside the merge.

Reported: MB/s through the pass, decomposed into dictionary merge, ordinal remap, guard, postings
merge, and — for the blob — block repacking through zstd. Swept over window size and over the
number of extents in the window, which is perf §3.2's axis on the write side.

### 4.3 The fold is a deadline, and this is the operator's number

The fold runs on its own thread inside a nightly gated window (decisions 0056, 0057). Its cost to a
resident session is measurement §2.3's business — the flip, as an interval with a population — and
this document does not restate it. What is new here is that the fold's **duration** grows with the
new families: records §7 models a keyword fold inside index §6.2's ~12 GB/column streaming envelope,
and a text fold as the expensive one, decompressing and recompressing ~76 GB of title-shaped bytes
for order three to five minutes of CPU per column, and tens of minutes for an abstract-shaped one.

The owner's question — what number tells an operator it no longer fits — has a specific answer, and
it is not the duration. It is **window occupancy: fold wall clock ÷ the gated window's length**, per
column and in total.

Three properties make that the right quantity rather than a duration:

- A fold that overruns its window does not fail; it runs into the day, and the harm is the IO
  contention P3 measured (up to 2.03× on a concurrent viewport at a bundle larger than RAM). The
  mitigation is `MADV_SEQUENTIAL` and there is **no throttle** (decision 0052), so there is no lever
  to pull once it overruns — which is why the number has to warn early rather than report late.
- Slower is gentler (compaction §6.1), so a fold that takes longer at the *same* occupancy is not a
  regression. Occupancy separates "the fold got slower" from "the fold got bigger", and only the
  second is an operator's problem.
- A run can legitimately contain **no fold** — the schedule is a gated window, not a timer — so the
  fold count is an outcome to report and the arm needs a forced-fold mode to be deterministic. That
  requirement is measurement §7's and this arm inherits it rather than inventing a second one.

Reported per fold: occupancy, the per-column and per-family decomposition, bytes in and out, and
whether on-disc bytes fell — the last being measurement §8's G6, which this document does not
duplicate.

### 4.4 The cheapest measurement in this document, and the largest term it closes

Records §7's text-fold model has exactly one **assumed** term: zstd compression at 0.4–0.8 GB/s,
against a decompression rate of 1.5–1.7 GB/s that is measured. That single assumption sets the
whole three-to-five-minutes-per-column figure, and a factor of two in it is a factor of two in the
fold's headline cost.

It is measurable **today**, on built code, without the text family existing: the shipped
`RecordBlobWriter` at the shipped level and the shipped 256 KiB target already writes real records,
and the epic-1 campaign ran it over 2,400,000 arXiv rows to produce compression *ratios* while
never recording a *rate*. One number, minutes of machine time, turning the fold model's only
assumed term into a measured one. It is the first thing this suite should run and the only item
here that needs nothing built first.

---

## 5. Scale, fixtures, and what may be extrapolated

Fixtures exist at 2,422,486, 25,000,000 and 100,000,000 items. The 10⁹ tier has **never been
built**: roughly 20–40 minutes and ~23 GB for the points file, 30–45 minutes and ~62 GB for the
bundle, with free space the binding constraint at ~54 GB — so building it means **clearing the
other three first**, not adding to them. Build costs for the attribute fixtures are 5.3 s at 2.4M,
74 s at 25M and 4m36 at 10⁸ render-only.

**The suite does not build a 10⁹ tier of its own.** Records §12's dataset ruling already places one
in the epic order — full-schema, built once, after the string families land — and a second would be
a second thing to keep current. The suite rides it and states, per figure, that it is waiting.

Two consequences follow from the free-space constraint and are worth stating because they change
how a campaign is run. A 10⁹ campaign **cannot hold a 10⁸ baseline on disc beside it**, so
baselines are recorded before the tier is built or the comparison is lost. And the 10⁹ bundle at
~62 GB against 47 GB of RAM is past this machine's residency boundary, which measurement §2.4
establishes is reachable here — so a 10⁹ figure is taken under continuous reclaim and is not
comparable to a 10⁸ figure taken warm, whatever the per-row constant says.

**The extrapolation rule**, which is what makes the difference between an honest ×10 and a wrong
one:

> A per-unit constant may be extrapolated **one decade** if it has been shown flat across **two**,
> and only with the regimes it does not cover named. A structure that crosses a **cache, RAM or
> disc** boundary between the measured scale and the claimed one is measured or it is not claimed.

The row-route constants are the model of the first clause: flat within 10% from 25M to 10⁸, with
the extrapolation's own gaps named — TLB pressure and twelve threads contending over a gigabyte,
both of which can only push the figure up. That is what an extrapolated row should look like.

Row 7 is the model of the second clause, and it is the one row where **extrapolation is refused**.
The keyword dictionary at 2.4M keys is ~10 MB and fits in this machine's 32 MiB L3; at 10⁸ keys it
does not. The per-key decode measured 11.0–18.8 ns *inside cache*, and multiplying that by 400 to
reach 11–19 s at 10⁹ crosses the boundary that decides the constant. The probe that produced it
says so itself and flags it rather than claiming it. §6.4's row 7 is therefore a **flag, not a
figure**, until the dictionary arm runs at ≥10⁸ keys — and the 2–10 s the design originally modelled
must not be quoted as measured either.

One fixture the corpus does not have and cannot synthesise honestly: a **prefix-free key column**
— UUIDs, hashes — which front-codes to nearly its raw bytes and makes the keyword layout merely tie
the flat column it replaces. Every keyword storage and decode figure in this corpus is arXiv-shaped.
Adding such a column is a fixture change, not a measurement, and it is the cheapest way to find the
keyword family's worst case rather than its typical one.

---

## 6. How a figure is reported so it is not a lie

Measurement §4's two modes are inherited unchanged: **A/B mode** (`min_ns` over 3–5 repetitions,
normalised) for comparing implementations, **distribution mode** (duration-bounded, N ≥ 1,000) for
anything user-facing, with `p99_ns` suppressed below 100 samples. Component constants are A/B;
§6.4's request budgets are distribution mode. What follows is what this surface adds.

**Alignment is the reporting problem here, not noise.** This repo's scan constants are **bimodal in
instruction-address alignment**: a pure 16-byte shift of instruction-for-instruction identical code
moves the fixed-width constant between ~0.25–0.28 ns and ~0.42–0.44 ns with period 64 — a **64–68%
swing** — and nothing in between. Every per-row and per-entity constant on this surface is a tight
compute-bound loop of exactly the affected kind. Two consequences bind every figure in this suite:

- **Every cell declares its alignment state.** Either the binary was built with function alignment
  pinned, or the figure is reported as the bimodal pair rather than as a point. ⊘ The pinning flag
  is verified to work on the probe binary and has **not** been applied to the workspace profile or
  re-run against `tessera-server`; that is perf §10's first question, and until it is ruled the
  suite reports pairs.
- **An unpinned scan-surface cell is excluded from gating rather than gated with a wider band.**
  G1's +15% is meaningless against a constant that is bimodal at ±65%, and widening the band to
  ~70% would hide every regression worth catching. The cell carries an `alignment_unpinned` flag
  and is excluded from G1 exactly as `low_container_resolution` cells already are — reported,
  never gated. Pinning is what restores G1 over this surface, which is what makes perf §10's first
  question a gate question rather than a tidiness one.

Crate boundaries do not substitute for pinning: they re-roll the layout rather than fixing it,
which is why an earlier attempt appeared to recover "about half".

**A campaign that ran on a busy machine is void, and the suite detects it rather than trusting the
operator.** Under load average 12 the *same binary* measured 0.28, 0.46 and 0.49 ns on three
consecutive rounds — a 1.75× spread with the code held fixed, against the 65% effect it would have
been used to detect. So every campaign opens and closes with a **canary cell**: one fixed, cheap,
well-characterised measurement. If the two disagree by more than a stated band, the run is void in
the same sense G0 already makes a run void — the numbers are not wrong, they are not comparable.
Interleaved A/B remains the discipline for any pair being compared; it catches drift because both
arms degrade together.

**Threads are stated, and the divisor is measured.** No figure carries `÷ cores`; it carries the
measured speedup at the scale it was measured at, which was 7.4–8.5× on twelve cores at 10⁸ and
2.8–3.6× at 2.4M.

**Marking, per decision 0013, on every cell:** *measured* / *modelled* / *assumed*, plus the
**level** (component / engine / request), the **scale**, and the alignment state. A figure that
cannot state all four is not published. The distinction is not decoration — perf §2.2 exists
because the corpus currently marks a probe-harness constant multiplied by ten as *measured*, which
is true of the constant and not of the row.

---

## 7. What makes the suite fail

A suite that only produces numbers is a dashboard. Gates G0–G3 (`bench/README.md` §7) and G4–G6
(measurement §8) are inherited; G1 gains one exclusion (perf §6, `alignment_unpinned`). Three are
added, and they fail different things:

| gate | checks | class | on failure |
|---|---|---|---|
| **G7** | every §6.4 row marked *measured* has a cell in this run that produced it, at its stated level and scale | **corpus** | the design's own honesty marking is wrong — the fix is an edit to §6.4, not to the code |
| **G8** | flush duty cycle and fold window occupancy below their configured fractions | assertion about the system | the write side no longer fits its schedule; fails the run |
| **G9** | the keyword family is no slower than the `utf8` scan it replaces, at the operators both support | regression fence | the replacement is a regression and `utf8` must not be deleted yet |

**G7 is the gate that turns this suite from a dashboard into a gate**, and it is nearly free. §6.4
is prose today, so a row can read *measured* indefinitely after the measurement that justified it
was superseded — which is how row 8 still quotes 169 µs against a built reader that measures
253–265 µs. Making the table a checked artefact means the marking cannot rot silently. It fails the
documentation rather than the build, which is the correct target: the code is not wrong, the claim
about it is.

**G9's figures are not this document's.** A parallel track is measuring the `utf8`-versus-keyword
fence now, and the fence has a hard ordering property: it must be captured **before** `utf8` is
deleted, because afterwards there is no A/B to run and the fence degrades into a recorded baseline.
Its figures slot into §6.4's rows 5 and 7 and into this suite as G9's baseline; nothing here
duplicates the measurement.

**Three failure classes, kept apart**, because conflating them is how a gate gets ignored: a **void
run** (G0, the canary) means no comparison is possible and nothing is reported; a **regression**
(G1–G4, G9) means a number moved and a person decides; an **assertion about the system** (G5, G6,
G8) means a property the design claims does not hold, and it fails the run rather than reporting a
percentage. G7 belongs to none of the three: it fails a document.

---

## 8. Cost, and what runs when

A suite nobody runs because it takes six hours does not exist. Three tiers, matching conformance
§6's own tiers so there is one schedule rather than two.

| tier | what runs | scale | wall clock |
|---|---|---|---|
| **per change** | component A/B on the scan and dictionary constants; the `filter` arm's operand cells; the blob write and read rates | 2.4M | minutes — **bounded by `cargo build`, not by measurement** |
| **nightly** | the above at 25M and 10⁸; the width and run-count sweeps; the layer sweep over synthesised extents; a bounded coalesce; G7 | 25M, 10⁸ | ~1 hour, fixture builds included (74 s and 4m36) |
| **release** | the wire envelope; a real soak with a forced fold; the fold at scale; the 10⁹ tier when the dataset stage builds it | 10⁹ | a half-day of machine, not of person |

Two things make the per-change tier affordable and they are worth stating because they are the
reason it belongs in the per-PR gate at all: the 2.4M attribute fixture builds in **5.3 s**, so no
fixture is checked in or kept warm; and a 2.4M filter cell is single-digit milliseconds, so a
couple of hundred cells is seconds. The measurement is not the cost — the build is.

The layer sweep is nightly rather than per-change because it needs a live engine performing
flushes, and it is nightly rather than release because **synthesised extents** stand in for a real
soak. The real soak stays in the release tier; it is measurement §2.2's arm and this document adds
only the per-item columns to it.

⊘ The nightly and release tiers of conformance §6 **do not exist**; only the per-PR tier does. This
suite's nightly and release tiers land with them or not at all — a nightly running one seed batch
is the per-PR gate with a worse schedule, and the same is true here.

---

## 9. What this deliberately does not do

- **No new corpus.** Every axis is constructible on the existing fixtures plus the dataset stage's
  own tiers. The one fixture gap named is a prefix-free key column (perf §5), which is a schema
  change rather than a corpus.
- **No replacement for the correctness gate.** A filter that stays fast while resurrecting a
  suppressed entity passes every gate here. `conformance.md` owns that, and the composed-verdict
  rule (§6) is not a performance property.
- **No claim over the wire that was not taken over the wire.** Perf §2.1's envelope makes an
  engine-level figure into a request-level claim by a measured addition, and the moment the addition
  stops being constant the rule is void rather than adjusted.
- **No 10⁹ keyword dictionary figure by extrapolation** (perf §5). The structure crosses L3 between
  the measured scale and the claimed one.
- **No per-user model.** Measurement §6.1 owns the trajectory and think-time question, and its
  parameters are assumed rather than measured because this project has no telemetry. Nothing here
  adds a second one.
- **No pricing of the fold's flip.** Measurement §2.3 owns it — an interval with a population, not a
  duration. This document prices the fold's *duration and its per-family decomposition* only.
- **No throttle, and no figure implying one.** Decision 0052 refuted the mechanism; a fold that
  overruns its window has no lever, which is why perf §4.3's number warns early.

---

## 10. Open questions — for the owner

Four, each rulable without opening a source file.

1. **Is function alignment pinned in the workspace profile?** Today the workspace sets no alignment
   flag, so every scan constant in this corpus is one draw from a bimodal distribution and *not
   known to be the good draw*. `-C llvm-args=-align-all-functions=6` was measured to remove the
   sensitivity at no measurable baseline cost and a 0.14% binary growth, but only on a probe binary
   — the memo that established it says applying it to the workspace profile is an owner decision.
   **Recommendation: pin it**, and re-run one arm against `tessera-server`'s own link to confirm the
   property transfers. Cost if wrong: a profile flag to revert. Cost of not ruling: every per-row
   figure in this suite is published as a bimodal pair, and **every scan-surface cell is excluded
   from G1** — so the surface this suite exists to protect is the one part of the system with no
   regression gate over it.
2. **Do §6.4's budgets bind an operand or a request?** Decision 0062 composes filters as a boolean
   tree, so a request may carry several leaves. Three leaves at row 4's ~250–280 ms are inside the
   1 s budget individually and outside it together. The design does not say which reading is
   intended, and the two lead to different arms: a per-operand budget is swept over operators, a
   per-request budget is swept over **tree shape**, which nothing currently measures.
   **Recommendation: per request**, with a per-operand figure reported beneath it — a viewer waits
   for the request. Cost if wrong: the tree-shape axis is measured and unused.
3. **Does a release before the dataset stage ship on 10⁸ plus extrapolation?** Records §12 places
   the 10⁹ tier after the string families, and this suite rides it rather than building its own. So
   until then, no §6.4 row has a measured figure at the scale it is stated at.
   **Recommendation: yes, explicitly** — every §6.4 row carries its scale and its extrapolation
   basis under perf §5's rule, which is a stronger position than an unmarked table today. Cost if
   wrong: the tier is built early, at ~62 GB and the loss of the other fixtures.
4. **What fraction of the nightly window is the fold's gate?** Perf §4.3 makes occupancy the
   operator's number; the threshold is a policy this document should not set. **Recommendation:
   0.5**, so the warning arrives a doubling before the overrun rather than at it, given there is no
   throttle to apply once it overruns. Cost if wrong: a gate that fires on a healthy system, or one
   that fires too late to act on.

---

## Appendix R — review record

**Drafted 2026-08-13.** Not yet reviewed.

The draft's spine is the audit in perf §2.2, taken by reading §6.4's eleven rows back to the
evidence each cites. Three findings came out of that reading rather than out of the brief, and each
is the reason a section exists: the corpus does not distinguish a constant timed at a library entry
point from one taken through the engine, while §6.4's budgets are request budgets (perf §2.1); the
row-space route is built for eleven fixed widths and measured on two, with decision 0064's presence
bitmap — whose cost lands per run and per segment rather than per row — never timed at all
(perf §3.3); and records §7's text-fold model has exactly one assumed term, the compression rate,
which is measurable today on the shipped blob writer without the text family existing (perf §4.4).

Two stale figures in §6.4 are recorded in perf §2.3 rather than corrected, this document owning no
part of that file: row 8's 169 µs/block against the built reader's measured 253–265 µs, and the
`÷ cores` shorthand against a measured 7.4–8.5× on twelve.

One expectation is carried as a falsifiable hypothesis rather than as a claim, because the corpus
has been wrong about a neighbouring one: scan work invariant in layer count, setup linear in it
(perf §3.2). The fragment build was modelled as growing with tier count and measured flat, so the
sweep is specified to be capable of returning a negative result and reporting it.
