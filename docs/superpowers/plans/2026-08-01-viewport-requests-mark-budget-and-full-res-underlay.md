# Viewport-Addressed Requests, the Global Mark Budget, and the Full-Resolution Underlay

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the client issue one viewport-addressed request per view at a depth chosen to hit a global mark budget, and make the density underlay reach screen resolution — so that marks-on-screen is roughly constant across zoom, one client cannot shed itself with 429s, and the underlay is a density field rather than a mosaic.

**Architecture:** Two workstreams that share one cause. **A** replaces deck.gl's `TileLayer` (tile-addressed, one fetch per tile) with a custom layer issuing one `POST /v1/viewport` per view, choosing depth from `d = log₄(B / (m_target · f))`; client-only, no server change. **B** raises the underlay to screen resolution — which Phase 0 showed needs **no new engine algorithm**, only a guard sized 128× larger and a negotiated dense-raster encoding, because "full resolution" is a property of the viewport and A is what supplies the tiles. Phase 0 measured both premises rather than assuming them, and cancelled a third of the plan.

**Tech Stack:** Rust (`tessera-engine`, `tessera-server`, `tessera-wire`, `tessera-bench`), TypeScript (`clients/ts/core`, `clients/ts/viewer`), deck.gl 9.3, Arrow.

## Global Constraints

- **Governing documents.** Design §7.2 (selection; annotated 2026-08-01), §7.3 (underlay), client-interaction §8.2, §9, §10 (all annotated 2026-08-01). Where this plan and the contracts spec disagree, the contracts spec governs.
- **I7 is not negotiable, and this plan comes near it.** A client choosing which *depth to request* is choosing what to ask for; a client dropping marks to fit a budget would be choosing what is visible. **The budget may only select the request, never filter the response.** Every mark the server serves is drawn.
- **I2**: the underlay's counts are exact masked aggregates. No interpolation between cells, no client-side smoothing that invents a value the server did not report, no estimation of a cell the server omitted (omission means zero, itself a masked count — C18).
- **Nesting (§7.2) is what makes depth-choice safe.** Requesting depth *d* under a shallow view returns a superset of the natural tile's marks, so nothing pops. Any change that breaks nesting invalidates this plan.
- **Additive wire changes only.** A payload that no client asked to change must stay byte-identical, exactly as the underlay's absence does today (`tessera-wire`'s `payload` module doc).
- **British spelling.** Measurements go in `probes/`, not in prose from memory.
- **Current constants**, read 2026-08-01: `k_min = 2`, `k_max_marks = 500`, `max_k = 5000`, `theta_target_marks = 16`, `max_underlay_offset = 4`, `max_underlay_cells = 8192`, `max_tiles_per_request = 262_144`.
- **Fixtures**: `data/bench-fixtures/{2m4,1e8,1e9}`. The 1e9 opens in ~35 s.

---

## Phase 0 is complete, and it changed this plan. Read this first.

Both measurement tasks ran on 2026-08-01 against all three fixtures.
Results: `probes/2026-08-02-viewport-and-underlay/{viewport_cost,underlay_route}.md`.

**Task 1 (viewport cost) — Phase 1 proceeds, with three corrections.**

- The stop-condition was "≥500 tiles in under 150 ms". **4,096 tiles cost 7 ms at 2m4, 42 ms at
  1e8, 227 ms at 1e9** for the broadest principal. Survives with 8× margin.
- **Cost tracks the visible set touched, not the tile count — and falls by 6× from 1 tile to 256.**
  More, finer tiles is *cheaper*. This inverts the intuition the tile-addressed design encodes and
  means depth choice should prefer deeper for cost as well as for marks.
- **`marks ≈ m_target · f · 4^d` holds to ~1%**, and is independent of corpus size — 66 k marks at
  depth 6 whether the corpus is 2.4 M or 10⁹. A mark budget is a portable constant.
- **150 ms is not universally achievable**: at 10⁹ with the broadest principal the floor is ~200 ms
  at any depth. Do not promise it. Read it against the MVP's 1,276 ms for a *single* depth-0 tile.

**Task 5 (underlay) — Tasks 5 and 6 are cancelled; the premise was wrong.**

- **"Full resolution" is a property of the viewport, not the tile.** A view of *T* tiles over a
  10⁶-pixel screen needs `10⁶/T` sub-cells per tile. At Phase 1's operating point (4,096 tiles)
  that is **256 — offset 4, today's cap.** The MVP looked blocky because it fetched *one* tile, not
  because the cap is wrong. **Phase 1 fixes the underlay's resolution as a side effect.**
- **The per-sub-cell algorithm is affordable at the operating point** — 179 ms (1e8) / 466 ms (1e9)
  for a screen's worth. It is *not* cheap everywhere: a single shallow tile costs 152–417 ms at
  offset 9, which is what the annotation actually predicted. Phase 1 stops requesting shallow
  depths, so the route survives where it is used. **No `underlay.rs` for now — but Task 6 is
  DEFERRED, not refuted**: the single-pass alternative was never measured.
- **A whole-screen underlay costs 179 ms against 74 ms without, at 1e8** — in the same request.
- **A whole-screen underlay costs 466 ms against 222 ms without, at 1e9** — it roughly doubles the
  request. Quarter resolution costs 287 ms, so resolution-versus-latency is a real trade the client
  should expose rather than hard-code.
- **The one thing that must change is `max_underlay_cells = 8,192`**, 128× too small for a screen's
  worth (1,048,576). Both 1e8 and 1e9 sustain that cell count in one request, so it is supportable.
- **The sparse encoding loses badly at scale, which settles it**: fill is 53% at 1e8 and **99.2% at
  1e9**, so `(cell u64, count u64)` costs 16.6 MB where a dense `u16` raster costs 2.1 MB — 8×, and
  growing with scale. But fill is **0.3%** for a narrow principal at high offset, so the encoding
  must be **negotiated, not switched**: a dense raster would ship megabytes of zeros to the viewer
  that needs them least.

**Consequently:** Task 1 and Task 5 are done. Tasks 5–6 as originally written are **withdrawn**.
Phase 2 is now Task 7 (guard + encoding) and Task 8 (client), and is much smaller than drafted.
Two findings not previously in the plan are folded in as Task 2a and Task 7's sizing rule.

---

## The arithmetic this plan rests on

From design §7.2's annotation. A viewport covering fraction *f* of the slice holds ≈ `f · 4^d`
tiles at depth *d*, each drawing ≈ `m_target` marks. So:

```
marks(d) ≈ m_target · f · 4^d
d(B)     = log₄( B / (m_target · f) )        choose depth for budget B
tiles(B) = f · 4^d = B / m_target             INDEPENDENT of zoom and of f
```

The last line is the load-bearing one: **a request carries `B / m_target` tiles whatever the
zoom** — 3,125 tiles for `B = 50,000` at `m_target = 16`. `max_tiles_per_request` (262,144) does
not bind. What is unknown is what such a request *costs*, which is Task 1.

---

## File Structure

```
probes/2026-08-02-viewport-and-underlay/
  README.md                       what was measured, and the two rulings it produced
  viewport_cost.md                Task 1 results
  underlay_route.md               Task 5 results
crates/tessera-bench/src/
  bin/viewport_sweep.rs           Task 1 harness
  bin/underlay_route.rs           Task 5 harness
crates/tessera-engine/src/
  viewport.rs                     MODIFY: encoding-selection plumbing only (Task 7)
crates/tessera-wire/src/
  payload.rs                      MODIFY: raster sub-cell stream (Task 7)
crates/tessera-server/src/
  config.rs                       MODIFY: max_underlay_cells reshaped (Task 7)
  viewer.rs                       MODIFY: pass the raster through
clients/ts/core/src/
  budget.ts                       NEW: depth choice + calibration (Tasks 2, 2a)
  types.ts, decode.ts             MODIFY: raster sub-cells (Task 8)
clients/ts/viewer/src/
  viewportLayer.ts                NEW: the custom layer (Task 3)
  map.ts                          MODIFY: use it instead of TileLayer
  underlay.ts                     MODIFY: raster → texture (Task 8)
  panels/budget.ts                NEW: budget readout (Task 4)
```

---

## Phase 0 — COMPLETE (2026-08-01). Retained for the harnesses and the method; rulings are summarised above.

### Task 1: What does one large viewport request cost? — **DONE**

Workstream A is only viable if a single request carrying `B / m_target` tiles is fast enough to
pan on. Nothing measured so far exceeds one tile per request.

**Files:**
- Create: `crates/tessera-bench/src/bin/viewport_sweep.rs`
- Create: `probes/2026-08-02-viewport-and-underlay/viewport_cost.md`

**Interfaces:**
- Consumes: a running `tessera serve`, or `tessera_engine::Engine` directly (prefer direct — it removes HTTP from the measurement).
- Produces: a table of `(fixture, visible_total, depth, tiles_in_request, server_ms, bytes)` and the two derived numbers Task 2 needs: **the largest tile count answerable in under 150 ms**, and whether that figure depends on depth or only on tile count.

- [ ] **Step 1: Write the harness**

Sweep, per fixture in `{2m4, 1e8, 1e9}` and per principal in `{narrow, broad, everything}` (terms from `probes/`'s own preset measurement, re-measured per fixture):

```
for depth in 0..=10:
    bbox = full extent
    request(slice, zoom=depth, bbox, k=default, underlay_offset=0)
    record: tiles_resolved, tiles_nonempty, sigma_visible, points_gathered,
            server_us, bytes
```

then repeat for a viewport covering `f ∈ {1, 1/4, 1/16}` of the extent, so the `tiles = B/m_target`
independence claim is checked rather than assumed.

Use `StageTimings` (the `bench-timing` feature) so `count_ns`, `select_ns` and `gather_ns` are
separable — the question "does cost track tile count or visible count" is answerable only with the
breakdown.

- [ ] **Step 2: Run it against all three fixtures**

Run: `cargo run --release -p tessera-bench --features bench-timing --bin viewport_sweep -- --fixture data/bench-fixtures/1e9`
Expected: a CSV per fixture. The 1e9 run takes minutes, not hours.

- [ ] **Step 3: Write up the two rulings**

`viewport_cost.md` must answer, in numbers:

1. **What is the largest `tiles_in_request` answerable in < 150 ms** (a pan should not feel
   stalled) at 1e9 for the broadest principal?
2. **Does cost track tile count, visible count, or points gathered?** This decides whether the
   budget can be a constant or must adapt per principal.
3. **Is `marks ≈ m_target · f · 4^d` true in practice?** Plot measured `points_gathered` against
   the prediction. A systematic deviation means the depth-choice formula in Task 2 needs a
   correction term.

If the answer to (1) is below ~500 tiles, **stop and report**: the one-request-per-viewport design
does not survive, and the plan needs re-thinking around a hybrid (coalesced groups of tiles rather
than one request). That is a genuine possible outcome, not a formality.

- [ ] **Step 4: Commit**

```bash
git add crates/tessera-bench/src/bin/viewport_sweep.rs probes/2026-08-02-viewport-and-underlay/
git commit -m "probe: what one viewport-addressed request costs across depth and scale"
```

---

## Phase 1 — Workstream A: viewport-addressed requests and the mark budget

Client-only. No server change. Gated on Task 1's ruling (1).

### Task 2: Depth choice from a mark budget

**Files:**
- Create: `clients/ts/core/src/budget.ts`, `clients/ts/core/test/budget.test.ts`
- Modify: `clients/ts/core/src/index.ts`

**Interfaces:**
- Consumes: `Meta`, `tileToCellBox`, `CELL_GRID`, `MAX_DEPTH` from `coords.ts`.
- Produces:
  - `type BudgetInputs = {budget: number; mTarget: number; worldBbox: [number, number, number, number]; maxTiles: number}`
  - `chooseDepth(inputs: BudgetInputs): {depth: number; tiles: number; predictedMarks: number; limitedBy: 'budget' | 'maxTiles' | 'maxDepth'}`
  - `tilesInBbox(worldBbox, depth): number`

- [ ] **Step 1: Write the failing test**

```ts
import {describe, expect, it} from 'vitest';
import {chooseDepth, tilesInBbox} from '../src/budget.js';
import {WORLD_SIZE} from '../src/coords.js';

const full: [number, number, number, number] = [0, 0, WORLD_SIZE, WORLD_SIZE];
const base = {budget: 50_000, mTarget: 16, maxTiles: 262_144};

describe('chooseDepth', () => {
  it('asks for budget / mTarget tiles regardless of how much is in view', () => {
    // The load-bearing property: tile count is independent of zoom and of viewport fraction.
    const whole = chooseDepth({...base, worldBbox: full});
    const quarter = chooseDepth({...base, worldBbox: [0, 0, WORLD_SIZE / 2, WORLD_SIZE / 2]});
    const sixteenth = chooseDepth({...base, worldBbox: [0, 0, WORLD_SIZE / 4, WORLD_SIZE / 4]});
    for (const r of [whole, quarter, sixteenth]) {
      // Depth is an integer, so the tile count lands within a factor of 4 of the target.
      expect(r.tiles).toBeGreaterThan(base.budget / base.mTarget / 4);
      expect(r.tiles).toBeLessThanOrEqual(base.budget / base.mTarget * 4);
    }
    // And it goes DEEPER as less is in view, which is the whole point.
    expect(quarter.depth).toBeGreaterThan(whole.depth);
    expect(sixteenth.depth).toBeGreaterThan(quarter.depth);
  });

  it('never returns a depth outside the grid', () => {
    const deep = chooseDepth({...base, budget: 10 ** 9, worldBbox: full});
    expect(deep.depth).toBeLessThanOrEqual(16);
    expect(deep.limitedBy).toBe('maxDepth');
  });

  it('reports when the tile guard is what capped it, not the budget', () => {
    const capped = chooseDepth({...base, budget: 10 ** 6, maxTiles: 64, worldBbox: full});
    expect(capped.tiles).toBeLessThanOrEqual(64);
    expect(capped.limitedBy).toBe('maxTiles');
  });

  it('predicts marks from the tile count, not from the budget it was asked for', () => {
    const r = chooseDepth({...base, maxTiles: 64, budget: 10 ** 6, worldBbox: full});
    expect(r.predictedMarks).toBe(r.tiles * base.mTarget);
  });

  it('counts tiles in a bbox exactly at every depth', () => {
    expect(tilesInBbox(full, 0)).toBe(1);
    expect(tilesInBbox(full, 1)).toBe(4);
    expect(tilesInBbox([0, 0, WORLD_SIZE / 2, WORLD_SIZE / 2], 1)).toBe(1);
  });
});
```

- [ ] **Step 2: Run it, watch it fail**

Run: `cd clients/ts/core && npx vitest run test/budget.test.ts`
Expected: FAIL — no `../src/budget.js`.

- [ ] **Step 3: Implement**

```ts
import {CELL_GRID, MAX_DEPTH, WORLD_SIZE, type TileIndex} from './coords.js';

export type BudgetInputs = {
  budget: number;
  mTarget: number;
  worldBbox: [number, number, number, number];
  maxTiles: number;
};

export type DepthChoice = {
  depth: number;
  tiles: number;
  predictedMarks: number;
  limitedBy: 'budget' | 'maxTiles' | 'maxDepth';
};

/** How many tiles of depth `depth` a world-space bbox intersects. */
export function tilesInBbox(
  bbox: [number, number, number, number],
  depth: number
): number {
  const span = WORLD_SIZE / 2 ** depth;
  const [x0, y0, x1, y1] = bbox;
  const clamp = (v: number) => Math.min(2 ** depth - 1, Math.max(0, Math.floor(v / span)));
  return (clamp(x1) - clamp(x0) + 1) * (clamp(y1) - clamp(y0) + 1);
}

/**
 * The depth to request so that the whole view draws roughly `budget` marks.
 *
 * §7.2 makes marks-per-TILE depth-stable at ≈ `m_target`, so marks-on-SCREEN is
 * `m_target × tiles-in-view`, and the only lever is which depth's tiles are asked for. Because
 * priority prefixes nest, a deeper request under a shallow view is a superset of the natural
 * tile's marks — nothing pops (§7.2, and the same property §8.2 credits for `best-available`).
 *
 * **This selects the request, never the response.** Every mark the server serves is drawn; a
 * client that dropped marks to fit a budget would be choosing what is visible, which is I7's.
 */
export function chooseDepth(inputs: BudgetInputs): DepthChoice {
  const {budget, mTarget, worldBbox, maxTiles} = inputs;
  const wanted = Math.max(1, budget / Math.max(1, mTarget));

  let chosen = 0;
  let limitedBy: DepthChoice['limitedBy'] = 'budget';
  for (let depth = 0; depth <= MAX_DEPTH; depth++) {
    const tiles = tilesInBbox(worldBbox, depth);
    if (tiles > maxTiles) {
      limitedBy = 'maxTiles';
      break;
    }
    chosen = depth;
    if (tiles >= wanted) {
      limitedBy = 'budget';
      break;
    }
    if (depth === MAX_DEPTH) limitedBy = 'maxDepth';
  }

  const tiles = tilesInBbox(worldBbox, chosen);
  return {depth: chosen, tiles, predictedMarks: tiles * mTarget, limitedBy};
}
```

- [ ] **Step 4: Run the tests**

Run: `cd clients/ts/core && npx vitest run`
Expected: PASS, all suites.

- [ ] **Step 5: Commit**

```bash
git add clients/ts/core/src/budget.ts clients/ts/core/src/index.ts clients/ts/core/test/budget.test.ts
git commit -m "feat(core): choose request depth from a global mark budget

Marks-per-tile is depth-stable by §7.2, so marks-on-screen is m_target x tiles,
and depth is the only lever. Tile count comes out independent of zoom and of how
much is in view. Selects the request, never the response — I7."
```

### Task 2a: Close the loop on the measured mark count

Phase 0 found `marks ≈ m_target · f · 4^d` holds to ~1% at 2m4 and 1e9 but runs 8–18% under at 1e8,
because clustering leaves tiles empty. The client should not carry a clustering model; it already
has the true figure in every response.

**Files:** `clients/ts/core/src/budget.ts`, `clients/ts/core/test/budget.test.ts`

**Interfaces:** Produces `calibrate(previous: {predictedMarks: number; actualMarks: number}, next: BudgetInputs): BudgetInputs` — a proportional correction to `mTarget`, clamped and damped.

- [ ] **Step 1: Write the failing test**

```ts
it('corrects mTarget toward what the server actually served', () => {
  // Predicted 65,536 but only 55,000 arrived: the effective per-tile yield is lower.
  const next = calibrate({predictedMarks: 65_536, actualMarks: 55_000}, {...base, worldBbox: full});
  expect(next.mTarget).toBeLessThan(base.mTarget);
  expect(next.mTarget).toBeGreaterThan(base.mTarget * 0.5); // damped, not a jump
});

it('is a no-op when the prediction was right', () => {
  const next = calibrate({predictedMarks: 65_536, actualMarks: 65_536}, {...base, worldBbox: full});
  expect(next.mTarget).toBeCloseTo(base.mTarget, 6);
});

it('never lets a pathological response drive mTarget to zero or infinity', () => {
  const starved = calibrate({predictedMarks: 65_536, actualMarks: 0}, {...base, worldBbox: full});
  expect(starved.mTarget).toBeGreaterThan(0);
  const flooded = calibrate({predictedMarks: 1, actualMarks: 10 ** 7}, {...base, worldBbox: full});
  expect(Number.isFinite(flooded.mTarget)).toBe(true);
});
```

- [ ] **Step 2: Run it, watch it fail. Step 3: implement with damping and clamps. Step 4: green. Step 5: commit.**

Damping matters: depth is an integer, so an undamped correction oscillates between two depths on
alternate frames — visible as the map flickering between densities while the user does nothing.

### Task 3: The viewport layer

Replaces `TileLayer`. One request per view.

**Files:**
- Create: `clients/ts/viewer/src/viewportLayer.ts`
- Modify: `clients/ts/viewer/src/map.ts`, `clients/ts/viewer/src/state.ts`

**Interfaces:**
- Consumes: `chooseDepth`, `TesseraClient.viewport`, `positionsToWorld`.
- Produces: `buildViewportLayers(store, client): Layer[]`, and `AppState.view: {depth: number; tiles: number; limitedBy: string; requestedAt: number} | null`.

- [ ] **Step 1: Write it**

The shape, with the reasoning that must survive into the code:

```ts
/**
 * One request per view, not one per tile.
 *
 * `POST /v1/viewport` is viewport-addressed: a bbox spanning many tiles returns every tile's
 * counts plus a flat points batch. deck.gl's `TileLayer` is tile-addressed and issues one fetch
 * per tile, which at 1e9 shed 12 of 23 requests from a single tab (client-interaction §8.2's
 * annotation). This layer keeps the verb's own shape.
 *
 * Consequences, all deliberate:
 * - No per-tile cache. A pan refetches the view. That is one request, and the replica store is
 *   where caching belongs (client-interaction §10) — not smuggled in here.
 * - In-flight requests are aborted on view change, so at most one is outstanding. That is the
 *   whole of the coalescing story at this layer.
 * - Points render as ONE binary attribute buffer. `served` still splits the batch per tile for
 *   the counts panel; rendering does not need the split.
 */
```

Behaviour:

1. Debounce `onViewStateChange` by ~120 ms (a pan emits per frame; the request must not).
2. Compute the world bbox from the viewport, clamp to `[0, WORLD_SIZE]²`.
3. `chooseDepth({budget, mTarget: meta.selection.thetaTargetMarks, worldBbox, maxTiles})`, with
   `mTarget` carried forward through `calibrate` (Task 2a) rather than re-read each time.
3a. **Never request depth < 3.** Phase 0's decisive cost finding: at 10⁹ depths 0–2 cost
   0.8–1.3 s *regardless of how few tiles they ask for*, because cost tracks the visible set and
   not the tile count — while depth 4–6 costs ~200 ms and returns three orders of magnitude more
   marks. There is nothing to lose by skipping the shallow end and ~1 s per request to gain. A
   floor of 3 is not a tuning choice; it is avoiding the only expensive region measured.
4. Abort the in-flight request; issue one `client.viewport(token, {slice, zoom: depth, bbox: dataBbox, underlayOffset})`.
5. On success, store the whole result; render one `ScatterplotLayer` over `worldPositions`.
6. On abort, do nothing. On failure, record it (the existing failure surface).

Note the bbox here is a **view** bbox, not a tile bbox, so `tileToRequestBbox`'s half-cell inset does not apply; the inclusive-corner behaviour of `tile_corners` is *wanted* — it is what makes the request cover every tile the view touches. Add a comment saying so, or the next reader will "fix" it.

- [ ] **Step 2: Verify against a live server**

Run the 1e9 fixture and `node smoke.mjs`. Expected, and each is a regression if absent:
- **`/v1/viewport` request count per pan is 1**, not 6–24.
- **Zero 429s** in a normal pan/zoom session.
- Marks drawn stay within a factor of ~4 of the budget across depths 0–8 — the constant-marks
  property, which is the point of the whole task.

- [ ] **Step 3: Record the before/after in the probe write-up**

Append to `viewport_cost.md`: requests per pan and marks-on-screen across zoom, before and after.
The before figures are in `clients/ts/README.md` (17/50/242/456).

- [ ] **Step 4: Commit**

### Task 4: The budget panel

**Files:** Create `clients/ts/viewer/src/panels/budget.ts`; modify `main.ts`.

Shows: budget (a slider), chosen depth, tiles requested, predicted vs actual marks, and
`limitedBy`. The prediction-versus-actual row is the useful one — a persistent gap means the
`m_target · f · 4^d` model is wrong, which is exactly what Task 1 Step 3 (3) set out to check and
what a user will notice first.

- [ ] Steps: write it, wire it, verify by eye, commit.

---

## Phase 2 — Workstream B: the full-resolution underlay

**Much smaller than drafted.** Phase 0 cancelled the algorithm work: the existing per-sub-cell
route is affordable at full resolution, and the resolution problem is caused by the tile count,
which Phase 1 fixes. What remains is a guard and an encoding.

Do not start before Phase 1 lands — the raster's natural shape is per-viewport, and Phase 1 is what
makes requests per-viewport.

### Task 5: ~~Which underlay algorithm~~ — **DONE**; see `probes/2026-08-02-viewport-and-underlay/underlay_route.md`

### Task 6: Implement a single-pass underlay route — **DEFERRED, pending evidence**

Task 5 measured the existing route at the operating point and found it affordable: 179 ms (1e8) /
466 ms (1e9) for a whole screen's worth. **Deferred rather than withdrawn** — the single-pass
alternative was never measured, the route is expensive at shallow zoom (152–417 ms for one tile),
and at 1e9 the underlay doubles the request, which is the margin a chooser would be for. Nothing
here is needed for Phase 1; revisit if the underlay's share becomes the constraint.

The differential-test requirement is withdrawn with the task it guarded — but its *reasoning* moves
to Task 7: **a density field that is subtly wrong still looks like a density field**, so any change
to what the underlay emits needs exact-equality against the current output, never an eyeball.

### Task 7: Raise the guard, and add a negotiated dense raster

**Files:** `crates/tessera-server/src/config.rs`, `crates/tessera-wire/src/payload.rs`,
`crates/tessera-server/src/viewer.rs`, `crates/tessera-engine/src/viewport.rs` (request plumbing only).

**Interfaces:**
- Consumes: `SubCellCount` as today.
- Produces: a request field selecting the sub-cell encoding (`sparse` default, `raster` opt-in), and a raster stream `(cell_origin u64, offset u8, counts u32[])` with geometry implied by `(origin, offset)`.

- [ ] **Step 1: Raise `max_underlay_cells`, and say what it does not bound**

Default to **1,048,576** (one screen's worth; both 1e8 and 1e9 sustain it in one request). The doc
comment must carry the finding that makes it dangerous to read as a cost bound: **the same cell
budget costs `medium` 46 ms of CPU and `everything` 2.9 s at 1e9.** The guard bounds cells, not
cost, and sizing it is a deployment decision about the broadest principal served — §8.1's
per-principal support surprise in a third place. Refuse rather than clamp, per `config.rs`'s
discipline. Test the default and the refusal.

- [ ] **Step 2: Write the failing byte-identity test first**

The un-negotiated payload must stay byte-identical, including the zero-trailing-bytes case. Assert
it against a golden captured before the change.

- [ ] **Step 3: Add the raster encoding behind negotiation**

Sparse stays the default and the only thing an existing client can receive. `u16` counts saturate
rather than wrap, and saturation must be *reported* — a silently clamped count is a wrong masked
aggregate, which is an I2 problem, not a rendering one. If saturation cannot be reported cleanly,
use `u32` and accept 4.2 MB.

- [ ] **Step 4: Exact-equality test across encodings**

Sparse and raster must describe the identical multiset of `(cell, count)` for the same request.
This is Task 6's withdrawn differential requirement, relocated.

- [ ] **Step 5: Commit**

### Task 8: Client raster decode and viewport-wide colouring

**Files:** `clients/ts/core/src/{types,decode}.ts`, `clients/ts/viewer/src/underlay.ts`.

- [ ] Decode the raster into a typed array; keep the sparse path.
- [ ] Upload as a texture with **nearest** filtering (interpolating exact counts invents densities).
- [ ] Apply `eq_hist` over the **whole viewport raster** rather than per tile — which also removes
  the per-tile colour discontinuity the MVP has, since each tile currently equalises against its own
  distribution.
- [ ] Expose the resolution/latency trade measured in Phase 0 (offset 3 ≈ 287 ms, offset 4 ≈ 466 ms
  at 1e9) as a control, rather than hard-coding one of them.
- [ ] Re-run the orientation check from the MVP (correlate the underlay grid against the served
  points' grid; as-is must beat transposed). Cheap, and it is what caught the bit order last time.
- [ ] Commit.

## Self-Review

**Spec coverage.** Client-interaction §8.2's annotation (coalescing, request multiplication) →
Tasks 1, 3. §10's annotation (depth choice, budget) → Tasks 2, 3, 4. §9's annotation (underlay
resolution, evaluation cost, wire encoding, the guard) → Tasks 5, 6, 7, 8. Design §7.2's annotation
(no knob expresses marks-on-screen; the refusal to couple θ to the viewport) → Tasks 2 and 4, and
the constraint that θ is untouched is stated in the Global Constraints.

**Deliberately not covered.** Retry-on-429 with backoff (client-interaction §8.2 annotation, item
3) — it belongs with the replica store and is not needed once one request is outstanding at a time;
per-view caching; epochs; the tile-addressed GET alias's scale caveat, which is a documentation
change to make once Task 3 has proved the alternative.

**The known risk was retired by measurement.** As drafted, Task 1 could have killed Phase 1 (a slow
large request trades 429s for a stalled pan) and Task 5 could have answered "full resolution is not
affordable". Both ran: the first survives with 8× margin, the second cancelled its own follow-on
task. What replaces the risk is a scoping correction — **~200 ms, not 150 ms, is the floor at 10⁹
for the broadest principal**, and the client should show that latency rather than promise it away.

**Residual risks now that Phase 0 is in.**
- *The underlay doubles the request at 10⁹* (222 → 466 ms at full resolution). Task 8 exposes the
  resolution control rather than hard-coding it, but a deployment serving broad principals may want
  the underlay off by default at that scale. Not decided here.
- *`max_underlay_cells` bounds cells, not cost*, and the same budget differs by ~60× in CPU between
  a narrow and a broad principal. Task 7 Step 1 documents it; it is not solved by it.
- *`u16` raster counts can saturate.* Task 7 Step 3 requires saturation be reported or the type
  widened — a silently clamped count is a wrong masked aggregate, an I2 problem rather than a
  rendering one.

**Type consistency.** `chooseDepth`/`tilesInBbox`/`BudgetInputs`/`DepthChoice` are defined once in
`budget.ts` and used under those names in Tasks 3 and 4. `tileToRequestBbox` is *not* used by the
viewport layer, and Task 3 says why. `subCellsToImage` survives Task 8 with a raster input.
