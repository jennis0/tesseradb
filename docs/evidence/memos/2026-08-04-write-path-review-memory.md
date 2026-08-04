# Write-path review — performance/memory lens

**Status:** Evidence — review transcript, never normative. One of three independent reviews of
`docs/design/write-path.md` (then r2); companions:
[fidelity](2026-08-04-write-path-review-fidelity.md),
[invariants](2026-08-04-write-path-review-invariants.md). Commissioned on the owner's question:
*"the implications of flush/merge on the memory consumption of the DB during those processes."*
**Dispositioned the same day**: the figures joined write-path §2.1/§4.3/§7/§12; finding 3's
skipped-tick plan build was fixed in code; finding 1's byte gauge was put to the owner and ruled
not needed yet. The corpus records **no peak-memory measurement for any maintenance event** —
every row below is modelled from the code's data structures unless marked measured.

## Peak transient per event

"Transient" = bytes above the steady-state resident set, at the shipped defaults. `B` = deep
bytes of the ingest buffer; at the 1,000,000-item bound with fixture-shaped items,
`B ≈ 0.25–0.35 GB` (modelled; the only adjacent measurement is the clone's *time* — 165 ms p50,
210–437 ms max at ~1–1.34 M items, deny-ack baseline memo).

| Event | Peak transient | Notes |
|---|---|---|
| Flush **plan** (executor) | ≈ 1×B | deep clone of every non-deleted buffered item |
| Flush **execute** (pool) | ≈ 2–2.5×B | plan + row copy + items ×2 at the sort + Arrow buffers + digest read-back (~500–600 B/item at peak) |
| — **promoting** flush only | + 2× dictionary map | `Dict::extended_with` clones the whole lookup map: **7.1 GB per copy measured at 1.17×10⁸ terms** (`probes/2026-08-03-dict-fst/`); both dictionaries resident until the superseded generation drops; ~12 GB/copy modelled at the 2×10⁸ declared bound. **The write path's largest single memory term**; the ratified FST base shrinks the copy ~9× |
| Flush **publication** (executor) | ≈ 1×B + MBs | full buffer clone before removals; manifest clone + JSON Θ(segments + deny set) — ~30–60 MB/write at 10⁶ denies, modelled |
| Every window close (ingest or deny) | ≈ 1×B resp. 1× overlay | the steady-state doubling under sustained ingest: old + clone once per window and again per tick |
| **Worst overlap** (flush executing + window closing) | ≈ 3.5–5×B ≈ **0.9–1.7 GB** | + the dictionary term if promoting |
| **Merge execute** (built, unwired) | ≈ **5–7× Σ input file bytes** | all inputs decoded to items at once (~260 B/row vs ~45 on disk), doubled at the sort; a 256 MiB cap models to ~1.3–1.8 GB pool transient |
| Tier coalesce | ≈ 2–3× pair bytes | the union map holds every input tier's pairs at once |
| Cache patch window | bound + admission × entry | the 2 GiB bound genuinely caps map-resident bytes (evict-to-fit verified); process peak = bound + 48 × 125.12 MB ≈ 8 GB worst case — a formula the cache states itself |
| 0044 background refresh (unbuilt) | ≈ bound + one entry | sequential walk; **not** 2× bound, provided one source Arc is held at a time |

**What `max_merged_segment_bytes` actually bounds:** the selection-time per-segment byte sizes —
neither the decoded resident set (~5–7× larger) nor the output; `execute_merge` itself enforces
nothing.

## Findings

1. **The buffer is bounded in items, not bytes — the one genuinely unbounded transient operand.**
   Per-row bytes are capped only by the 16 MiB per-batch ceiling; every maintenance transient is
   a multiple of buffer *bytes*; `/control/status` publishes a count. *(Byte gauge ruled not
   needed yet, 2026-08-04.)*
2. **A promoting flush clones the entire dictionary map, and the corpus did not record it** —
   larger than the whole projection cache at the FST probe's measured scale. *(Now at write-path
   §4.3.)*
3. **The tick paid the plan even when it published nothing**: full deep-copy plans built before
   the `flush_in_flight` check, dropped on a skip — an O(B) allocate-and-free per tick for a
   gauge on exactly the stalled node. *(Fixed: clone-free count on the skip path.)*
4. **The steady-state doubling under sustained ingest is real and was already stated honestly**;
   no place where write-path.md stated a memory cost the code contradicts — the gap was that it
   stated almost no memory figures at all.
5. **Monotone, unalarmed terms**: the deny lane's queue (stated, accepted); the overlay (alarm
   only, stated); the partition manifest's `files` + deny arrays, cloned and JSON-serialised per
   publication with no bound or alarm on manifest size.
6. **At a full cache, the 0044 refresh's transient 2×-per-key residency makes LRU evict
   not-yet-refreshed *sources***, degrading those keys to full rebuilds — a
   refresh-effectiveness note for the mechanism's design, not a memory bug.

## Probe recommendation

P1–P4 (merge review memo) are all time-shaped; **none records memory**. The single probe that
most reduces uncertainty: one instrumented cycle — ingest to the 1 M bound → `/control/flush` →
publication, with one promoting cell against a large dictionary — sampling `VmHWM`/heap deltas.
That pins the shared per-item multiplier and the dictionary-clone term in one run; the merge
multiplier rides the same instrumentation inside P4 when Task 22b wires a caller.
