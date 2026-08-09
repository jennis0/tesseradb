import {WORLD_SIZE} from './coords.js';
import {MIN_DEPTH, chooseDepth, tileRectOfBbox, type DepthChoice} from './budget.js';
import {rectArea, type TileRect} from './rects.js';

/**
 * Layer 2: deciding *which* tiles to want.
 *
 * Split from the replica because the two answer different questions and a consumer may want only
 * one of them. The replica answers asks; this decides what to ask for, and a consumer with its own
 * tile scheduler — deck.gl's `TileLayer`, a MapLibre source — drops this layer entirely and keeps
 * the cache. Everything here is pure: no timers, no fetch, no clock. The scheduling that drives it
 * lives with the caller, which is what makes the policy testable at all.
 *
 * **Regions, not tile lists.** A viewport is a rectangle and a plan is a rectangle; enumerating the
 * tiles inside one costs O(tiles) — measured at 181 ms for a 262 144-tile ring — to produce a shape
 * four integers already describe. The replica subtracts what it holds as rectangles too, so no
 * layer of this ever materialises a tile set.
 *
 * **Presentation, never selection.** Which tiles a client asks for is its own business; which marks
 * it is *served* for a tile is the engine's (I7). Requesting a deeper tile than the viewport
 * strictly needs is sound because §7.2's prefixes nest — a deeper answer is a superset of the
 * shallower one, so nothing pops when the user arrives there — and it is why look-ahead can be a
 * client-side choice at all.
 */

export type Viewport = {
  /** deck.gl world-space centre. */
  target: [number, number];
  /** `log2` pixels per world unit. */
  zoom: number;
  width: number;
  height: number;
};

export type PlannerInputs = {
  viewport: Viewport;
  /** Target marks on screen. */
  budget: number;
  /** Calibrated marks-per-tile. */
  mTarget: number;
  maxTiles: number;
  /** The previous response's visible count, the saturation term for depth choice. */
  visibleInView?: number;
  /** World units per millisecond, signed, from recent movement. Biases the ring downwind. */
  velocity?: [number, number];
  /** What the replica holds and what it may hold — sizes the ring. See {@link ringMargin}. */
  heldBytes?: number;
  budgetBytes?: number;
};

/** One region to ask about, in tile-index space at `depth`. */
export type PlannedFetch = {
  kind: 'visible' | 'foreground' | 'ring' | 'deeper';
  depth: number;
  rect: TileRect;
};

export type Plan = {
  choice: DepthChoice;
  /** Exactly what is on screen. Issued first, so the screen fills before the margin is bought. */
  visible: PlannedFetch;
  /** The screen plus its prefetch margin. Issued second; the overlap subtracts away. */
  foreground: PlannedFetch;
  /** What to draw: everything held over a wider box. See {@link RENDER_MARGIN}. */
  render: TileRect;
  /** Anticipatory, issued only while the view is still, and abandoned the moment it moves. */
  background: PlannedFetch[];
};

/**
 * How much beyond the visible box the foreground request covers.
 *
 * Costs `MARGIN²` in tiles (1.3 → 1.69×) and buys the common interaction for free: measured, most
 * drags move the view by well under 30% of its width. The alternative — requesting exactly the
 * visible box — guarantees a round trip for every pixel of movement.
 */
export const MARGIN = 1.3;

/**
 * How far beyond the visible box the *drawn* buffer reaches.
 *
 * **Wider than {@link MARGIN}, because drawing and fetching are bounded by different things.** What
 * to fetch is limited by what the budget will pay for; what to draw is limited only by what is
 * already held, and re-assembling costs milliseconds where a round trip costs hundreds. Drawing
 * only the fetched box puts the edge of the marks 30% beyond the screen, so a pan of more than 15%
 * of the viewport runs off it and waits for a re-assembly — measured at 65–197 ms with **zero**
 * requests, which is pop-in with a fully warm cache and nothing to fetch.
 *
 * Costs `RENDER_MARGIN²` in marks drawn, and deck.gl takes binary attributes so the marks are a
 * buffer upload rather than per-mark work.
 *
 * **Sized against the gesture, not against the screen.** Escaping the buffer costs a full rebuild
 * and a fresh upload of every mark; staying inside it costs nothing at all, because deck re-projects
 * what is already there. So the question is how far a gesture travels: an aggressive pan moves a
 * third to a half of a viewport, and at 1.8 the buffer reached only 0.4 widths beyond the screen —
 * exactly short of it, which put a rebuild on the interaction users make most deliberately. At 2.6
 * it reaches 0.8.
 *
 * The trade is marks drawn against rebuilds avoided: `RENDER_MARGIN²` more marks in the buffer, and
 * the rebuild happens less often. The redraw itself is ~1 ms at 6 × 10^4 marks, so the cost that
 * matters is the GPU upload, which is paid per rebuild rather than per frame.
 */
export const RENDER_MARGIN = 2.6;
/**
 * How far the anticipatory ring reaches when the replica is nearly full.
 *
 * The floor, not the figure: see {@link ringMargin}.
 */
export const RING_MARGIN = 2.2;

/** How far it reaches when the replica is empty. */
export const RING_MARGIN_MAX = 6;

/**
 * Fill the replica to this fraction of its budget before the ring stops growing.
 *
 * Below it the cache is not the scarce resource and a wider ring is close to free — the marks are
 * held either way, and the only extra cost is fetching ground the user may not reach. Above it a
 * wider ring would evict what it just bought.
 */
export const RING_FILL_TARGET = 0.6;

/**
 * How far the ring should reach, given how full the replica already is.
 *
 * **Sized against the cache budget rather than fixed**, because a fixed multiple is wrong at both
 * ends. Measured on the demo corpus at the broad principal: points cost ~48 B each, so a 512 MB
 * budget holds ~10.7 × 10^6 of them — and a 2.2× ring left the replica at 41 MB and 8 × 10^5 points
 * after a dozen pans, 8% of what it was given. The cache was never the constraint; the ring was,
 * and it was fetching a thin margin and then waiting to be asked again.
 *
 * Grows as `RING_MARGIN_MAX` down to `RING_MARGIN` as the replica fills, so an empty cache is
 * aggressive and a full one stops buying what it would have to evict. The reach is spent on
 * *coarser* bands rather than more fine ones — see {@link plan} — so a wide ring is affordable.
 *
 * **It is bought with server work, and the exchange rate is steep.** Measured over six pans on the
 * demo corpus, against the fixed 2.2× ring: four of six pans needing no request instead of three,
 * and 2.5 × 10^6 points held instead of 8 × 10^5 — for **6× the server CPU** (33 ms → 207 ms) and
 * **4× the bytes** (9.1 MB → 35.7 MB). Worth it for one principal on a dedicated box; against
 * `caching.md` §4's ceiling of a handful of concurrently active broad principals it is not
 * obviously worth it at all, and `RING_MARGIN_MAX` is the dial.
 */
export function ringMargin(heldBytes: number, budgetBytes: number): number {
  if (budgetBytes <= 0) return RING_MARGIN;
  const fullness = Math.min(1, heldBytes / (budgetBytes * RING_FILL_TARGET));
  return RING_MARGIN_MAX - (RING_MARGIN_MAX - RING_MARGIN) * fullness;
}
/**
 * How far a velocity of one viewport-width per second shifts the ring, as a fraction of the
 * viewport. Bounded well under 1 so a fast flick biases rather than abandons the current view.
 */
export const VELOCITY_BIAS = 0.5;

/** The world-space box a viewport covers, expanded by `margin` and clamped to the world. */
export function worldBbox(
  viewport: Viewport,
  margin = 1,
  shift: [number, number] = [0, 0]
): [number, number, number, number] {
  const scale = 2 ** viewport.zoom;
  const halfW = (viewport.width / 2 / scale) * margin;
  const halfH = (viewport.height / 2 / scale) * margin;
  const cx = viewport.target[0] + shift[0];
  const cy = viewport.target[1] + shift[1];
  const clamp = (v: number) => Math.min(WORLD_SIZE, Math.max(0, v));
  return [clamp(cx - halfW), clamp(cy - halfH), clamp(cx + halfW), clamp(cy + halfH)];
}

/**
 * What to ask for, given a viewport.
 *
 * **Depth is chosen for what is visible, and the margin is then fetched at that depth.** Choosing
 * it for the margined box instead would spend the budget on off-screen marks and quietly lower the
 * resolution of what the user is actually looking at.
 */
export function plan(inputs: PlannerInputs): Plan {
  const {viewport, budget, mTarget, maxTiles, visibleInView, velocity} = inputs;

  const visible = worldBbox(viewport, 1);
  const choice = chooseDepth({budget, mTarget, worldBbox: visible, maxTiles, visibleInView});

  const foreground: PlannedFetch = {
    kind: 'foreground',
    depth: choice.depth,
    rect: tileRectOfBbox(worldBbox(viewport, MARGIN), choice.depth)
  };

  /**
   * The visible box alone, fetched before the margin.
   *
   * **The screen fills in the time its own contents take, not the time the margin takes.** One
   * request for the margined box means the user waits for `MARGIN²` — 1.69× — of the marks they can
   * actually see, and on cold ground at a high budget that difference is seconds. Splitting costs
   * one extra round trip, against a per-request floor of ~170 µs; the margin is then a rectangle
   * subtraction away and is fetched second, off the critical path.
   */
  const visible_: PlannedFetch = {
    kind: 'visible',
    depth: choice.depth,
    rect: tileRectOfBbox(visible, choice.depth)
  };

  const background: PlannedFetch[] = [];

  // **The ring, biased downwind.** A pan continues in the direction it started far more often than
  // it reverses, so shifting the ring along recent movement buys the next second of panning at the
  // same tile cost as a centred one.
  //
  // **It is not free, and measurement says so.** Most of the ring is already held — 46,070 of
  // 46,410 tiles on the demo corpus — but the remainder is speculative work for tiles the user may
  // never look at. Measured (`probes/2026-08-08-lookahead-contention/`): the fraction of pans
  // needing no request at all roughly doubles, for roughly half again the server CPU per pan.
  //
  // The bias is the part whose value is unmeasured. A symmetric ring fetches ahead in every
  // direction at once and so costs the same whether the guess was right or not; shifting it is
  // what would make the cost depend on predicting correctly.
  // **The periphery is fetched coarser, not just further.** Each level shallower is a quarter of
  // the points per unit area, and §7.2's prefixes nest, so a coarse band is a legitimate superset
  // of the fine one it will be replaced by — it draws immediately when the user arrives and refines
  // when the foreground fetch lands. Doubling the reach while dropping a depth therefore costs
  // about the same as the band before it, instead of four times as much: a ring reaching 8x the
  // viewport costs three foregrounds rather than sixty-four.
  //
  // Without this a wide ring is unaffordable at any useful reach. Measured at a fixed depth, a 6x
  // ring asked for 2.3 x 10^6 points in one response.
  const reach = ringMargin(inputs.heldBytes ?? 0, inputs.budgetBytes ?? 0);
  const shift = velocity ? ringShift(viewport, velocity) : ([0, 0] as [number, number]);
  for (let step = 0; ; step++) {
    const depth = choice.depth - step;
    const margin = RING_MARGIN * 2 ** step;
    if (depth < MIN_DEPTH || margin > reach * 2) break;
    const rect = tileRectOfBbox(worldBbox(viewport, Math.min(margin, reach * 2), shift), depth);
    if (rectArea(rect) > maxTiles) break;
    background.push({kind: 'ring', depth, rect});
  }

  return {
    choice,
    visible: visible_,
    foreground,
    render: tileRectOfBbox(worldBbox(viewport, RENDER_MARGIN), choice.depth),
    background
  };
}

/**
 * The anticipatory next-depth fetch, for a client that has measured its cost and wants it.
 *
 * **Separate from {@link plan}, and off by default.** The ring is nearly free once bands are held —
 * most of it is already in the replica — but this is not: the client by construction never holds
 * depth `d+1`, so nothing elides and the request costs a genuine slice of a viewport's selection
 * scan on every idle pause. It is tile-count-neutral (four times the tiles over a quarter of the
 * area) and emphatically not CPU-neutral, and at N users it multiplies. Measure before enabling.
 *
 * What it buys is zoom-*in* feeling instant, which the ring cannot help with: a zoom lands on tiles
 * at a depth the replica has never seen, and nothing but having fetched them ahead of time avoids
 * the round trip.
 */
export function deeperFetch(inputs: PlannerInputs, choice: DepthChoice): PlannedFetch | null {
  if (choice.depth >= 16) return null;
  const depth = choice.depth + 1;
  // The centre quadrant: half the linear extent, so four times the tile density over a quarter of
  // the area is the same tile count the foreground already pays for.
  const rect = tileRectOfBbox(worldBbox(inputs.viewport, 0.5), depth);
  if (rectArea(rect) > inputs.maxTiles) return null;
  return {kind: 'deeper', depth, rect};
}

function ringShift(viewport: Viewport, velocity: [number, number]): [number, number] {
  const scale = 2 ** viewport.zoom;
  const worldWidth = viewport.width / scale;
  const worldHeight = viewport.height / scale;
  // Velocity is world units per ms; one viewport-width per second is `worldWidth / 1000`.
  const nx = (velocity[0] * 1000) / worldWidth;
  const ny = (velocity[1] * 1000) / worldHeight;
  const clamp = (v: number) => Math.max(-1, Math.min(1, v));
  return [clamp(nx) * VELOCITY_BIAS * worldWidth, clamp(ny) * VELOCITY_BIAS * worldHeight];
}
