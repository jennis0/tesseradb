> **ARCHIVED 2026-08-01 — EXECUTED. The probe campaign it specified ran; its conclusions are in probes/ and the memos.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# The drawn-mark budget — what bounds it, and how we find out

**Date:** 2026-07-29
**Status:** spec, for review
**Touches:** design §7.2, §10.3–10.5, §13.2, Appendix A; contracts §2.5, §2.6, §3; SA §4.5; plan §5, §6, §14; probes/optimisations.md §3.5, §4.4

---

## 1. Why this exists

The design assumes a viewport draws a few thousand marks. §13.2 states it as a
property that does not break — *"a viewport shows a few thousand points at 10⁹
exactly as at 10⁷"* — and Appendix A budgets the wire at 0.6 MB for 50k points
(12 bytes/point). §7.2's *k* is "the on-screen mark budget per tile", used
throughout at k≈30 over ~10⁴-row tiles.

Owner decision (2026-07-29): **scale is one of the two things Tessera sells, and
the drawn-mark budget should be the largest a given client can render.** For a
capable GPU that is plausibly 10⁷ marks, three orders above the figure the design
is written around.

That is not a parameter change. It inverts which reads dominate, which
structures must be resident, and whether the on-disk columns should be
compressed — the question that started this. **Nobody knows where the real
ceiling is**, and four independent limits could each be the binding one. This
spec defines the measurements that find it, and records what is already settled
so the probes are not re-derived from scratch.

## 2. What is already settled

Recorded so the probes do not re-litigate these.

**k is client-declared and server-capped.** Contracts §3 already specifies
`POST /v1/viewport` as taking `k (server-capped)`. No contract change is needed
to make the budget negotiable — the gap is that the cap has never been
calibrated, and the surrounding documents are written as though k is ~30.

**Priority-based selection is prefix-stable in k.** Priority is a fixed
per-entity constant (contracts §2.6: high 16 bits of splitmix64 over the entity
ID), so the ranking never reorders. The k lowest-priority visible items for
k=1,000 are a strict subset of those for k=100,000. Two consequences:

- A large budget need not be one blocking response. The server can send a
  prefix and extend it; the client accumulates. §7.2's nesting proof is
  untouched, because both responses are the same definition evaluated at
  different k.
- The ceiling to find is therefore a **rate**, not a payload size.

**There are two distinct reads, and they scale differently** (probes §3.5):

| Read | Rows | Scales with |
|---|---|---|
| Priority, under direct evaluation | every *visible* row in the tile ranges | coverage × rows-in-range |
| Output gather (x, y, entity_id, node_id, scalars) | the k that won | the mark budget |

At k≈30/tile these are ~20M and ~9,000 respectively at 10⁹ and 2% coverage. At a
10⁷ mark budget the second becomes ~10⁷, and the two are comparable.

**The gather inverts from sparse to scanning at large k.** 10⁷ rows out of 10⁹
is 1% density. A 4 KB page holds 1024 f32s, so the probability a given page is
touched is ~99.99% — the gather stops being a sparse point read and becomes a
full scan of the geometry columns (~24 GB at 10⁹) to extract ~240 MB. A
near-sequential scan extracting a sparse subset is the regime where block codecs
win, and **x and y are Morton-sorted, so they are strongly structured** and
compress well under delta/frame-of-reference encoding. This reverses the §10.3
uncompressed argument *for the gather columns specifically*.

**Two columns remain incompressible regardless.** `priority` is uniform u16 by
construction (splitmix64 output — 16 bits of entropy per value, 2 GB at 10⁹),
and `permutation.bin` is a maximum-entropy permutation because §11.1 forbids
Morton-ordered entity IDs (leak C6); probes §4.1.3 measures delta-coding at ~17%
and notes the incompressibility is a consequence of the current row layout, not
a property of permutations. Neither is a candidate for a codec under any mark
budget.

**`morton.u64` is half empty.** Contracts §2.5 stores a 32-bit code low-aligned
in a u64. The code is 32 bits because §5.2 fixes the grid at 2¹⁶ × 2¹⁶ — a
property of the *grid*, not the population, so it does not change at 10¹⁰ or
10¹¹. Narrowing to u32 saves 4 GB at 10⁹ at zero decode cost and constrains only
future grid depth beyond 16, which nothing currently wants.

**`entity_id` must stay u64.** Considered and rejected during this design.
Contracts §1 already caps entity IDs below 2³² in `bundle_format = 1`, with
"§16's exhaustion answer will bump the format". Narrowing the stored column
would bake that ceiling into the hot layout: entity IDs are monotone and never
reused under I9, so deletions and §12.5 partition moves burn headroom, and
4.29e9 is only ~4× a 10⁹ corpus before churn. At 10¹⁰ it exhausts outright.
Keeping u64 makes the eventual format-2 migration a no-op for that column.

## 3. What is not known

Four ceilings. The viable mark budget is the minimum of them, and no two are
measured by the same probe.

1. **GPU render.** Fill rate and picking overhead — not VRAM, which is not
   close: 10⁷ × 12 B is 120 MB against a 10 GB card.
2. **Transport and decode.** Arrow IPC bytes on the wire, plus JS-side decode to
   GPU-ready typed arrays.
3. **Handle table (I10).** Per-session `u32` handle table at N entries, and
   whether panning mints without bound. This ceiling is specific to Tessera —
   no comparable system mints an opaque per-session identity for every drawn
   mark — and it is therefore the one with no prior art to fall back on.
4. **Gather and selection.** Column read throughput at ~1% density over 10⁹, and
   top-k cost as k approaches the visible count.

Present belief, recorded so the probes can falsify it: it binds at **transport
or the handle table**, not at GPU or storage. This is a guess.

## 4. The probes

Hardware for all four: the existing box — RTX 3080 10 GB, 39 GB RAM, 12 cores,
WSL2 (browser on the Windows host). Corpus: the 10⁹ scaled corpus at
`data/scaled/`.

### P1 — GPU render ceiling

*Needs no Tessera code.*

deck.gl `ScatterplotLayer` over synthetic points, sweeping N from 10⁵ to 3×10⁷.
Four arms: with and without GPU picking; with and without collision-filtered
labels. Binary attributes throughout — the JS-object path is not the
configuration this system would ship.

**Report:** frame time at each N per arm; the N at which frame time crosses 16 ms
and 33 ms; VRAM at each N; whether picking or labels dominates the degradation.

**Decides:** the upper bound on any budget, and whether picking must become
optional above some N.

### P2 — Transport and decode ceiling

*Needs no Tessera code — a static server emitting the contracts §3 point schema
is enough.*

Serve N points as Arrow IPC `(handle: uint32, x: float32, y: float32, …scalars)`,
sweeping N over the same range. Measure separately: bytes on the wire, transfer
time, and `apache-arrow` decode time to GPU-ready typed arrays. Arms:
uncompressed IPC buffers (the contracts §7 rule) against LZ4 and ZSTD IPC buffer
compression, over both localhost and a LAN hop.

**Report:** end-to-end ms to first drawable buffer at each N and arm; the split
between transfer and decode; the N at which end-to-end crosses 100 ms and 1 s.

**Decides:** whether the budget is transport-bound; whether contracts §7's
"buffers are uncompressed for zero-copy slicing" should hold on the *wire* as
well as on disk — a separate question from §10.3, and one this probe answers
directly; and whether prefix-then-extend delivery is necessary rather than
merely available.

### P3 — Handle table ceiling and lifecycle

*Needs a model, not the engine.*

Two halves. The measurement: per-session handle table footprint and mint rate at
N from 10⁵ to 3×10⁷, for each candidate representation in §5. The design
question is in §5 and must be answered before the Phase 1 allocator is written.

**Report:** bytes per session at each N per representation; mint throughput;
growth under a simulated pan trace across the 10⁹ corpus at 1%, 10% and 50%
coverage.

**Decides:** the per-session memory cost of the mark budget, which multiplies by
concurrent sessions and lands directly on the §13.1 residency ceiling.

### P4 — Gather and selection

*Needs Phase 1's columns. This is probes §3.5's owed probe, re-scoped to large k.*

Two reads, measured separately and never conflated:

- **Priority read** over tile-sized row ranges, scattered against
  signature-clustered visible sets, swept over coverage 0.01%–100% and tile
  depth. At full coverage over full extent this is a scan of the entire 2 GB
  priority column.
- **Output gather** of x/y/entity_id/node_id/scalars at k from 10³ to 10⁷,
  measured both column-major (the current layout) and with `priority` split into
  its own file; and uncompressed against delta/FoR-encoded x and y.

**Report:** rows/second for each read at each point in the sweep; the k at which
the gather ceases to be page-sparse; measured compression ratio and decode
throughput for Morton-ordered x/y; whether splitting `priority` into its own file
changes the warm working set materially.

**Decides:** the storage layout. Specifically whether §10.3's uncompressed rule
should be qualified per column group, and whether signature-major row layout
(plan §14) is worth its cost — this probe is input 2 of the three that §14 names.

## 5. The handle-accumulation decision

Owed in Phase 1, because §5 lists the handle allocator as Phase 1 scope and this
cannot be retrofitted after the wire format ships.

**The constraint that rules out the easy answer.** Tile-caching clients (deck.gl's
`TileLayer`) hold a tile's payload across pans, so a handle must stay valid as
long as the client holds the tile carrying it. Minting fresh per request breaks
`POST /v1/items/{handle}` for any cached tile.

**What SA §4.5 already fixes**, and which no option may violate: a handle decodes
to an index into the worker's per-session handle table, **never** an entity ID;
handles are per-session-keyed and structureless to the client; entity IDs cross
no process boundary.

Three candidate representations:

| | Mechanism | Per-session cost at 10⁷ | Risk |
|---|---|---|---|
| **A** | Stable-for-session table, never freed | Forward + reverse maps, ~80–160 MB, grows without bound across a long pan session | Simplest; the growth is the problem |
| **B** | Table scoped to the pin/generation, freed when the pin drains | Bounded by the working set | Handle validity becomes pin-scoped — a cached tile outliving its pin gets `410`, which the client must handle |
| **C** | Derived, not stored — handle = keyed format-preserving permutation over the row ID | Zero; O(1) both directions, no table | **Changes an I10-adjacent property**: a bijection means every 32-bit value decodes to a real row, so contracts §3's `404 unknown` ("nothing is enumerable") weakens to "everything decodes, then is mask-checked". Needs independent review before adoption, not just measurement |

C is the only one that makes the cost vanish rather than bounding it, and it is
also the only one that touches an invariant. It must not be adopted on
performance grounds alone.

**Open question this spec does not answer:** whether handle stability is required
*within* a session across pan-away-and-return, or only for as long as the client
holds the tile. A is required for the former; B suffices for the latter. This is
a client-contract question and should be settled with the visualisation
architecture before P3 arms are finalised.

## 6. Phasing

| Work | When | Why there |
|---|---|---|
| P1, P2 | **Now, parallel to Phase 1** | Need no Tessera code, and they set the k at which Phase 1's own exit criterion should be measured |
| P3 measurement + §5 decision | **Phase 1** | §5 already scopes the handle allocator to Phase 1; the decision cannot be made after the wire ships |
| `morton` u64 → u32 | **Phase 1** | Phase 1 writes the tiler and the format reader; doing it later is the same edit twice plus a format bump |
| P4 | **Post-Phase-1, pre-Phase-2** | Needs real columns at 10⁹, which is Phase 1's output. probes §3.5 already schedules it here |
| Storage layout decisions | **Phase 2**, gated on P4 | Column-group split, per-group codec policy, and §14's signature-major decision all wait on the same numbers |

**The argument against deferring P1 and P2.** §5's exit criterion is "p99 viewport
latency under 10 ms against the 10⁹ corpus with a real mask applied" — at an
unstated k. If the viable budget is 10⁵–10⁷ and Phase 1 measures at ~10⁴ total,
Phase 1 passes against a mark budget the product does not want, and the wire
format, handle allocator and column layout are all revisited afterwards. The
walking skeleton would prove the wrong thing. P1 and P2 are days of work and
remove that risk entirely.

## 7. Residency, restated

Not a probe, but the reason the numbers matter. Ordering the structures by access
cadence rather than by size gives a much smaller resident floor than §10.5
implies, at 10⁹:

| Cadence | Structures | Size |
|---|---|---|
| Per viewport, scanned | `priority` | 2 GB |
| Per viewport, gather | x, y, entity_id, node_id, scalars | 24 GB, scanned at large k |
| Per viewport, sparse | `morton` | 4 GB, ~30 pages/tile touched |
| Per session | term index (~10⁴ postings of 117M), `permutation.bin` (full scan when used) | ~22 GB |
| Live | projected masks, handle tables | per session |
| Cold | temporal group, `pairs.parquet` | 22 GB |

Two corrections fall out. §10.5's "serving nodes hold the term index resident"
reads as a per-viewport claim and is not one — mask build is per *session* and
reads only the ~10⁴ postings the principal satisfies. And probes §3.2's "the
permutation must stay cached" refers to the **projected mask** per
*(token, slice, pin)*, not to `permutation.bin`, which is read once per session.

At small k the resident floor is `priority` plus live session state. **At a 10⁷
mark budget the gather columns join it**, which is what makes the budget a
residency question and not only a latency one.

## 8. Corrections owed to the corpus

Independent of any probe result:

1. **Appendix A double-counts the permutation.** It lists "Permutation arrays,
   both directions | 8 GB". Contracts §2.6 stores only `entity_to_row: u32 ×
   bound` (4 GB at 10⁹); the row→entity direction is the `entity_id` column,
   already counted in the 24 GB hot column set.
2. **probes/optimisations.md §4.4 carries a stale trigger.** It gates
   signature-major on "a real-label signature histogram", which design r18
   retired permanently. Plan §14 already resolved this correctly — demoting the
   histogram to deployment guidance and keeping the conformance suite and the
   gather probe as the live gates. §4.4 should be aligned to §14.
3. **§13.2's "the render path is invariant" is now a claim under test**, not a
   settled property. It should say so until P1 and P2 report.
4. **Appendix A's wire budget** (0.6 MB / 50k points) is stated as a figure
   rather than as a measurement at an assumed k. P2 replaces it.

## 9. Success criteria for this spec

This spec succeeds when: P1–P3 have reported and a server-side k cap is set from
their minimum rather than assumed; the handle representation in §5 is decided and
recorded with its reasoning; the four §8 corrections are applied; and P4 is
scheduled with the k range that P1–P3 established. It does **not** attempt to
decide the storage layout — that is Phase 2, gated on P4.
