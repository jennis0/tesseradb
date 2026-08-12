# Epic 1 measured: the blob's row compresses better than assumed, and the row route costs roughly five times what the probe said

**Date:** 2026-08-12 · **Status:** Evidence, not normative · **Machine:** WSL2 on Linux 6.18,
AMD Ryzen 9 5900X (Zen 3, 12 cores, 32 MiB L3), 47 GB RAM · **Harness:**
[`record_blob_ratio`](../../../crates/tessera-bench/src/bin/record_blob_ratio.rs),
[`row_route_cost`](../../../crates/tessera-bench/src/bin/row_route_cost.rs) ·
**Raw:** [`probes/2026-08-12-epic1-measurements/`](../../../probes/2026-08-12-epic1-measurements/)

`records-and-search.md` §11 item 6 owes three residuals. Two are now measured on the built system
and one is measured only part of the way; every figure below is marked, and the part that is not
measured is named rather than filled in.

## Results

**1. The blob's mixed-row compression: the assumption holds, with margin.** §3 marks the mixed-row
ratio *assumed* on the strength of a 2.44× measured on per-column title bytes. Written through the
shipped `RecordBlobWriter` at the shipped 256 KiB target, over 2,400,000 real arXiv records, a
fourteen-field mixed row reaches **3.00×** against a title-only control run through the same
writer at **2.54×** — the mixed row compresses **18% better than the column it was assumed merely
to match** (*measured*). Interleaving costs less than the shared context between neighbouring rows
buys. §3's sentence can be re-marked from assumed to measured, in the direction it hoped for.

**2. The built row route costs 2.5–3.4 ns per viewport row, not 0.48–0.73.** §6.2 quotes the
probe's constants and warns they describe the approach rather than shipped code (review N4). The
warning was right: the gap is **3.4–7.1× depending which ends of the two ranges are compared,
~4.8× at the midpoints** (*measured*, single-threaded, three corpus scales). What the probe got
right is the *shape*: the constant is flat across 2.4M, 25M and 10⁸ items and across viewport
sizes from 3×10⁵ rows to the whole slice, so "invariant in corpus size, mask shape and coverage"
survives intact. Only the magnitude moves.

**3. The coarse-zoom cell at 10⁸ costs 280–299 ms single-threaded, above the 42–260 ms §6.2
quotes** (*measured*) — and **33–38 ms under the sweep's real parallelism** on 12 cores
(*measured*). §6.2's conclusion survives its constants: the cell is over the 100 ms target and
inside the O(1 s) budget only because the route runs inside the tile sweep's rayon fan-out.

**4. ⊘ The 7–1,269× advantage over the entity route is NOT confirmed — it measures 0.63–2.58× at
10⁸, and in the coarse cell the sign reverses.** This is not a measurement that came out
differently; it is a comparison the built system does not offer. The probe's entity side is
`ValueColumn::scan_eq`, and the built engine answers an indexed category from its **derived
postings** (0063) — 15–26 µs at 10⁸, which no per-entity scan could be. On a 343,391-row viewport
at 10⁸ the row route costs 0.91–0.95 ms against the entity route's 0.60–2.36 ms (*measured*): paired
per value it wins by 1.6–2.6× on the broader ones and loses by 1.5–1.6× on the selective ones. At the coarse-zoom cell
the entity route is **100–450× faster** (0.66–2.72 ms against 280–299 ms). §6.2 should stop
quoting the probe's ratio as the built route's advantage.

**5. Two figures nobody asked for, both load-bearing.** The blob's addressing costs **4.13 B per
has-row entity** and is shape-independent (*measured*) — which confirms the ~4.0 B implied by §3's
N7 arithmetic. And a random single-row blob read costs **253–265 µs against §3's quoted 169 µs**
(*measured*), 1.5× — consistent with a 256 KiB block decompressing at the string-storage probe's
own 1.5 GB/s, so the quoted figure was optimistic rather than the reader being slow.

### What the design owes, exactly

The design file is frozen to this track; these are the re-markings it owes.

| § | claim today | owed |
|---|---|---|
| §3 | "a shared-context row of mixed fields is *assumed* to compress at least as well, not measured" | **measured**: 3.00× mixed against a 2.54× title control, both through the shipped writer |
| §3 | "169 µs for a random single-value read" | **253–265 µs measured** through the built reader; the 169 µs is the probe's, not this format's |
| §3 | the N7 arithmetic's implied ~4.0 B addressing | **4.13 B/entity measured**, shape-independent |
| §6.2 | "The probe measured 0.48–0.73 ns per viewport row" | keep as the probe's; add **2.5–3.4 ns measured on the built route**, invariance confirmed |
| §6.2 | "7–1,269× over the entity route at 10⁸" | **refuted for a category**: 0.63–2.58× measured at 10⁸ on a 343,391-row viewport, and the entity route is 100–450× faster in the coarse cell. The engine answers an indexed category from postings, not the scan the probe timed |
| §6.2 | "measured 42–260 ms at 10⁸ single-threaded … modelled 0.4–2.6 s at 10⁹" | **280–299 ms measured at 10⁸**; **2.80–2.99 s modelled at 10⁹** serial, **0.33–0.39 s modelled** at 12 cores |
| §6.4 | "any viewport-bounded filter … ≲ 1–33 ms *(probe-measured / modelled)*" | the row-space half holds: **0.89–1.05 ms measured** for a 343,391-row viewport at 10⁸, corpus-size invariant |
| §6.4 | "÷ cores" as the parallel remedy | measured speedup at 10⁸ is **7.4–8.5× on 12 cores**, and 2.8–3.6× at 2.4M — the shorthand is optimistic, and increasingly so below 10⁷ rows |
| §11 item 6 | four residuals owed | two discharged (blob ratio, built route constants); the coarse cell discharged **at 10⁸, modelled to 10⁹**; list timing on real skew untouched |

---

## 1. The record blob's mixed-row compression

Every shape below is written through `RecordBlobWriter` — the shipped writer, the shipped
`RECORD_BLOCK_TARGET`, the shipped zstd level and row framing — over records read from the Kaggle
arXiv snapshot in snapshot order, which is submission order, which is entity order
(`probes/dataset.md` §4.1; the string-storage probe reads the same file the same way). 2,400,000
entities, every one carrying a row.

| shape | fields | source B/e | framed B/e | blocks | **format ratio** | source ratio | B/e, all three files | µs/read |
|---|---|---|---|---|---|---|---|---|
| `title` (control) | 1 `utf8` | 75.56 | 90.56 | 830 | **2.54** | 2.12 | 39.78 | 264.9 |
| `mixed` | 8 `utf8` + 6 fixed | 244.02 | 314.87 | 2,885 | **3.00** | 2.33 | 109.00 | 253.2 |
| `mixed` + abstract | + 1 `utf8` | 1,262.46 | 1,340.31 | 12,306 | **2.74** | 2.58 | 492.73 | 258.0 |

*Format ratio is framed row bytes ÷ `blocks.bin` — what the compressor achieved on what the format
handed it. Source ratio divides by the raw value bytes instead, so framing is counted as a cost.*

**The control is what makes this a comparison.** Quoting `mixed` against 2.44 directly would
compare things differing in more than the row shape: the probe compressed a column that carried an
8 B offset per value, through a different compressor invocation, with no row framing at all. Its
own arithmetic is visible in this table — the probe's flat title column is 83.56 B/entity at
2,400,000 and this measures the same titles at 75.56 B plus the 8 B offset it does not store, to
the byte — but a ratio taken over different inputs is not transferable. Running the same field
through the same writer isolates the one variable §3's assumption is about, and on that comparison
the mixed row wins by 18%.

**Why it wins.** A blob row interleaves a `utf8` title with an `i64` timestamp and a `u8` count, so
the compressor sees short runs of dissimilar bytes where the column gave it 256 KiB of one kind —
the reason to expect worse. Against that, the fields a row adds are the ones that repeat hardest
between neighbouring rows: the same licence URLs, the same `arXiv:` prefixes, the same journal
names, the same submitter, run after run in submission order. The second effect is larger, and the
abstract shape shows where it stops: adding 1 KB of prose per row that shares almost nothing with
its neighbours pulls the ratio back to 2.74.

**The addressing is 4.13 B per has-row entity and does not vary with the row.** `directory.arrow`
is 9.93–10.30 MB across all three shapes while `blocks.bin` spans 85 MB to 1.17 GB: the `u32`
within-block offset per has-row entity dominates it, exactly as §3's has-row-rank design says. The
has-row bitmap is 527 B for a universal set. On the `title` shape that addressing is 10% of the
blob's total cost, which is the regime §3's N7 note is about — a near-sequential `id` whose
compressed content is under a byte per entity pays essentially all of its cost here.

**⊘ The read is slower than §3 quotes.** 253–265 µs per random single-row read against the quoted
169 µs. The arithmetic is unsurprising once stated — 256 KiB at the string-storage probe's measured
1.5–1.7 GB/s is 150–175 µs, and the built reader adds the file read, the directory binary search
and the row decode on top — so this is the 169 µs figure having been a decompression rate rather
than a read latency, not a defect in the reader. It stays comfortably inside §10.3's
per-interaction budget; it is simply not 169 µs, and the design quotes it as if it were.

**What this does not measure.** Any scale past the real corpus: arXiv has 2.42M records and this
reads a prefix of them. Block behaviour is scale-free by construction (the target is a byte count,
not a fraction), but nothing here demonstrates that at 10⁹. The coalesce and fold rewrite rates
(§11 item 7) are untouched. And every string here is English; §11 item 3's non-Latin corpus would
change both the byte volumes and the compressor's dictionary hit rate.

## 2. The built row route's constants

`evaluate_row_route` is private and this track does not edit what it measures, but it does not need
to be reached: the stage timings already separate it. `filter_eval_ns` ends at `evaluate_routed`,
which for a row-routed tree only builds the routed tree; the scan lands in `filter_cross_ns`, and
`rows_in_ranges` is the domain it walked. **For a pure row leaf** — one category predicate, no
entity-space sub-tree, hence no crossing to confound the counter — `filter_cross_ns ÷
rows_in_ranges` is `evaluate_row_route` and nothing else. Every filter measured here is a single
leaf for that reason.

Single-threaded, `archive` (`u8` codes) and `primary_category` (`u16`), two values each:

| items | ~3×10⁵ rows | ~1.5×10⁶ rows | ~10⁷ rows | whole slice |
|---|---|---|---|---|
| 2,422,486 | 2.83–3.02 (1.4×10⁵) | 2.53–2.63 (1.07×10⁶) | — | 2.59–2.64 |
| 25,000,000 | 2.83–3.07 | 2.59–2.98 | 3.02–3.11 (6.3×10⁶) | 3.10–3.27 |
| 100,000,000 | 2.60–3.06 | 2.49–2.62 | 2.68–2.82 (1.8×10⁷) | 2.71–2.79 |

*ns per row in the domain, minimum of five repetitions (three at 10⁸), both columns and both
values. §6.2 quotes 0.48–0.73 from the probe.*

**The invariance claim survives and the constant does not.** Across a 41× range of corpus size and
a 300× range of viewport size the built route sits at **2.5–3.4 ns per row** — flat, as the probe
said it would be, and several times above what the probe said it would be. §6.2 already warned that the
built version pays "the segment boundary and the `ScalarSlice` match" on top; the measured
surcharge is larger than that phrasing suggests, and the natural reading — a small constant
addition — is wrong.

Two effects worth naming because they are invisible in a per-row figure:

- **A fixed floor: 64–78 µs serial, 260–400 µs parallel.** At a 3,504-row viewport the per-row
  figure is 18–22 ns serial and 74–114 ns at 12 threads — bitmap setup and, in the parallel case,
  rayon fan-out, amortised over too few rows. **Up to roughly 10⁵ rows in the domain the parallel
  path is slower than the serial one** (0.42–0.86 ms against 0.40–0.42 ms at 1.4×10⁵ rows). The
  engine's `SERIAL_FALLBACK_MAX_ROWS` guards the tile sweep on exactly this argument; `scan_rows`
  splits unconditionally through `domain_chunks`, and the small-viewport cell is where that costs
  something. It is a fraction of a millisecond and well inside §6.4's viewport row, so this is an
  observation rather than a defect — but a per-keystroke filter over a small viewport is the shape
  it touches.
- **⊘ The code width makes no measurable difference and no direction is claimed.** `u8` and `u16`
  land within a few percent of each other and the sign is not consistent: at 10⁸ the `u16` column
  is 1.4% dearer on the shared value, at 25M and 2.4M it is 1–8% *cheaper*. The scan is not
  bandwidth-bound at one and two bytes on this machine; that says nothing about eight.

**A caveat that applies to every constant in this memo.**
[`2026-08-11-scan-constant-sensitivity`](2026-08-11-scan-constant-sensitivity.md) established that
this repo's scan constants are bimodal in instruction-address alignment — a 64–68% swing between
two discrete values, driven by where the linker put the hot loop, with no source change. The
workspace sets no alignment flag, so **these figures are one draw from that distribution and the
draw is not known to be the good one.** They should be read as "2.5–3.4 ns at this link", and a
figure re-measured after an unrelated commit that differs by up to two thirds is that effect, not a
regression.

## 3. The coarse-zoom cell, under the sweep's real parallelism

Zoom 0 over the whole extent: the domain is the slice, which is §6.2's coarse-zoom cell and §11
item 6's residual. The residual is specifically about *parallelism*, and `scan_rows` is already
parallel — it splits the domain with `par_iter` inside the engine's one shared pool — so this is
measured rather than modelled at every scale below 10⁹.

| items | serial | 12 threads | serial ns/row | 12-thread ns/row | speedup |
|---|---|---|---|---|---|
| 2,422,486 | 6.14–6.59 ms | 1.79–2.18 ms | 2.54–2.72 | 0.74–0.90 | 2.8–3.6× |
| 25,000,000 | 77.8–85.0 ms | 11.2–16.4 ms | 3.11–3.40 | 0.45–0.65 | 4.8–7.6× |
| 100,000,000 | **280.4–299.3 ms** | **32.9–38.5 ms** | 2.80–2.99 | 0.33–0.39 | 7.4–8.5× |

*All measured. Minimum of five repetitions (three at 10⁸), warm cache, `filter_cross_ns`, both
columns and both values.*

**Against §6.2's 42–260 ms at 10⁸ single-threaded, the built route measures 280–299 ms** — above
the top of the quoted range, and 6.5–7× the probe's own R-dense cell (42–43 ms), which is the
like-for-like comparison since R-dense is the variant `scan_rows` implements.

**At 10⁹: 2.80–2.99 s serial and 0.33–0.39 s on 12 cores — modelled, and here is the basis.** The
per-row constant is flat within 10% from 25M to 10⁸ (3.11–3.40 → 2.80–2.99 ns) and the parallel
split is by fixed-size chunks, so neither the per-row work nor the fan-out shape changes over a
further 10×. What the extrapolation does *not* cover, stated because it is the part most likely to
break it: at 10⁹ the `u8` column is 1 GB and the `u16` 2 GB, against 100 MB and 200 MB at 10⁸. Both
scales are already far past this machine's 32 MiB L3 — which is why the 2.4M row is the fast
outlier and is excluded from the basis — so the cache regime does not change; but TLB pressure and
twelve threads contending for memory bandwidth over a gigabyte are not measured, and both can only
push the figure up.

**§6.2's conclusion stands; its arithmetic does not.** The design says this cell is "over the
100 ms target and inside the O(1 s) budget only with the parallelism the sweep already has". Both
halves are confirmed: 0.33–0.39 s modelled at 10⁹ is inside O(1 s) and three to four times the 100 ms
target, and without the parallelism it would be 2.80–2.99 s and outside the budget. The design
reached the right conclusion from constants that were optimistic by 4–7× and a parallel divisor
(`÷ cores`, §6.4) that measures 7.4–8.5× on twelve rather than 12×.

**⊘ These are 1- and 2-byte columns, and §6.2's cell is ultimately about an 8-byte one.**
`scan_rows` accepts a `u8`/`u16`/`u32` code slice and refuses anything else, so the row route
serves categories and nothing else today — which is §6.2's own statement that "a rendered *number*
stays outside it until 0064's render half lands". The coarse-zoom cost §6.2 prices is the cost the
cell will have *when numbers arrive*, and an `i64` column is 4–8× the bytes per row measured here.
Whether that costs 4–8× more is not measurable yet and is not modelled: `u16` measured only 3–8%
above `u8`, so the scan is not bandwidth-bound at these widths at 10⁸ — but 8 GB of `i64` at 10⁹
across twelve threads is a different regime from 1 GB of `u8`, and nothing here speaks to it. **Do
not carry these constants onto a rendered number.**

## 4. ⊘ The entity-versus-row comparison, and why it is not a number

§6.2's headline is 7–1,269× over the entity route. **It is refuted for a category column, and the
reason is structural rather than a wrong constant.**

**The built entity route for a category is not a scan.** The probe's E column is
`ValueColumn::scan_eq` walking the candidate entity by entity. The engine, for a category column
carrying `index = true`, answers `eq` by intersecting the column's **derived postings** with the
composed candidate (0063; `filter.rs`'s module header states it). Measured on the both-placement
fixture at 10⁸ items, `filter_eval_ns` for that route is **15–26 µs** at the selective values and
147–249 µs at the broad one — hundreds of picoseconds per corpus entity, which no per-entity scan
could be. The 7–1,269× compares the row route against a route the engine does not take.

**The rule will not hold both operands still, so the comparison is two measurements.** 0068 routes
row-space while `rows_in_ranges ≤ |M_auth|`, so the route is a function of exactly the two
quantities that would have to be fixed to time both sides of one request; forcing the other route
means an engine override, which is outside a measurement track's business. What the sweep does
instead is vary the viewport across the crossover over **one** principal and check the shape each
route's cost has. At 10⁸, one principal (whose `|M_auth|` the crossover brackets between 1.7×10⁶
and 1.8×10⁷ rows, so 2–18% coverage):

| route | 3.4×10⁵ rows | 1.7×10⁶ rows | 1.8×10⁷ rows | 10⁸ rows (whole slice) |
|---|---|---|---|---|
| row (`eval + cross`) | **0.91–0.95** | 4.31–5.01 | — | — |
| entity (`eval + cross`) | — | — | **0.60–2.36** | 0.66–2.72 |

*ms, serial, minimum of three. The blank cells are the route 0068 did not choose there.*

**The entity route is nearly flat and the row route is proportional, which is what makes the two
columns comparable.** Entity grows only 1.1–1.5× over a 5.6× domain change — the residue is the
crossing being clamped to the domain, not the postings intersection — so the figure measured just
past the crossover is close to what the same principal would have paid at a smaller viewport. Read
that way, and paired per value rather than as two ranges: on a 343,391-row viewport the row route
is **2.58× and 1.62× faster** on the two broader values (`archive=hep-lat`, `primary_category=math.CT`)
and **1.47× and 1.59× slower** on the two selective ones. The whole spread is **0.63× to 2.58×**.
Not 7×, and nowhere near 1,269×.

**At the coarse cell the sign reverses hard.** Entity 0.66–2.72 ms against the row route's
280–299 ms over the same slice at 10⁸: **100–450× the other way.** This is the same direction the
probe's own arm 2 found ("at the coarsest zoom the two swap places") and far larger, for the same
reason the viewport cell is smaller — postings, not a scan.

**None of this touches §6.2's store-once decision, and it must not be read as doing so.** That
decision is about a rendered **number or datetime**, which has no postings and no entity-space copy
at all, so the row scan is the only route it has and its coarse cell costs what section 3 above
measures.
The comparison here is only available *because* a category is §4.2's exemption and keeps an
entity-space structure. What the measurement does say is that where both exist, §6.2's stated
advantage is the wrong size and, in the coarse cell, the wrong sign.

**What would settle the general question.** A row-versus-postings comparison across the selectivity
range at one scale, as two measurements over one principal on the shape above. That is one more arm
on this harness, and it is what §11 item 6 should ask for instead of the probe's ratio.

## 5. Method, and the fixtures that had to be made

**No fixture that could carry a rendered category existed.** `data/bench-fixtures/1e9` (38 GB) and
`2m4` both open and both carry `declared_scalars = []`; `data/scaled/attrs/schema.toml` no longer
parses, having been written against the `used_for = [...]` surface epic 1 replaced with the
`render` / `index` booleans. The schema is restated at
[`schema-render.toml`](../../../probes/2026-08-12-epic1-measurements/schema-render.toml), and a
second copy adds `index = true` so 0068's rule has something to choose between — a render-only
category has one placement and cannot be timed against the entity route at all.

**Above the real corpus, the attribute tail is carried, not invented.**
`probes/build_attributes.py` stops at 2,422,486 because "a wider scale would have to invent
attribute values for 99.8% of its items". That argument is about values, and the scaled corpus is
the same 2,422,486 papers repeated as affine transforms of their geometry (`dataset.md` §4.3), so
[`build_scaled_attrs.py`](../../../probes/2026-08-12-epic1-measurements/build_scaled_attrs.py)
assigns entity `e` the real attributes of source paper `e mod 2422486`. Every value written is a
real arXiv value on an entity that is a transform of the paper it came from.

**What that licenses and what it does not.** It licenses a *timing* fixture: byte width, code
distribution, match rate and the result bitmap's structure are the real corpus's. It does **not**
license any storage, vocabulary or skew claim — the distinct-value counts stay the real corpus's 38
and 176 however far this scales — and nothing in this memo reads one off it. The one visible respect in which it is
not a larger corpus: each replica's rows land in Morton order under its own transform, so the code
sequence along row space is a shuffled interleaving rather than concatenated copies. That is the
property the scan sees, which is why rows are derived from the real geometry rather than
concatenated.

Builds: 5.3 s at 2.4M, 74 s at 25M, 4m36 at 10⁸ (render-only). Both-placement at 10⁸ is 2 GB
larger and slower, the entity-space postings being the difference.

**⊘ Nothing here was measured at 10⁹, and this is what it would take.** The points file: the 10⁸
one took 3 minutes and 2.3 GB, and a 10⁹ one keeps every row of the geometry rather than a tenth,
so 20–40 minutes and ~23 GB. The bundle: `probes/2026-07-31-1e9-rebuild/` recorded 10m25 for the
attribute-free 10⁹ build at 47 GB, and this schema adds 14.88 GiB of hot column — call it 30–45
minutes and ~62 GB. Free space is the binding constraint at 54 GB after these three fixtures, so
it means clearing them first, not adding to them. Under two hours of wall clock, and it was not
spent. The three scales measured establish the constant's flatness over 41×, which is what the
extrapolation rests on; a 10⁹ run would replace one modelled row with a measured one, and is the
arm to run if the coarse cell's budget is contested.
