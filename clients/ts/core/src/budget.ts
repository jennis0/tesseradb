import {MAX_DEPTH, WORLD_SIZE} from './coords.js';

/**
 * Choosing which depth to request, so that the number of marks on screen is roughly constant
 * regardless of zoom.
 *
 * §7.2 makes marks-per-TILE depth-stable at ≈ `m_target`; marks-on-SCREEN is therefore
 * `m_target × tiles-in-view`, and the only lever is which depth's tiles are asked for. At depth 0 a
 * viewport contains exactly one tile, which is why the MVP drew 17 marks at full extent and 456
 * four zoom levels in.
 *
 * **This selects the request; it never filters the response.** Every mark the server serves is
 * drawn. A client that dropped marks to fit a budget would be choosing what is visible, which is
 * I7's and not ours. `viewportLayer` asserts the drawn count equals the served count.
 *
 * Because priority prefixes nest (§7.2), requesting depth *d* under a shallow view returns a
 * superset of the natural tile's marks, so nothing pops — the same property §8.2 credits for making
 * `best-available` refinement look right.
 */

/** Requesting shallower than this is never worth it — see {@link MIN_DEPTH}'s doc. */
export const MIN_DEPTH = 3;

export type BudgetInputs = {
  budget: number;
  mTarget: number;
  worldBbox: [number, number, number, number];
  maxTiles: number;
  /**
   * The principal's visible count over the view, when known from the previous response. Without it
   * the prediction has no saturation term and overshoots wildly for sparse principals — measured at
   * 10⁹, a `narrow` principal (1,366 visible) draws all 1,366 from depth 4 onward while the
   * unsaturated model predicts millions.
   */
  visibleInView?: number;
};

export type DepthChoice = {
  depth: number;
  tiles: number;
  predictedMarks: number;
  limitedBy: 'budget' | 'maxTiles' | 'maxDepth' | 'saturated';
};

/** How many tiles of `depth` a world-space bbox intersects. */
export function tilesInBbox(bbox: [number, number, number, number], depth: number): number {
  const span = WORLD_SIZE / 2 ** depth;
  const [x0, y0, x1, y1] = bbox;
  const index = (v: number) => Math.min(2 ** depth - 1, Math.max(0, Math.floor(v / span)));
  return (index(x1) - index(x0) + 1) * (index(y1) - index(y0) + 1);
}

/**
 * The depth to request so the view draws roughly `budget` marks.
 *
 * Walks up from {@link MIN_DEPTH} and stops at the first depth that meets the budget, is capped by
 * `maxTiles`, saturates the principal's visible set, or hits the grid.
 *
 * **The floor of 3 is argued from marks, not from cost.** An earlier draft justified it as
 * "depths 0–2 cost 0.8–1.3 s at 10⁹", which is true only of the broadest principal — a `narrow`
 * principal pays 100–354 µs there. The durable reason is that shallow depths cannot deliver: at
 * depth 0 a view holds one tile and ~16 marks whatever the budget says, and a narrow principal gets
 * 14 of its 1,366 marks at depth 0 against 1,001 at depth 3.
 */
export function chooseDepth(inputs: BudgetInputs): DepthChoice {
  const {budget, mTarget, worldBbox, maxTiles, visibleInView} = inputs;
  const wantedTiles = Math.max(1, budget / Math.max(1, mTarget));

  let depth = MIN_DEPTH;
  let limitedBy: DepthChoice['limitedBy'] = 'maxDepth';

  for (let d = MIN_DEPTH; d <= MAX_DEPTH; d++) {
    const tiles = tilesInBbox(worldBbox, d);
    if (tiles > maxTiles) {
      limitedBy = 'maxTiles';
      break;
    }
    depth = d;
    if (tiles >= wantedTiles) {
      limitedBy = 'budget';
      break;
    }
    // Nothing deeper can help once every visible item in view is already drawn.
    if (visibleInView !== undefined && tiles * mTarget >= visibleInView) {
      limitedBy = 'saturated';
      break;
    }
  }

  const tiles = tilesInBbox(worldBbox, depth);
  const predicted = tiles * mTarget;
  return {
    depth,
    tiles,
    // The saturation term. Without it the prediction is off by three orders of magnitude for a
    // sparse principal, and the calibration below would chase it forever.
    predictedMarks: visibleInView === undefined ? predicted : Math.min(predicted, visibleInView),
    limitedBy
  };
}

export type Observation = {
  predictedMarks: number;
  actualMarks: number;
  /** The principal's visible count over the same view, from the same response. */
  visibleInView: number;
};

/** Bounds on the correction, so one pathological response cannot move `mTarget` far. */
const M_TARGET_MIN_FACTOR = 0.25;
const M_TARGET_MAX_FACTOR = 4;
const DAMPING = 0.5;

/**
 * Correct `mTarget` toward what the server actually served.
 *
 * The model `marks ≈ m_target · f · 4^d` holds to ~1% for broad principals at full extent and drifts
 * −32%/+14% across viewport fractions, because tile occupancy varies with clustering. The client
 * has the true figure in every response, so it need carry no model of clustering at all.
 *
 * **The correction is one-directional: it may only make the next request deeper, never shallower.**
 * That is not a stylistic choice. A shallower request returns a strict *subset* of what was just
 * drawn, so marks would pop *out* while the user did nothing — which is exactly the count-modulated
 * lever design §7.2 and §7.3 strike as unsound ("`k` ∝ count inverts the requirement and marks pop
 * out on zoom-in"). Damping would change the frequency of that, not its existence. Overshoot in the
 * other direction is harmless for correctness: more marks than budgeted is a payload question, and
 * `maxTiles` bounds it.
 *
 * **Saturation stops the loop.** When the server has served every visible item in view, a deeper
 * request cannot add marks, and an uncorrected loop would ratchet depth to the `maxTiles` cap
 * permanently for every sparse principal — 262,144 tiles to deliver 1,366 marks.
 */
export function calibrate(observation: Observation, mTarget: number, base: number): number {
  const {predictedMarks, actualMarks, visibleInView} = observation;
  if (actualMarks >= visibleInView) return mTarget; // saturated: deeper cannot help
  if (actualMarks <= 0 || predictedMarks <= 0) return mTarget;
  if (actualMarks >= predictedMarks) return mTarget; // one-directional: never go shallower

  const ratio = actualMarks / predictedMarks;
  const damped = mTarget * (1 + (ratio - 1) * DAMPING);
  return Math.min(base * M_TARGET_MAX_FACTOR, Math.max(base * M_TARGET_MIN_FACTOR, damped));
}
