# What one viewport-addressed request costs

**Date:** 2026-08-01 · **Harness:** `crates/tessera-bench/src/bin/viewport_sweep.rs`
**Raw:** `sweep2m4.csv`, `sweep1e8.csv`, `sweep1e9.csv` · **Machine:** WSL2, 47 GB, 14 compute threads
**Config:** server defaults — `k_max_marks = 500`, `theta_target_marks = 16`, `k_min = 2`,
`max_tiles_per_request = 262,144`. Measured against `Engine` directly, no HTTP.

Reading note: `server_us` is wall clock. `count/select/gather` are **cross-worker CPU sums** above
the parallel threshold, so they legitimately exceed it — see `tessera-server`'s `stage_header` doc.
Every figure excludes the row-projection warm-up, which is reported separately.

---

## The three rulings the plan asked for

### 1. The largest request answerable quickly — and the honest caveat

The plan's stop-condition was *"if fewer than ~500 tiles are answerable in under 150 ms, the
one-request-per-view design does not survive."* **It survives comfortably.**

| fixture | principal | depth 6 (4,096 tiles) | marks returned |
|---|---|---|---|
| 2m4 (2.4 M visible) | everything | **7 ms** | 66,230 |
| 1e8 (53.3 M visible) | everything | **42 ms** | 60,366 |
| 1e9 (42.0 M visible) | broad | **96 ms** | 67,169 |
| 1e9 (518.5 M visible) | everything | **227 ms** | 66,398 |

4,096 tiles is 8× the threshold, at every scale. **The caveat: 150 ms is not reachable at 10⁹ for
the broadest principal at any depth** — the minimum over all depths is ~200 ms, at depths 4–6. For
every other principal and fixture it is met with room.

That 227 ms should be read against the alternative rather than against zero. The tile-addressed
MVP paid **1,276 ms for a single depth-0 tile** at the same scale, and needed one request per tile.
One 227 ms request replaces thousands of those.

### 2. Latency falls with depth; CPU rises. Both, and the distinction is load-bearing

*Corrected 2026-08-01 after independent review. An earlier draft of this section read "cost falls
by 6× going from 1 tile to 256 tiles — more tiles is cheaper", which is true of wall clock and
false of work done. The problem this workstream exists to fix is a throughput gate, so the wrong
half was emphasised.*

At 10⁹, `everything`, full extent. CPU is `count + select + gather`, which above the parallel
threshold is a cross-worker sum rather than a partition of wall clock:

| depth | tiles | marks | **wall** | **CPU** |
|---|---|---|---|---|
| 0 | 1 | 21 | 1,270 ms | 1,267 ms |
| 2 | 16 | 269 | 794 ms | 1,332 ms |
| 3 | 64 | 1,104 | 247 ms | 1,648 ms |
| 4 | 256 | 4,227 | **202 ms** | 2,087 ms |
| 6 | 4,096 | 66,398 | 227 ms | 2,503 ms |
| 8 | 65,536 | 1,040,729 | 820 ms | 8,372 ms |

**Wall falls 6×; CPU rises monotonically, 2× by depth 6 and 6.6× by depth 8.** The wall-clock fall
is the parallel sweep engaging, not work disappearing. Any claim that deeper requests are
"cheaper" without qualification is wrong.

### 2b. Under concurrency — the measurement that decides it

Single-client latency on idle cores cannot answer a throughput question. 1e8, `everything`, full
extent, one session per thread (`concurrency1e8.csv`):

| depth | threads | p50 | p95 | req/s | **marks/s** |
|---|---|---|---|---|---|
| 0 | 1 | 125 ms | 126 ms | 8.0 | 120 |
| 0 | 8 | 151 ms | 177 ms | **49.6** | 744 |
| 4 | 8 | 165 ms | 225 ms | 41.3 | 179,125 |
| 6 | 1 | 37 ms | 40 ms | 27.5 | 1,662,380 |
| 6 | 4 | 123 ms | 137 ms | 31.9 | 1,925,298 |
| 6 | 8 | 172 ms | 454 ms | **31.2** | **1,886,349** |

Two things are true at once, and both belong in the design:

- **Depth 6 is CPU-saturating.** Throughput plateaus at ~31 req/s from two threads onward on 14
  compute threads, and p50 degrades 4.7× from 37 ms to 172 ms as clients arrive. Depth 0 scales
  almost linearly to 49.6 req/s instead — because a one-tile request is serial and eight of them
  simply use eight cores.
- **Normalised by work delivered, it is not close.** Depth 6 returns **1.9 M marks/s against depth
  0's 744** — 2,500×. Depth 0's superior request throughput is throughput of requests that return
  21 marks each.

**The conclusion for the plan.** Viewport-addressed requests are right, and the reason is
marks-per-second rather than latency. But a deployment serving many concurrent broad-principal
viewers is sizing for CPU, not for request count, and the client should expect p50 to degrade under
load rather than assume the single-client figure. **The depth floor should be argued from marks
delivered, not from "deeper is cheaper", which is false.**

### 3. `marks ≈ m_target · f · 4^d` holds, to about 1%

The formula the depth-choice rests on. Predicted `16 · 4^d` against measured, full extent,
`everything`:

| depth | predicted | 2m4 | 1e8 | 1e9 |
|---|---|---|---|---|
| 6 | 65,536 | 66,230 | 60,366 | 66,398 |
| 7 | 262,144 | 262,589 | 223,225 | 264,077 |
| 8 | 1,048,576 | 1,025,467 | 855,098 | 1,040,729 |

Within ~1% at 2m4 and 1e9; 1e8 runs 8–18% under. **The count is independent of corpus size** —
66 k marks at depth 6 whether the corpus holds 2.4 M items or 10⁹ — which is what an anchor on
`V_total` predicts.

**But that table is one configuration, and the model degrades outside it.** *(Added 2026-08-01
after review: the harness collected `f ∈ {1, 1/4, 1/16}` precisely to check the independence claim,
and the first draft of this section reported only `f = 1`. The f-sweep was collected and not
reported, which is the worst way to be wrong.)*

Marks per resolved tile, 1e9, `everything`, depth 8:

| *f* | tiles | marks | per tile |
|---|---|---|---|
| 1 | 65,536 | 1,040,729 | 15.88 |
| 1/4 | 16,641 | 304,238 | 18.28 |
| 1/16 | 4,225 | 46,265 | **10.95** |

That is −32% to +14% around `m_target = 16`, systematic and monotone in *f* — not ~1%. **The
independence claim holds for broad principals at full extent and degrades to −32% when zoomed in.**

**And it fails outright once the budget exceeds the principal's visible set.** 1e9, `narrow`
(`V_total` = 1,366), *f* = 1:

| depth | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|
| marks | 1,001 | 1,366 | 1,366 | 1,366 | 1,366 | 1,366 |

Pinned from depth 4 onward. The model has no `min(·, V_total)` term and is off by three orders of
magnitude here. Since `V_total` is in every response, the term is free to add — and these are
exactly the sparse principals I7 exists to protect, so getting it wrong is not a rounding matter.

Three riders for the implementation:

- **Add the saturation term.** `marks ≈ min(m_target · f · 4^d, V_total_in_view)`.
- **Neither tile denominator is right.** Resolved tiles run 8% under at 1e8 depth 6; non-empty tiles
  run 19% *over* (3,159 non-empty, 60,366 marks = 19.1 each). §7.2's per-depth inflation term is the
  reason. Do not "fix" the model by swapping denominators.
- **Close the loop, but carefully.** The client has the true count in every response. See the plan's
  Task 2a — and the constraint that calibration must never make the next request *shallower*, or it
  serves a subset of what it just drew.

---

## What this changes in the plan

- **Phase 1 proceeds.** Its premise is measured, not assumed.
- **Depth choice should prefer deeper, not shallower.** The plan's `chooseDepth` walks up from 0
  and stops at the first depth meeting the budget; that is right for marks and also right for
  cost, which was not obvious before this.
- **A 150 ms budget is not universally achievable and should not be promised.** At 10⁹ with the
  broadest principal, ~200 ms is the floor. Either the target is stated per scale, or the client
  shows the latency rather than pretending it away.
- **The shallow end stays expensive and is now the only expensive thing.** Depths 0–2 at 10⁹ cost
  0.8–1.3 s regardless of how few tiles they ask for, because the cost is the visible set. A client
  that never requests depth < 3 avoids the entire problem — and by ruling (2) it loses nothing by
  doing so.
