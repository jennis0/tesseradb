import {WORLD_SIZE} from './coords.js';
import {chooseDepth, tilesOfBbox, type DepthChoice} from './budget.js';

/**
 * Layer 2: deciding *which* tiles to want.
 *
 * Split from the replica because the two answer different questions and a consumer may want only
 * one of them. The replica answers asks; this decides what to ask for, and a consumer with its own
 * tile scheduler — deck.gl's `TileLayer`, a MapLibre source — drops this layer entirely and keeps
 * the cache. Everything here is pure: no timers, no fetch, no clock. The scheduling that drives it
 * lives with the caller, which is what makes the policy testable at all.
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
};

/** One thing to ask for, in priority order. */
export type PlannedFetch = {
  kind: 'foreground' | 'ring' | 'deeper';
  depth: number;
  tiles: bigint[];
};

export type Plan = {
  choice: DepthChoice;
  /** What the user is looking at. Always first, always issued. */
  foreground: PlannedFetch;
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
/** How far the anticipatory ring reaches. Costs `RING_MARGIN²` in tiles over the visible box. */
export const RING_MARGIN = 2.2;
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
    tiles: tilesOfBbox(worldBbox(viewport, MARGIN), choice.depth)
  };

  const background: PlannedFetch[] = [];

  // **The ring, biased downwind.** A pan continues in the direction it started far more often than
  // it reverses, so shifting the ring along recent movement buys the next second of panning at the
  // same tile cost as a centred one. Cheap after the replica exists: most of the ring is already
  // held, so it is a near-empty request rather than a second viewport's worth of work.
  const shift = velocity ? ringShift(viewport, velocity) : ([0, 0] as [number, number]);
  const ring = tilesOfBbox(worldBbox(viewport, RING_MARGIN, shift), choice.depth);
  if (ring.length <= maxTiles) {
    background.push({kind: 'ring', depth: choice.depth, tiles: ring});
  }

  return {choice, foreground, background};
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
  const centre = worldBbox(inputs.viewport, 0.5);
  const tiles = tilesOfBbox(centre, depth);
  if (tiles.length > inputs.maxTiles) return null;
  return {kind: 'deeper', depth, tiles};
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
