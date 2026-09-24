import {MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {rectContains, type TileRect} from './rects.js';

/**
 * Which depth to request, so the number of marks on screen stays near a budget at every zoom.
 *
 * Every response's tile list carries the per-tile masked count at the depth asked for, so after one
 * response the client knows what the ground under the view holds. A depth costs `Σ min(k, count)`
 * over the cells a request would address, which is the server's own cap, and the depth asked for is
 * the deepest whose figure fits the budget ({@link countedMarks}, {@link chooseDepth}).
 *
 * Where no counts cover the view (the first request of a session, or ground the replica has not
 * covered) an average model answers: `m_target` marks per tile. It assumes tiles are evenly
 * occupied, which fails on data such as a gazetteer, where ocean cells are empty and land cells
 * reach `k`. {@link calibrate} corrects it.
 *
 * This chooses the request and does not filter the response: every mark served is drawn. Served
 * prefixes nest across depth, so a deeper request returns a superset of a shallower one's marks and
 * nothing disappears.
 */

/** The shallowest depth requested; see {@link chooseDepth}. */
export const MIN_DEPTH = 3;

/** One cell's masked count, as the tile list reported it. */
export type CountCell = {x: number; y: number; count: number};

/**
 * Per-cell masked counts at one depth, and the rectangle they are complete for.
 *
 * A cell of `covers` absent from `cells` holds nothing, since a response omits cells whose masked
 * count is zero. Ground never fetched looks the same as empty ground, and reading it as empty
 * chooses a depth too deep. So whoever builds a field reads `cells` over the whole of `covers` and
 * has fetched every tile of `covers` at `depth`; `Driver.adopt` does both. A consumer checks only
 * that the view lies inside `covers`.
 */
export type CountField = {
  depth: number;
  cells: readonly CountCell[];
  covers: TileRect;
};

export type BudgetInputs = {
  budget: number;
  mTarget: number;
  worldBbox: [number, number, number, number];
  maxTiles: number;
  /** The counts to predict from, where any cover the view. Absent, the average model answers. */
  counts?: CountField;
  /** The cap in force, `min(k, k_max_marks)`: the `k` of `Σ min(k, count)`. Required to use counts. */
  k?: number;
  /**
   * The principal's visible count over the view, from the previous response. It bounds the average
   * model, which otherwise predicts millions of marks for a principal who can see a thousand. A
   * count-driven prediction is bounded by its counts and does not use it.
   */
  visibleInView?: number;
  /** Answer for this depth, for the caller's hysteresis. `maxTiles` still binds. */
  force?: number;
};

export type DepthChoice = {
  depth: number;
  tiles: number;
  predictedMarks: number;
  /**
   * Where {@link predictedMarks} came from.
   *
   * - `counts`: `Σ min(k, count)` at the depth the counts are held at. Exact where the cap is what
   *   limits a tile, which is the dense case the budget exists for; an overestimate where the
   *   server serves fewer than the cap.
   * - `bound`: the counts are held at another depth. An upper bound going deeper (a cell of `count`
   *   members yields at most `min(count, k · 4^Δ)` marks), a lower bound going shallower (an
   *   ancestor tile reaches beyond the view, and the counts describe only the part inside).
   * - `average`: no counts cover the view; `m_target × tiles`, corrected by {@link calibrate}.
   */
  source: 'counts' | 'bound' | 'average';
  /**
   * What the average model predicted at the chosen depth, whether or not it decided. {@link calibrate}
   * corrects against this, never {@link predictedMarks}: corrected against a count-driven figure, the
   * average would chase the ratio between two predictions, and it must stay accurate as the fallback.
   */
  averageMarks: number;
  limitedBy: 'budget' | 'maxTiles' | 'maxDepth' | 'saturated';
};

/** The half-open tile index range of `depth` a world-space bbox intersects. */
function tileRange(bbox: [number, number, number, number], depth: number) {
  const span = WORLD_SIZE / 2 ** depth;
  const [x0, y0, x1, y1] = bbox;
  const index = (v: number) => Math.min(2 ** depth - 1, Math.max(0, Math.floor(v / span)));
  return {x0: index(x0), y0: index(y0), x1: index(x1), y1: index(y1)};
}

/** How many tiles of `depth` a world-space bbox intersects. */
export function tilesInBbox(bbox: [number, number, number, number], depth: number): number {
  const r = tileRange(bbox, depth);
  return (r.x1 - r.x0 + 1) * (r.y1 - r.y0 + 1);
}

/** The tile-index rectangle a world-space bbox covers. */
export function tileRectOfBbox(
  bbox: [number, number, number, number],
  depth: number
): {x0: number; y0: number; x1: number; y1: number} {
  return tileRange(bbox, depth);
}

/**
 * The marks a request at `depth` costs, from counts held at `field.depth`: `Σ min(k, count)` over
 * the view's cells. At `field.depth` this is the figure itself. At another depth:
 *
 * - Deeper (`Δ > 0`): a cell of `count` members yields at most `count` marks, and at most `k` in
 *   each of its `4^Δ` descendants, so `min(count, k · 4^Δ)` is an upper bound. Erring high declines
 *   a depth that might have fitted rather than accepting one that will not.
 * - Shallower (`Δ < 0`): cells fold into their ancestor, which serves `min(k, Σ children)` of the
 *   part in view. The ancestor also reaches outside the view, so this is a lower bound. The error is
 *   confined to ancestors on the view's edge.
 *
 * `capped` is whether any cell reached the cap. Nothing capped means every member in view is served,
 * so a deeper request returns the same marks over four times the tiles.
 *
 * `null` where the view is not inside `field.covers`.
 */
export function countedMarks(
  field: CountField,
  bbox: [number, number, number, number],
  depth: number,
  k: number
): {marks: number; capped: boolean; exact: boolean} | null {
  const view = tileRange(bbox, field.depth);
  if (!rectContains(field.covers, view)) return null;
  return sumCells(cellsIn(field, view), depth - field.depth, k);
}

/** The field's cells inside a tile rectangle at its own depth. */
function cellsIn(field: CountField, view: TileRect): CountCell[] {
  const inside: CountCell[] = [];
  for (const cell of field.cells) {
    if (cell.x < view.x0 || cell.x > view.x1 || cell.y < view.y0 || cell.y > view.y1) continue;
    inside.push(cell);
  }
  return inside;
}

/** {@link countedMarks}' arithmetic, over cells already restricted to the view. */
function sumCells(cells: readonly CountCell[], delta: number, k: number) {
  if (delta >= 0) {
    const cap = k * 4 ** delta;
    let marks = 0;
    let capped = false;
    for (const cell of cells) {
      if (cell.count >= cap) capped = true;
      marks += Math.min(cap, cell.count);
    }
    return {marks, capped, exact: delta === 0};
  }
  // Coarser than the counts: each cell's members land in one ancestor, which serves `min(k, ·)` of
  // them. Keyed on the ancestor's tile index, under 2^16 on both axes.
  const shift = -delta;
  const ancestors = new Map<number, number>();
  for (const cell of cells) {
    const key = (cell.x >> shift) * 65_536 + (cell.y >> shift);
    ancestors.set(key, (ancestors.get(key) ?? 0) + cell.count);
  }
  let marks = 0;
  let capped = false;
  for (const count of ancestors.values()) {
    if (count >= k) capped = true;
    marks += Math.min(k, count);
  }
  return {marks, capped, exact: false};
}

/**
 * The depth to request so the view draws about `budget` marks.
 *
 * Walks up from {@link MIN_DEPTH} and stops at the first depth that misses the budget, saturates
 * the ground under the view, exceeds `maxTiles` or reaches the grid's depth. With counts,
 * `Σ min(k, count)` does not decrease with depth, so the first depth to miss the budget follows the
 * last to fit. Without counts the average model asks for about `budget / m_target` tiles and stops
 * once the principal's whole visible set is drawn.
 *
 * The floor of 3 is set by marks: at depth 0 a view holds one tile and about 16 marks whatever the
 * budget. The first depth is taken whatever it predicts, since there is no shallower answer.
 */
export function chooseDepth(inputs: BudgetInputs): DepthChoice {
  const {budget, mTarget, worldBbox, maxTiles, counts, k, visibleInView, force} = inputs;
  const wantedTiles = Math.max(1, budget / Math.max(1, mTarget));

  // Whether the counts cover the view depends on the view and the field alone, so it is settled once,
  // as is restricting them to the view.
  const view = counts ? tileRange(worldBbox, counts.depth) : null;
  const usable = counts !== undefined && k !== undefined && k > 0 && rectContains(counts.covers, view!);
  const inView = usable ? cellsIn(counts!, view!) : null;
  const predict = (depth: number) => sumCells(inView!, depth - counts!.depth, k!);

  const answer = (depth: number, limitedBy: DepthChoice['limitedBy']): DepthChoice => {
    const tiles = tilesInBbox(worldBbox, depth);
    const average = tiles * mTarget;
    // The average model's saturation term, applied to the average model's figure only.
    const averageMarks = visibleInView === undefined ? average : Math.min(average, visibleInView);
    if (!inView) return {depth, tiles, predictedMarks: averageMarks, source: 'average', averageMarks, limitedBy};
    const counted = predict(depth);
    return {
      depth,
      tiles,
      predictedMarks: counted.marks,
      source: counted.exact ? 'counts' : 'bound',
      averageMarks,
      limitedBy
    };
  };

  // A forced depth still answers with its own tile count and prediction; `maxTiles` still binds.
  if (force !== undefined && tilesInBbox(worldBbox, force) <= maxTiles) return answer(force, 'budget');

  let depth = MIN_DEPTH;
  let limitedBy: DepthChoice['limitedBy'] = 'maxDepth';
  /** With counts: every depth that fits, and its marks. The choice is made over the finished walk. */
  const fitting: {depth: number; marks: number}[] = [];

  for (let d = MIN_DEPTH; d <= MAX_DEPTH; d++) {
    const tiles = tilesInBbox(worldBbox, d);
    if (tiles > maxTiles) {
      limitedBy = 'maxTiles';
      break;
    }
    if (inView) {
      const counted = predict(d);
      if (d > MIN_DEPTH && counted.marks > budget) {
        limitedBy = 'budget';
        break;
      }
      depth = d;
      fitting.push({depth: d, marks: counted.marks});
      if (!counted.capped) {
        // Every member in view is served here. Deeper is four times the tiles for the same marks.
        limitedBy = 'saturated';
        break;
      }
      continue;
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

  // A deeper step costs four times the tiles, and past a point it buys bands rather than points: frame
  // time follows the band count. So the shallowest depth within `MIN_GAIN_PER_STEP` of the deepest
  // fitting depth's marks is taken. It is decided over the finished walk because a view that is one
  // cell at coarse depths stays flat at `k` and then jumps when the cell splits. Only depths at or
  // below the field's own qualify: a folded cell counts marks its ancestor serves outside the view.
  if (fitting.length > 1) {
    const best = fitting[fitting.length - 1]!.marks;
    const enough = fitting.find((f) => f.depth >= counts!.depth && f.marks >= best * (1 - MIN_GAIN_PER_STEP));
    if (enough && enough.depth < depth) return answer(enough.depth, 'saturated');
  }
  return answer(depth, limitedBy);
}

export type Observation = {
  /** The average model's figure at the chosen depth, {@link DepthChoice.averageMarks}. */
  predictedMarks: number;
  actualMarks: number;
  /** The principal's visible count over the same view, from the same response. */
  visibleInView: number;
};

/**
 * How far below the deepest fitting depth's marks the chosen depth may sit. Each step deeper costs
 * four times the tiles.
 */
export const MIN_GAIN_PER_STEP = 0.15;

/** Bounds on the correction, so one unusual response cannot move `mTarget` far. */
const M_TARGET_MIN_FACTOR = 0.25;
const M_TARGET_MAX_FACTOR = 4;
const DAMPING = 0.5;

/**
 * Corrects `mTarget` toward what the server served, for the average model.
 *
 * The server's threshold is anchored on the occupied-tile count inside the viewer's mask, so the
 * mean occupied tile draws `m_target` marks at every depth. Reading the tile count as uniform is
 * what fails on uneven data, and that is where counts take over. The correction uses the average
 * model's own figure ({@link DepthChoice.averageMarks}), so it measures that model's error even on a
 * request the counts decided.
 *
 * The correction works in both directions, but a shallower depth on a still view would remove marks
 * with no user action. The driver holds the presented depth while the view is still, and a
 * corrected depth applies on the next move.
 *
 * Saturation stops the loop: once every visible item in view is served, a deeper request adds no
 * marks, and an uncorrected loop would push a sparse principal's depth to the `maxTiles` cap.
 */
export function calibrate(observation: Observation, mTarget: number, base: number): number {
  const {predictedMarks, actualMarks, visibleInView} = observation;
  if (actualMarks >= visibleInView) return mTarget; // saturated: deeper cannot help
  if (actualMarks <= 0 || predictedMarks <= 0) return mTarget;

  const ratio = actualMarks / predictedMarks;
  const damped = mTarget * (1 + (ratio - 1) * DAMPING);
  return Math.min(base * M_TARGET_MAX_FACTOR, Math.max(base * M_TARGET_MIN_FACTOR, damped));
}
