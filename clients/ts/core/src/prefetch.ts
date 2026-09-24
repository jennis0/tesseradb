import {MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {MIN_DEPTH, chooseDepth, tileRectOfBbox, type CountField, type DepthChoice} from './budget.js';
import {rectArea, type TileRect} from './rects.js';

/**
 * Which tiles to want. Pure: no timers, no fetch, no clock; the caller schedules. A consumer with its
 * own tile scheduler (deck.gl's `TileLayer`, a MapLibre source) can drop this and keep the replica.
 *
 * Plans are rectangles, never tile lists: enumerating a quarter-million-tile ring costs a noticeable
 * fraction of a second to describe what four integers carry.
 *
 * Asking for tiles deeper than the view needs is sound because served prefixes nest: a deeper
 * answer is a superset of the shallower one, so nothing disappears when the user arrives.
 *
 * Anticipation has three separate quantities. Where to fetch ahead is decided here, from geometry,
 * velocity and depth. How fast is the caller's pacing, which changes the rate and never the region.
 * How much to keep is the replica's byte budget. One coupling is kept: {@link ringMargin} widens the
 * ring while the replica has room, because an empty cache wants its first ring close in.
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
  /** Calibrated marks per tile, for the average model used where no counts cover the view. */
  mTarget: number;
  /** Per-cell masked counts known to cover the view; see `budget.ts`. */
  counts?: CountField;
  /** The cap in force, `min(k, k_max_marks)`: the `k` of `Σ min(k, count)`. */
  k?: number;
  maxTiles: number;
  /** The previous response's visible count, the saturation term for depth choice. */
  visibleInView?: number;
  /** World units per millisecond, signed, from recent movement. Biases the ring downwind. */
  velocity?: [number, number];
  /** The depth currently drawn; a one-step budget disagreement defers to it. See {@link plan}. */
  holdDepth?: number;
  /** What the replica holds and what it may hold, which size the ring. See {@link ringMargin}. */
  heldBytes?: number;
  budgetBytes?: number;
  /**
   * Depths below the current one to fetch over the visible box and hold undrawn: default 1, 0 for a
   * constrained client, 2 or more where GPU and network allow.
   */
  depthLayers?: number;
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
 * How far beyond the visible box the foreground request reaches. Costs `MARGIN²` in tiles, and most
 * drags move the view by well under 30% of its width.
 */
export const MARGIN = 1.3;

/**
 * How far beyond the visible box the drawn buffer reaches.
 *
 * Wider than {@link MARGIN}: fetching is bounded by the budget, drawing only by what is held.
 * Leaving the buffer costs a rebuild and an upload of every mark; staying inside costs nothing,
 * since deck.gl re-projects what is there. An aggressive pan moves a third to a half of a viewport,
 * and at 2.6 the buffer reaches 0.8 viewport widths beyond the screen. The cost is `RENDER_MARGIN²`
 * more marks per upload.
 */
export const RENDER_MARGIN = 2.6;
/** How far the anticipatory ring reaches when the replica is nearly full; see {@link ringMargin}. */
export const RING_MARGIN = 2.2;

/** How far it reaches when the replica is empty. */
export const RING_MARGIN_MAX = 6;

/**
 * The fraction of its budget the replica fills before the ring stops growing. Below it a wider ring
 * costs only the fetch; above it a wider ring would evict what it bought.
 */
export const RING_FILL_TARGET = 0.6;

/**
 * How far the ring reaches, from `RING_MARGIN_MAX` when the replica is empty down to `RING_MARGIN`
 * as it fills. A fixed small ring leaves most of the cache budget unused; a wide one is affordable
 * because the reach is spent on coarser depths (see {@link plan}).
 *
 * The reach costs server work: on the demo corpus the wide ring held three times the points and
 * answered more pans without a request, for six times the server CPU and four times the bytes.
 * With many active principals `RING_MARGIN_MAX` is the setting to lower.
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
 * What to ask for, given a viewport. The depth is chosen for the visible box and the margin fetched
 * at that depth; chosen for the margined box, it would spend the budget off screen and lower the
 * resolution of what is in view.
 */
export function plan(inputs: PlannerInputs): Plan {
  const {viewport, budget, mTarget, maxTiles, counts, k, visibleInView, velocity, holdDepth} = inputs;

  const visible = worldBbox(viewport, 1);
  const ask = {budget, mTarget, worldBbox: visible, maxTiles, counts, k, visibleInView};
  let choice = chooseDepth(ask);
  // A one-step disagreement defers to the depth drawn. The visible count changes with the ground
  // under the view, so panning across a density boundary would flip the choice between neighbouring
  // depths. A real zoom moves the choice by more than one step.
  if (holdDepth !== undefined && Math.abs(choice.depth - holdDepth) === 1) {
    choice = chooseDepth({...ask, force: holdDepth});
  }

  const foreground: PlannedFetch = {
    kind: 'foreground',
    depth: choice.depth,
    rect: tileRectOfBbox(worldBbox(viewport, MARGIN), choice.depth)
  };

  /**
   * The visible box alone, fetched before the margin, so the screen fills in the time its own
   * contents take. The margin is fetched second, less what the first request covered.
   */
  const visible_: PlannedFetch = {
    kind: 'visible',
    depth: choice.depth,
    rect: tileRectOfBbox(visible, choice.depth)
  };

  const background: PlannedFetch[] = [];

  // The ring is shifted along recent movement, since a pan more often continues than reverses. Most
  // of a ring is already held; the rest is speculative server work.
  //
  // The periphery is fetched coarser as well as further. Each level shallower is a quarter of the
  // points per unit area, and a coarse band is a superset of the fine one that later replaces it,
  // so doubling the reach while dropping a depth costs about the same as the band before it.
  const reach = ringMargin(inputs.heldBytes ?? 0, inputs.budgetBytes ?? 0);
  const shift = velocity ? ringShift(viewport, velocity) : ([0, 0] as [number, number]);
  // The depths below the current one over the visible box come first: a zoom-in lands under the
  // cursor, and the pan ring does not help with it. Each costs up to about four times the view's
  // bytes over new ground and nothing over held ground.
  for (let layer = 1; layer <= (inputs.depthLayers ?? 1); layer++) {
    const depth = choice.depth + layer;
    if (depth > MAX_DEPTH) break;
    const rect = tileRectOfBbox(visible, depth);
    if (rectArea(rect) > maxTiles) break;
    background.push({kind: 'deeper', depth, rect});
  }
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
 * A fetch of the next depth over the centre quadrant of the view: four times the tile density over
 * a quarter of the area, so the same tile count as the foreground. Little of depth `d+1` is usually
 * held, so each idle pause that asks pays for a server scan. {@link plan} does not call this.
 */
export function deeperFetch(inputs: PlannerInputs, choice: DepthChoice): PlannedFetch | null {
  if (choice.depth >= 16) return null;
  const depth = choice.depth + 1;
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
