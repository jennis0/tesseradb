# Filter column layout — what it costs to know which entities a column covers

**Date:** 2026-08-08 · **Harness:** [`layoutprobe/`](layoutprobe/) · **Machine:** WSL2, 12 cores,
47 GB RAM, single-threaded

The value array is identical across every layout under consideration, so the whole question is the
**addressing structure** beside it: how a scan learns which entities a filter column covers, and
where each covered entity's value sits.

## Results

**Two constants govern the masked scan, and they are stable across three orders of magnitude.**

| | ns per candidate entity | Measured at |
|---|---|---|
| Contiguous candidate, direct-indexed values | **~2.9** | 2.87 (1e9, 1% contiguous), 2.92 (1e9, 25%), 2.56 (1e8) |
| Scattered candidate, direct-indexed values | **~22** | 22.1 (1e9), 24.1 (1e8) |

The 7.6× penalty is random access into the value array — cache misses, not bandwidth. **This
refutes the modelling that preceded it:** the 2026-08-08 structures memo assumed 5–10 GB/s and
derived 40–80 ms for a 25% principal at 10⁹. Measured is **730 ms**, an effective ~1.4 GB/s. The
scan is bound by per-candidate work, not by memory bandwidth, and any sizing that treats it as a
bandwidth problem will be ~5–7× optimistic.

**Addressing bytes per present entity** (1e9; totals in parentheses where sub-byte):

| Presence shape | bare | runs | roaring | pairs |
|---|---|---|---|---|
| Every entity present | **0** | 0.0000 (12 B) | 0.0002 (216 KB) | 4.00 (4.0 GB) |
| Slice-blocked, 10 slices | n/a | 0.0001 (12 KB) | 0.0004 (36 KB) | 4.00 (400 MB) |
| Scattered, 10% | n/a | 10.80 (1.08 GB) | **1.25** (125 MB) | 4.00 (400 MB) |

**Scan latency, ms, at 1e9:**

| Presence / candidate | bare | runs | roaring | pairs |
|---|---|---|---|---|
| all / 1% contiguous | **28.7** | 35.0 | 1078 | 279 |
| all / 25% broad | **730** | 939 | 1518 | 870 |
| all / 1% scattered | **221** | 259 | 3684 | 1413 |
| slices / 1% contiguous | n/a | 113.8 | **109.0** | 56.6 |
| slices / 25% broad | n/a | 2873 | **161** | 800 |
| scattered / 25% broad | n/a | 10204 | **232** | 881 |

Every layout is **linear in *n*** from 1e6 to 1e9 (9.5–11.2× per decade). No cliffs, and the
ranking never changes — the choice can be made once and does not need re-deriving at scale.

## What to build

- **Presence universal → a bare value array**, entity id as the index. Free, and fastest in every
  candidate shape measured. Nothing else is competitive when it is legal.
- **Presence partial → a Roaring presence bitmap** with values stored compactly. A few tens of KB
  for slice-blocked presence, 1.25 B/present when genuinely scattered, and the only layout that
  does not degrade on a broad candidate.
- **Explicit `(entity_id, value)` pairs → never optimal**, on either axis, at any scale or presence
  shape measured. Its case is simplicity, not cost. It costs 4 B/entity **per column**: at 1e9 with
  16 filter columns, 64 GB of pure redundancy against Appendix A's ~20 GB term-index budget.

**Run tables are a trap, and this is the finding most likely to be re-derived wrongly.** A
`(start, len, base_rank)` table is unbeatable on storage — 12 bytes for an entire 1e9 column — and
that number alone recommends it. It collapses on broad candidates, because rank costs a binary
search *per candidate entity*: 2.9 s slice-blocked and 10.2 s scattered at 1e9, against Roaring's
161 ms and 232 ms. **Do not choose an addressing structure on its storage column.**

## Where the masked scan stops being affordable

**The budget is a filter budget, not a viewport budget** — owner ruling, 2026-08-08:
**0.5–1 s is acceptable for a filter change at 10⁹**, because a filter changes far less often than
the viewport does. An earlier reading of these numbers judged them against the selection path's
158–191 ms p99 and concluded an accelerator was required wherever coverage could be broad. That
was the wrong yardstick.

| Candidate shape | Affordable coverage at 10⁹, 500 ms | at 1 s |
|---|---|---|
| Contiguous | ~1.7 × 10⁸ (17%) | ~3.4 × 10⁸ (**34%**) |
| Scattered | ~2.3 × 10⁷ (2.3%) | ~4.5 × 10⁷ (4.5%) |

Measured, a 25%-coverage contiguous principal costs **730 ms** — inside the band with no accelerator
at all. Only near-total coverage exceeds it: a full-corpus scan measures ~3.0 s. **The corner is the
privileged tail, not broad coverage**, and it is the only place an accelerator earns anything.

## Arm 2 — does the category posting close the gap the scan leaves?

`accel` ([`layoutprobe/src/bin/accel.rs`](layoutprobe/src/bin/accel.rs), raw
[`run-accel.csv`](run-accel.csv)) answers `value == v ∧ candidate` three ways over the same
relation: the flat-column **scan**; **intersect**, which is what the built keyed reader does; and
the candidate-driven **probe** proposed to equalise work. The column is `u8`, the narrowest a
category can be and so the most favourable case for the scan.

**Yes, decisively.** At 10⁹, 25% coverage, worst posting shape:

| Value's share of the corpus | scan | intersect | probe |
|---|---|---|---|
| 0.1% | 785 ms | 0.13 ms | 5.0 ms |
| 1% | 795 ms | 0.78 ms | 10.6 ms |
| 25% | 1,351 ms | 3.56 ms | 38.3 ms |

The worst cell measured anywhere — scattered posting, 25% of the corpus carrying the value, **full**
coverage — is 5,271 ms scanned against **49.5 ms** intersected, a 107× reduction that lands inside
the 158–191 ms operating point. Postings are also nearly free to store: a correlated posting is
8 B–54 KB at 10⁹, a scattered one 2–125 MB, against a 1 GB `u8` column.

**The corpus's 21.7 ms / 2,885 ms warning does not apply to this operation and should not be cited
against it.** That spread is a union over ~10⁴ *term* postings during `M_auth` construction. A
single value intersected against a candidate is one bitmap operation and never enters that regime.

### The hidden-value channel, measured directly

per-point-attributes §3.8 requires that a value the principal cannot see be indistinguishable **in
work** from a value that does not exist. Both return nothing; only timing could separate them. The
`hidden-25pct` rows place a 25% candidate **disjoint** from a correlated value's members, so the
principal sees none of them, and time it against a value with no members at all.

At 10⁹, 25% coverage:

| Value | intersect | probe |
|---|---|---|
| Does not exist (no members) | 0.000 ms | 1.51 ms |
| Hidden, 1M members | 0.000 ms | 1.05 ms |
| Hidden, 10M members | 0.001 ms | 1.18 ms |
| **Hidden, 250M members** | **0.000 ms** | **1.06 ms** |

**No measurable channel, at either evaluation form.** Roaring's intersection short-circuits on
container keys, so a posting whose containers do not meet the candidate's costs a key-list merge and
nothing more, however many members it holds. The plain intersect is as flat as the candidate-driven
probe here, and the probe's ~1.1 ms floor buys nothing the intersect does not already give.

**Read this narrowly.** Work still scales with the *visible* result — 38 ms for a value 62M of whose
members the principal can see — but that is not a disclosure, since the principal is entitled to
those results. The measured absence of a channel is for the **correlated** shape only. A uniformly
scattered value cannot be fully hidden from a broad principal in the first place (its members are in
every container), but the intermediate case — a scattered posting whose containers the candidate
meets while no bits match — is **not measured**, and reasoning says work there is container-
proportional while the result is empty. That case, not the one measured, is where a residual could
live.

## Arm 3 — getting the result into row space

`project` ([`layoutprobe/src/bin/project.rs`](layoutprobe/src/bin/project.rs), raw
[`run-project-1e9.csv`](run-project-1e9.csv)) compares the two routes from an entity-space filter
result to the row-space answer a viewport needs. **project** gathers `entity_to_row` over the
result's set bits, sorts and builds a row-space bitmap — O(set bits). **per-tile** never projects:
for each of the ~300 tiles a viewport resolves to, it walks that tile's contiguous row range, maps
row → entity and tests membership — O(rows in the viewport), independent of the result's size.
`entity_to_row` is a genuine Fisher–Yates permutation, because both routes are dominated by random
access and a structured map would flatter them equally and wrongly.

**Two constants, at 10⁹:**

| Route | Cost | Scales with |
|---|---|---|
| project | **~27 ns per set bit** | the result |
| per-tile test | **~6–22 ns per viewport row** | the viewport |

| Result cardinality | project | per-tile, 300 tiles × 1,000 rows |
|---|---|---|
| 10⁵ | **3.8 ms** | 1.73 ms |
| 10⁶ | 27.1 ms | **2.37 ms** |
| 10⁷ | 271.7 ms | **3.79 ms** |
| 10⁸ | 2,779 ms | **6.49 ms** — 428× |

**The rule: project only when the result is smaller than roughly a quarter of the viewport's row
count; test per tile otherwise.** The ratio is just the two constants. At a 300,000-row viewport the
crossover is a result of ~10⁵, and both scales agree on it (~1/4 at 10⁸, ~1/3 at 10⁹).

**The asymmetry is what matters for the design.** The per-tile route is bounded by the viewport,
which is already bounded by the drawn-mark budget, so it scales with neither the corpus nor how much
the filter matches. Projecting a 10⁸-entity result costs 2,779 ms — **more than the 730 ms scan that
produced it** — so for any broad filter the projection, not the scan, was the dominant term, and
this route removes it.

**These project figures are a floor, not a worst case.** The results here are *contiguous*, which is
the cheap end for a gather. The corpus's 127 ns/set-bit point (`probes/results.md` §6, a 69.3×10⁶
mask) is ~4.7× this arm's constant and is the likely shape of a scattered one; §10.4's 10.7 ns/item
at 10⁹ is a different operation again. **The per-bit constant is shape-dependent and no design should
quote a flat one.**

## What this does not settle

- **Value width.** Everything here is a `u32` column. A `u8` category quarters the value bytes but
  not the per-candidate constant, so the scattered-candidate case (cache-miss bound) should improve
  more than the contiguous one (work bound). Not measured.
- **Strings.** Variable-width values change the access pattern entirely and are not modelled here.
- **Parallelism.** Single-threaded throughout. A real scan would be parallel across candidate
  containers; the ranking between layouts should be unaffected, but the absolute budget above would
  move.
- **Disk.** In-memory throughout, because the machine had 4.7 GB free. A cold scan is a different
  measurement and the 2.9 ns constant does not transfer to one.
- **The accelerator crossover.** Arm 2 settles this for categories. For *numerics* it is still open:
  neither zone maps nor bit slicing is built, and which (if either) should be is an owner ruling
  (`filter-index.md` §3). If bit slicing is ever taken, its modelled ~4 GB per `u32` column at 10⁹ —
  derived from an assumed half-density, never measured — needs a gate arm before it is believed.
- **The row-space projection of a filter operand, against operand cardinality.** The corpus holds two
  measured points that do not agree per-bit: 8.8 s for a 69.3×10⁶-entity mask (`results.md` §6, i.e.
  127 ns/set-bit) and 10.7 s at 10⁹ (§10.4, i.e. 10.7 ns/item). The constant is evidently
  cardinality-dependent, and no curve exists between them. Any design quoting a flat per-bit figure is
  quoting one end of an unmeasured range.
- **The build's peak resident set, per family.** The current emit holds a `u32` per non-absent entity
  per column on top of an attribute tail already priced as outside the memory plan at 10⁹.

## Method

`layoutprobe` builds one synthetic column of 1,000 distinct `u32` values under three presence
shapes, and scans it for a single value under three candidate masks, in four layouts. Presence
shapes: universal; ten slices interleaved in 10⁵-entity blocks (modelling concurrent multi-slice
ingest, where write-path §4.2 makes an entity range ascending-*with-holes*); and 10% scattered.
Candidates: 1% contiguous (a sparse principal under signature-sorted assignment), 25% contiguous
(a head principal), 1% scattered (the `surnames` policy shape, where signature sorting measured
~1.0×).

Every layout holds the same logical relation, and the harness asserts all four return **identical
answers** before any timing is reported — without which the fast ones are only fast. Each cell is
preceded by an untimed warm pass. Roaring sizes are *serialised* (`Portable`), which is what a
bundle would store, not the in-memory footprint.

Reproduce: `cargo build --release && ./target/release/layoutprobe 1000000 100000000 1000000000`.
