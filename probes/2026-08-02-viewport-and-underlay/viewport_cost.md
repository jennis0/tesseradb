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

### 2. Cost tracks the visible set touched, not the tile count

Decisively, and it is the most useful thing here. At 10⁹, `everything`, full extent:

| depth | tiles | marks | wall |
|---|---|---|---|
| 0 | 1 | 21 | **1,270 ms** |
| 1 | 4 | 72 | 1,307 ms |
| 2 | 16 | 269 | 794 ms |
| 3 | 64 | 1,104 | 247 ms |
| 4 | 256 | 4,227 | 202 ms |
| 5 | 1,024 | 16,837 | 200 ms |
| 6 | 4,096 | 66,398 | 227 ms |
| 7 | 16,384 | 264,077 | 411 ms |
| 8 | 65,536 | 1,040,729 | 820 ms |

**Cost falls by 6× going from 1 tile to 256 tiles.** More tiles is *cheaper*, up to a broad minimum
around depths 4–6, after which the gather dominates and it climbs again. The shape is confirmed by
the zoomed-in case: at *f* = 1/16, depth 8 asks for 4,225 tiles and costs **30 ms** — the cheapest
cell measured for that principal — because the ranges touch a small part of the visible set.

The consequence for the client is the opposite of the intuition the tile-addressed design encodes:
**asking for more, finer tiles is not a cost to be rationed.** It buys marks and saves time
simultaneously, until the payload becomes the constraint.

### 3. `marks ≈ m_target · f · 4^d` holds, to about 1%

The formula the depth-choice rests on. Predicted `16 · 4^d` against measured, full extent,
`everything`:

| depth | predicted | 2m4 | 1e8 | 1e9 |
|---|---|---|---|---|
| 6 | 65,536 | 66,230 | 60,366 | 66,398 |
| 7 | 262,144 | 262,589 | 223,225 | 264,077 |
| 8 | 1,048,576 | 1,025,467 | 855,098 | 1,040,729 |

Within ~1% at 2m4 and 1e9; 1e8 runs 8–18% under, which is a clustering effect (fewer non-empty
tiles), not a modelling error. **The count is independent of corpus size** — 66 k marks at depth 6
whether the corpus holds 2.4 M items or 10⁹ — which is exactly what an anchor on `V_total`
predicts, and it means a mark budget is a portable constant rather than a per-deployment tuning.

Two riders worth carrying into the implementation:

- **Use non-empty tiles, not resolved tiles, when reasoning about marks.** At 2m4 depth 9, 262,144
  tiles resolve but only 54,157 are non-empty. Marks track the latter.
- **The client should close the loop rather than trust the formula.** It has the actual mark count
  in every response; one proportional correction to the depth estimate absorbs clustering without
  any model of it.

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
