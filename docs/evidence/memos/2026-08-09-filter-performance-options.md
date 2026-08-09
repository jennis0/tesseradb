# Filter performance: the wins still on the table, and what each costs

**Date:** 2026-08-09 · **Status:** Evidence memo — recommends, does not rule.
**Reads with:** [`filter-index.md`](../../design/filter-index.md),
[`filter-surface.md`](../../design/filter-surface.md),
[`2026-08-08-filter-index-decisions.md`](2026-08-08-filter-index-decisions.md),
[`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/) — arms 1–7 plus the
three arms this memo adds: `resultbuild`, `unionsel`, `textwidth`.

Everything quoted as measured is a median of three runs on the campaign machine (WSL2, 12 cores,
47 GB, single-threaded), at 10⁹ unless marked. Parallelism is a known deferred lever and is not
proposed here.

## 1. Results

The largest known gap in the filter path — an unselective predicate costing 3.4–6.0 s against the
ruled 0.5–1 s filter budget at 10⁹ — **closes**, by building the result's Roaring containers
directly instead of inserting entities into them, and the fix also improves every broad-candidate
cell rather than taxing the selective ones the way the refused branchless collection did. Ranked:

| # | Option | Applies to | Measured gain at 10⁹ | Memory at 10⁹ | Disk at 10⁹ | Security | Cost, risk |
|---|---|---|---|---|---|---|---|
| **1** | **Packed result containers** — evaluate the predicate into 2¹⁶-entity bit blocks (SWAR kernel) and hand the blocks to croaring as a hand-assembled portable stream, in bounded chunks | Every fixed-width family, contiguous candidate runs, **all** selectivities | **The unselective gap closes with room to spare**: 25%-of-corpus match 3,384 → 200 ms (17×), 50% 5,919 → 127 ms (47×), 75% 10,230 → 139 ms (74×); the selective whole-corpus cell *also* improves, 683 → 173 ms — this is not arm 7's refused trade | Transient ~1 MB chunked, whatever the result (today ≤512 KB); the result bitmap itself is unchanged | None | **None new.** Work is a function of (candidate, column, *visible-match density*); a hidden value and an absent one both produce zero matches and take an identical path, so §3.8 holds by the same construction as today | Medium. One format-writing module (~300 lines) whose failure is a **wrong mask**, not a crash — §3's differential tests are the condition of shipping it; SWAR kernel is per-width and only `u8` is measured |
| 2 | **Wire the category postings in** — `ColumnPostings` is built, tested and unreachable; `FilterColumns::resolve` answers every category by scan | Category `eq`/`in`, all selectivities; the only option that helps a **scattered** candidate | Selective: 74–280 ms → 0.13–3.6 ms (arm 2, re-based on the shipped scan). Unselective `in`, scattered vocabulary: 3,641 → 287 ms at 25% of corpus, 6,368 → 511 ms at 50%; correlated: ≤2.5 ms everywhere. An `in` matching ~everything is the one cell the union *loses* (664 → 872 ms) | Mapped; resident is what requests touch | **Measured: 2.01 GB per fully scattered 10⁹ category column (2.0 B/present entity); 217 KB correlated.** 16 scattered columns: ~32 GB — postings for a scattered `u8` category cost ~2× the column they accelerate | **A real, newly measured residual**: a hidden scattered value with members costs 2.1 ms per operand where an absent value costs ~0.00 ms. Fine for `public` listing *if registered* (C4-shape row, C8-adjacent content); for `per_viewer` §3.8 forbids it — keep the scan (or arm 2's flat ~1.1 ms candidate-driven probe) there | Small. Reader exists and is green; engine wiring, manifest plumb, serving-path tests |

The third lead investigated — `i32` text offsets — is **refuted by measurement** and appears under
§6, not here. Options 1 and 2 overlap on unselective *category* predicates — either clears the
budget there. Option 1 covers what option 2 cannot (numerics, text, and every family's middling
selectivity); option 2 covers what option 1 cannot (the scattered candidate, where there are no
runs to pack). They compose rather than compete.

## 2. The category accelerator is built and never used — verified

`FilterColumns::resolve` (`tessera-engine/src/filter.rs`) answers `Equals`/`In` through
`ValueColumn::scan_eq`/`scan_in`; `tessera_filter::ColumnPostings` and `resolve_union` are exported
by the crate and referenced from exactly one place, `tessera-build/tests/filter_postings.rs`. The
build emits `attrs/<column>/postings.arrow` for categories; nothing at serve time opens it. This is
coherent with the record — `filter-index.md` §2.3 classifies the postings as an optional per-column
accelerator and D2 ruled them not required — but two things have moved since D2:

- **The owner's tolerance changed the trade.** D2's arithmetic was "the scan is inside the budget,
  so an accelerator buys latency nobody needs". That holds for selective predicates. Arm 7 then
  showed the unselective ones are 3.4–6.0 s — outside the budget — and §4 below measures that
  postings fix the category share of that gap outright.
- **`per_viewer` categories are owed postings anyway** (§2.3's exception): the membership sets
  `/v1/categories` needs *are* these postings. Wiring them into the filter path shares the artefact
  that ruling already requires.

**Judge the gain against the shipped scan, not arm 2's baselines.** Arm 2's scan column (785–5,271
ms) predates arm 4's 11–462× scan rework; the same cells on the shipped code are ~74–280 ms. The
honest selective-case comparison is 74–280 ms → 0.13–49.5 ms: two orders, but from inside the
budget to further inside it. The case with money in it is below.

## 3. Option 1 — build the containers, not the entities (`resultbuild`)

Arm 7 decomposed the unselective cost into ~40% mispredicted branches, ~30% croaring `add_many`,
~30% buffer traffic, and recorded that branchless evaluation becomes attractive only **alongside a
cheaper result representation**. The scan visits entities ascending, so all matches for one 2¹⁶
block arrive together; evaluated branchlessly they are *already* the block's 8 KB Roaring bitset —
the only problem is handing a finished container to croaring, which exposes no container-level API
(checked: `croaring` 2.7.0 and the `croaring-sys` bindings of `roaring.h`). The route measured here
assembles the **portable serialization format** by hand — cookie 12346, `(key, cardinality−1)`
descriptors, offset table, array (≤4096) or bitset payloads — and passes it to
`Bitmap::try_deserialize::<Portable>`, in 128-container chunks OR-ed into the result so the
transient stream stays ~1 MB rather than the whole result serialized (125 MB at half of 10⁹).

`u8` column, uniform values, range predicate; medians of three at 10⁹:

| Candidate | Matches | shipped | branchless pack + sink (`portable`) | SWAR pack + sink (`portable-swar`) | chunked sink (`portable-chunked`) | SWAR count floor | `saferange` |
|---|---|---|---|---|---|---|---|
| whole corpus | 1% | 683 ms | 854 | **173** | 174 | 92 | 915 |
| whole corpus | 5% | 1,136 | 1,011 | **317** | 319 | 94 | 1,200 |
| whole corpus | 25% | 3,384 | 815 | **200** | 229 | 93 | 2,084 |
| whole corpus | 50% | 5,919 | 826 | **127** | 158 | 95 | 6,601 |
| whole corpus | 75% | 10,230 | 836 | **139** | 138 | 97 | 5,288 |
| whole corpus | 100% | 874 | 825 | **144** | 124 | 93 | 788 |
| 25% broad | 1% | 169 | 209 | **39** | 39 | 23 | 224 |
| 25% broad | 25% | 861 | 207 | **33** | 32 | 24 | 502 |
| 25% broad | 50% | 1,462 | 206 | **33** | 32 | 24 | 1,599 |
| 25% broad | 75% | 2,462 | 204 | **32** | 33 | 23 | 1,256 |

Reading it:

- **`portable-swar` and `portable-chunked` put every measured cell inside the filter budget**,
  including the 75%-match whole-corpus cell the shipped code answers in 10.2 s. The worst packed
  cell anywhere is ~320 ms (whole corpus, 5% selectivity — array-container extraction is the
  overhead, and it peaks where every container hovers just under the 4096 array/bitset threshold).
- **This is not the refused branchless trade.** Arm 7's branchless-into-`Hits` cost the selective
  arms 1.5–2.1× to buy 1.3–1.6×. Here the whole-corpus 1%-selectivity cell *improves* ~4×
  (683 → 173 ms) because the SWAR kernel's floor (~0.10 ns/value, ~10 GB/s on a `u8` column) is
  below even the branchy selective scan's per-candidate cost. The one shape that cannot pack is a
  **scattered candidate** — one-element runs — which keeps the existing per-entity path and its
  arm-4 constants; an integration packs runs that span a block and falls back below that.
- **The sink and the kernel are separable, and the sink is nearly free.** Plain branchless packing
  (`count`, ~0.8 ns/value — it does **not** autovectorise) plus the portable sink already lands
  815–1,011 ms flat; the SWAR kernel is what buys the rest. So a first integration could ship the
  sink with the simple loop and already hold every cell at or under ~1 s, then add per-width
  kernels for the headroom.

**What it costs.** No disk. Transient memory ~1 MB chunked. The deserialized result arrives as
array/bitset containers — the hand stream never emits run containers — so a near-total result
should keep the existing "run-optimize when coalescing fired" pass (arm 7 measured both sides of
that trade); emitting run containers directly in the stream (cookie 12347) is possible and
unmeasured.

**The risk is a wrong mask, and the test that contains it.** A bug here produces a silently wrong
result — a disclosure, or a blanked map. Condition of shipping: (a) a property test driving random
block densities across the container-type boundary (4095/4096/4097), empty and full blocks, the
partial tail block, and the 0xFFFF key, asserting the hand-built bitmap `==` an `add_many`-built
reference and passes croaring's `internal_validate`; (b) the probe's own harness asserts equality
with the shipped scan on every timed cell already; (c) the conformance suite's differential vectors
extended to unselective predicates, which today's vectors — like arms 1–6 — hold selective.

**Security disposition: no new row.** The work is a function of the candidate, the column, and
where *visible* matches fall — quantities the principal is entitled to (the result itself discloses
them exactly). A value the principal cannot see and a value that does not exist both produce zero
matches, identical block cardinalities, and identical container emission: §3.8's pair stays
indistinguishable in work by construction, exactly as in the shipped scan. C4 is untouched; nothing
here consults unmasked data the scan does not already read.

**Not measured, stated as such:** SWAR kernels beyond `u8` (the measured kernel additionally
assumes bytes below 0x80; the general byte form is a few more ops and Hacker's-Delight standard);
`u16`/`u32`/`i64`/`f64` pack loops — a `u32` whole-corpus pack has a ~250–400 ms bandwidth floor
at the 10–16 GB/s this machine measures, which would still clear the budget, but that is
**modelled, not measured**; the partial-presence traversal feeding the packer; and a word-sink for
**text** (set bits per match into blocks without the branchless kernel — removes the `add_many`
term but not the branch term; unmeasured).

## 4. Option 2 — the unselective case postings were never measured on, and the residual (`unionsel`)

Postings exist only for categories; numerics and text are untouched by this option (zone maps and
BSI stay declined — see §6). Two shapes bracket the vocabulary-to-entity correlation as in arm 2:
`correlated` (each value's members contiguous) and `scattered` (uniform, the worst case). The
column is 100 values of 1% each; `in` names `k` of them, so `k` is both the set size and the
percentage of the corpus matched. The postings route is `fast_or` across the named values'
postings, then one intersection with the candidate — the whole-corpus-resolve-then-intersect
ordering the built reader forces (`filter-surface.md` §5.1's composition makes that sound: the
candidate is the composed verdict, and the intersection removes anything the postings still carry).

Medians of three at 10⁹:

| Shape | Candidate | k (= % matched) | scan | union + intersect |
|---|---|---|---|---|
| correlated | whole corpus | 5 | 468 ms | **0.1** |
| correlated | whole corpus | 25 | 514 | **0.5** |
| correlated | whole corpus | 50 | 570 | **1.1** |
| correlated | whole corpus | 100 | 673 | **2.5** |
| scattered | whole corpus | 5 | 1,113 | **218** |
| scattered | whole corpus | 25 | 3,641 | **287** |
| scattered | whole corpus | 50 | 6,368 | **511** |
| scattered | whole corpus | 100 | **664** | 872 |
| scattered | 25% broad | 25 | 887 | **235** |
| scattered | 25% broad | 50 | 1,583 | **446** |
| scattered | 25% broad | 100 | **165** | 991 |

- **The union stays container-priced.** The corpus's 21.7 ms / 2,885 ms warning is about ~10⁴-term
  unions; a tick-box `in` unions tens of postings and the worst honest cell is 511 ms — inside the
  budget. So the unselective *category* gap closes from either side, this option or option 1.
- **The one cell the union loses is "everything ticked"**: `k = 100` matches the whole candidate,
  the scan coalesces it into ranges in 165–664 ms, and the union pays for materialising the whole
  corpus first (872–991 ms). If postings are wired in, route on the named share of the vocabulary —
  a public schema quantity plus the candidate, so the routing itself leaks nothing.
- **Storage, measured:** the 100 postings serialize to **2.012 GB** for the scattered shape —
  2.0 B per present entity, twice the `u8` column itself — and **217 KB**
  correlated. Contiguity in entity space is again the highest-leverage property; a deployment whose
  categories are uncorrelated with the signature sort pays ~2 GB per column per 10⁹, 32 GB at the
  16-column shape, on disk and in page cache under load.

**The residual timing channel, now measured.** Arm 2 measured the *correlated* hidden pair flat
(0.000 ms both) and left open the case where a scattered posting's containers meet the candidate
while no bits match. Measured here at 10⁹ — a 25% contiguous candidate against a 10⁷-member
uniformly scattered posting it shares no bit with:

| Value | intersect against the disjoint 25% candidate |
|---|---|
| Does not exist (no members) | **0.000 ms** |
| Hidden, 10⁷ members, uniformly scattered | **2.1 ms** |

An observer issuing a filter for a value they cannot see and timing it learns, at ~millisecond
resolution, that the value **has members somewhere in the corpus** — and coarsely how many
containers they span — where a value that does not exist returns in ~0 ms. What this touches:
per-point-attributes §3.8 (the pair must be indistinguishable in work), and it is C4's shape
carrying C8-adjacent content (a monotone signal of a corpus-wide member count, which nothing may
publish). Not derivable from `/v1/meta`: that publishes types and operands identically to every
principal, never member counts. Disposition consistent with the owner's stated tolerance:

- `listing = "public"` columns: the value's *existence* is already published to every principal;
  the residual signal is its rough corpus-wide popularity. Acceptable as a registered row if the
  owner rules it so; it is not derivable today and must not ship unregistered.
- `listing = "per_viewer"` columns: this is precisely the existence question the control hides.
  Route those columns' filters through the scan (work candidate-proportional, value-independent)
  or the candidate-driven probe arm 2 measured at a flat ~1.1 ms. D4 already reached this split.

## 5. Narrower text offsets — refuted (`textwidth`)

Arm 6 left "halve the offsets" as the next text lever: per value the scan streams two 8-byte
offsets plus ~14 value bytes, so `i32` offsets should remove ~a fifth of the traffic. Measured at
10⁸, surname-shaped values, 25% contiguous candidate, the identical reimplemented walk over the
identical bytes with only the offset type differing:

| Predicate | `i64` offsets, three runs | `i32` offsets, three runs |
|---|---|---|
| `eq` | 102 / 102 / 113 ms | 108 / 122 / 120 ms |
| `contains` | 244 / 257 / 269 ms | 271 / 261 / 265 ms |

**The modelled ~20% saving: NOT confirmed by measurement — do not claim it is.** Equality measured
5–15% *slower* with `i32` in every pairing and `contains` within noise; the offset stream is
evidently prefetched in the shadow of the value bytes rather than competing with them, so the
"~22 B per value" arithmetic prices bytes that were not on the critical path. The capacity argument
would in any case have confined the option to small columns — Arrow `Utf8` offsets are `i32`,
capping concatenated bytes at **2 GiB** (arm 6's "fit in 4 GiB" was the unsigned reading; Arrow's
offsets are signed), which a 10⁹-entity column passes at two bytes a value. Not recommended; if
text ever needs to be faster, the lever left standing is the value-byte layout itself, not the
offsets.

## 6. Refused, and negative results

- **`saferange` (add the block as a range, `remove_many` the misses): refuted at the selectivity
  that matters.** At 10⁹ whole-corpus it measured 2,084 ms at 25% and 6,601 ms at 50% — the 50%
  cell **worse than the shipped scan's 5,919 ms** — because extracting and removing up to half the
  bits costs more than the insertion it saves. It only wins clearly at 75% (5.3 s against 10.2 s),
  where the portable sink is 38× better still. Do not re-propose it as the "safe alternative"; the
  safe alternative is the portable sink under §3's differential tests.
- **Plain branchless bit-packing does NOT reach the hardware floor — do not claim it does.** The
  obvious `w |= (pred as u64) << (i & 63)` loop measured ~0.8 ns/value (it does not autovectorise
  under this toolchain); the SWAR kernel is 8× that. Any integration that ships the simple loop
  should quote 815–1,011 ms whole-corpus, not the ~95 ms floor.
- **Branchless-into-`Hits` stays refused** (arm 7): 1.5–2.1× on selective arms for 1.3–1.6× on
  unselective ones. Superseded by option 1, which is branchless *and* changes the sink.
- **Zone maps stay declined, even under the owner's leak tolerance.** They buy a speed-up on
  numeric ranges that option 1 already brings inside the budget for a fraction of a leak-register
  row: the trade D5 refused has become strictly worse, since the win it bought is now available
  clean.
- **`u32` category `in` (O(log k)): left alone, on arithmetic rather than measurement.** At the
  measured 3.4 ns/candidate a 25%-coverage `in` at 10⁹ is ~850 ms — inside budget — and k is
  client-named and small. A first-byte bucket like `ByteSet`'s would likely close most of the gap
  to `u16`'s 0.46 ns; NOT measured, and not worth its surface until a workload shows a `u32`
  category at all.
- **`i32` text offsets: NOT confirmed by measurement** — §5. Equality 5–15% slower, `contains`
  within noise, capacity confined to sub-10⁹ columns anyway.
- **Postings for numerics or text: still declined** (`filter-index.md` §3, memo D5's C11 grounds).
  Nothing measured here reopens it; option 1 removes the remaining latency motive.

## 7. What this campaign still has not measured

The scattered-candidate text `contains` cell (~1 s per 10⁷ candidate entities, arm 6) is untouched
by everything above — no run to pack, no posting to intersect — and remains the one measured cell
over budget for a plausible principal; the one lever left standing for it is the value-byte layout,
unpriced. Cold/on-disk scans; the packed path over partial presence; run-emission
in the hand stream; SWAR beyond `u8`. Each is marked at its claim above where it qualifies one.

## Method

`resultbuild`, `unionsel` and `textwidth` live beside the other arms in
[`layoutprobe/`](../../../probes/2026-08-08-filter-layout/layoutprobe/); raw CSVs beside this memo's
figures are the probe directory's `run-resultbuild-1e9-*.csv`, `run-unionsel-1e9-*.csv`,
`run-textwidth-1e8-*.csv`. Every timed arm first asserts bitmap equality with the shipped scan's
result on the same inputs, `resultbuild` was additionally run at 1,000,003 entities so the tail
block is partial and non-word-aligned, and every
figure is the median of three whole-process runs interleaved nowhere — run-to-run drift on this
machine is ~5% and the scattered cells noisier, so differences under ~10% should be re-measured
A/B before anything is concluded from them.
