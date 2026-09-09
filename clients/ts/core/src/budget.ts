import {MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {rectContains, type TileRect} from './rects.js';

/**
 * Choosing which depth to request, so that the number of marks on screen is roughly constant
 * regardless of zoom.
 *
 * **The exact figure is on the wire, and it is what decides the depth.** Every response's *tiles*
 * frame carries the per-tile masked count at the depth it was asked for (contracts §3.2 item 1), so
 * a client that has seen one response knows what the ground under the view holds. The marks a
 * candidate depth costs is `Σ min(k, count)` over the cells the request would address — the
 * server's own cap clause (§7.2), arithmetic rather than a model — and the depth to ask for is the
 * deepest one whose figure fits the budget. {@link countedMarks} is that sum; {@link chooseDepth}
 * walks the depths with it.
 *
 * **The average model it replaced, and why it failed.** §7.2 makes marks-per-TILE depth-stable at
 * ≈ `m_target`, so marks-on-SCREEN reads as `m_target × tiles-in-view` and the only lever is which
 * depth's tiles are asked for. That model assumes tile occupancy is roughly uniform. It is, for an
 * embedding; it is not for a gazetteer, where ocean cells are empty and land cells saturate at `k`,
 * and no single average describes both. Measured on GeoNames (13.5 × 10⁶ points, `k` = 500, budget
 * 500,000): a wide view the average model sent to depth 10 was answered with **2,076,760 points —
 * 100 MB**, 4× the budget, which is exactly {@link calibrate}'s clamp — the correction was already
 * pinned at its bound and had nothing left to give.
 *
 * The average survives as the fallback, for the one view no counts describe: the first request of a
 * session, and any view panned onto ground the replica has not covered. {@link calibrate} still
 * corrects it, from the average model's own figure rather than from the count-driven one — see
 * {@link DepthChoice.averageMarks}.
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

/** One cell's masked count, as the *tiles* frame reported it. */
export type CountCell = {x: number; y: number; count: number};

/**
 * Per-cell masked counts at one depth, and the rectangle they are complete for.
 *
 * **A cell of `covers` absent from `cells` holds nothing.** A response omits a cell whose masked
 * count is zero, which is what makes a sum over `cells` a sum over the region rather than a sum over
 * whatever happens to be held — and it is the field's one hazard, because ground the client has
 * never fetched is indistinguishable from empty ground and reading a hole as empty chooses a depth
 * too deep. Hence the obligation on whoever builds one: `cells` read over the whole of `covers`,
 * and every tile of `covers` fetched at `depth`. `Driver.adopt` is the built producer and settles
 * both when the field is taken, so a consumer need only check that the view lies inside `covers`.
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
  /**
   * The counts to predict from, when the replica holds any that cover the view. Absent, the average
   * model answers — see this module's doc for which is which.
   */
  counts?: CountField;
  /** The cap in force, `min(k, k_max_marks)`. The `k` of `Σ min(k, count)`; without it no count. */
  k?: number;
  /**
   * The principal's visible count over the view, when known from the previous response. The average
   * model's saturation term, and only its: without it that model overshoots wildly for sparse
   * principals — measured at 10⁹, a `narrow` principal (1,366 visible) draws all 1,366 from depth 4
   * onward while the unsaturated model predicts millions. A count-driven prediction needs no such
   * term, being bounded by the counts it sums.
   */
  visibleInView?: number;
  /** Answer for this exact depth (hysteresis at the caller) — `maxTiles` still binds. */
  force?: number;
};

export type DepthChoice = {
  depth: number;
  tiles: number;
  predictedMarks: number;
  /**
   * Where {@link predictedMarks} came from.
   *
   * - `counts` — `Σ min(k, count)` over the cells the request addresses, at the depth the counts
   *   are held at. **Exact** where §7.2's cap clause is the binding one, which is the dense case the
   *   budget exists for; an over-estimate where its threshold clause serves fewer than the cap.
   * - `bound` — the counts are held at another depth, so the figure is a bound: an upper bound
   *   below (a parent of `count` members yields at most `count` marks at any depth, and at most
   *   `k` per child, so at most `min(count, k · 4^Δ)`), a lower bound above (an ancestor tile
   *   reaches beyond the view and the counts describe only the part inside it).
   * - `average` — no counts cover the view; `m_target × tiles`, corrected by {@link calibrate}.
   */
  source: 'counts' | 'bound' | 'average';
  /**
   * What the average model predicted at the chosen depth, whether or not it decided anything.
   *
   * {@link calibrate} corrects `m_target` against this and never against {@link predictedMarks}: the
   * loop is a model of the average, and feeding it a count-driven figure would have it chase the
   * ratio between two predictions rather than the error in its own. The average stays the fallback
   * for the first view of a session and for ground the replica has not covered, so it has to keep
   * learning while the counts are deciding.
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

/**
 * The tile-index rectangle a world-space bbox covers.
 *
 * The rectangle rather than its tiles: the replica subtracts what it holds as rectangles, so no
 * caller needs the enumeration and producing one costs O(tiles) to describe a shape four integers
 * already carry.
 */
export function tileRectOfBbox(
  bbox: [number, number, number, number],
  depth: number
): {x0: number; y0: number; x1: number; y1: number} {
  return tileRange(bbox, depth);
}

/**
 * The marks a request at `depth` costs, from counts held at `field.depth`.
 *
 * `Σ min(k, count)`, evaluated over the cells of the view — the server's own cap clause, so at
 * `field.depth` this is the figure rather than a prediction of it. At another depth the counts have
 * to be moved to it, and the two directions are not symmetric:
 *
 * - **Deeper** (`Δ > 0`): a cell of `count` members cannot yield more than `count` marks however
 *   finely it is split, nor more than `k` in each of its `4^Δ` descendants, so `min(count, k · 4^Δ)`
 *   bounds it from above. Erring high is the safe direction — it declines a depth that might have
 *   fitted rather than accepting one that will not.
 * - **Shallower** (`Δ < 0`): the cells fold into their ancestor, and `min(k, Σ children)` is what
 *   that ancestor serves *of the part in view*. The ancestor also reaches outside the view, and
 *   those members are not in the field, so this is a lower bound on what the request pays for. The
 *   error is bounded at both ends: it is confined to the ancestors on the view's boundary, so it
 *   falls as `1/√tiles` of the total, and where the tile count is small enough for that fraction to
 *   be large the whole request is bounded by `tiles × k` and so is small in absolute terms.
 *
 * `capped` is whether any cell hit the cap — the exact saturation test. Nothing capped means every
 * member in view is already served, so a deeper request returns the same marks over four times the
 * tiles.
 *
 * Answers `null` when the field cannot speak for the view: the view must lie inside the region the
 * cells were read over, or a cell missing from `cells` would be read as empty ground.
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
  // Coarser than the counts: each cell's members land in one ancestor, and the ancestor serves
  // `min(k, ·)` of them. Keyed on the ancestor's tile index, which is under 2^16 on both axes.
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
 * The depth to request so the view draws roughly `budget` marks.
 *
 * Walks up from {@link MIN_DEPTH} and stops at the first depth that misses the budget, saturates
 * the ground under the view, is capped by `maxTiles`, or hits the grid.
 *
 * **With counts, the deepest depth that fits.** `Σ min(k, count)` is non-decreasing in depth —
 * splitting a cell can only distribute its members across more caps — so the first depth to miss the
 * budget is the last one to fit, and the walk needs no other stopping rule. It stops early where
 * nothing is capped: every member in view is served there, and a deeper request would pay four
 * times the tiles for the same marks. Without counts the average model's rule stands unchanged:
 * ask for about `budget / m_target` tiles, and stop when the principal's whole visible set is
 * drawn.
 *
 * **The floor of 3 is argued from marks, not from cost.** An earlier draft justified it as
 * "depths 0–2 cost 0.8–1.3 s at 10⁹", which is true only of the broadest principal — a `narrow`
 * principal pays 100–354 µs there. The durable reason is that shallow depths cannot deliver: at
 * depth 0 a view holds one tile and ~16 marks whatever the budget says, and a narrow principal gets
 * 14 of its 1,366 marks at depth 0 against 1,001 at depth 3. The floor is also why the first depth
 * is taken whatever it predicts: a view whose shallowest legal depth already misses the budget has
 * no shallower answer to fall back on.
 */
export function chooseDepth(inputs: BudgetInputs): DepthChoice {
  const {budget, mTarget, worldBbox, maxTiles, counts, k, visibleInView, force} = inputs;
  const wantedTiles = Math.max(1, budget / Math.max(1, mTarget));

  // Whether the counts can speak for this view is a property of the view and the field alone, so it
  // is settled once rather than per candidate depth — as is restricting them to the view, which is
  // the only pass over the whole field the walk below makes.
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

  // A forced depth still answers with its own tile count and prediction — the caller is deciding
  // hysteresis, not arithmetic, and `maxTiles` remains a hard bound whatever the caller holds.
  if (force !== undefined && tilesInBbox(worldBbox, force) <= maxTiles) return answer(force, 'budget');

  let depth = MIN_DEPTH;
  let limitedBy: DepthChoice['limitedBy'] = 'maxDepth';
  /** With counts: every depth that fits, and its marks — the walk is finished before it is chosen from. */
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

  // **A depth must buy marks, not only tiles.** The deepest depth that fits was the rule, and it
  // paid four times the tiles per step for whatever a few dense cells still had capped: the
  // owner's trace of 2026-08-28 shows depth 13 chosen with 199,977 tiles for 395k predicted
  // marks, and depth 12 with 20,000 tiles for 150k — six points a band, and a frame time that
  // follows the band count rather than the marks (150 ms at 25k bands against 50 ms at 12k for
  // twice the points). So the deepest fitting depth's marks are what is available, and the
  // **shallowest** depth within `MIN_GAIN_PER_STEP` of it is taken; the members a coarser depth
  // still holds capped wait for a zoom. Decided over the finished walk rather than step by step,
  // because a view that is one cell at coarse depths is flat at `k` and then jumps when the cell
  // splits — a per-step rule stopped on the flat part.
  // Only depths at or below the field's own: a folded cell's `min(k, count)` counts marks the
  // ancestor tile serves *outside* the view as well — a capped world tile is 500 marks, of which
  // a small view shows a handful — so a fold is not a figure for what is on screen, and a step
  // coarser than the field is never taken on its account.
  if (fitting.length > 1) {
    const best = fitting[fitting.length - 1]!.marks;
    const enough = fitting.find((f) => f.depth >= counts!.depth && f.marks >= best * (1 - MIN_GAIN_PER_STEP));
    if (enough && enough.depth < depth) return answer(enough.depth, 'saturated');
  }
  return answer(depth, limitedBy);
}

export type Observation = {
  /** The average model's figure at the chosen depth — {@link DepthChoice.averageMarks}. */
  predictedMarks: number;
  actualMarks: number;
  /** The principal's visible count over the same view, from the same response. */
  visibleInView: number;
};

/** Bounds on the correction, so one pathological response cannot move `mTarget` far. */
/**
 * How far below the deepest fitting depth's marks the chosen depth may sit — 15% — against the four
 * times the tiles every step deeper costs. Above it a step is buying bands, not points.
 */
export const MIN_GAIN_PER_STEP = 0.15;

const M_TARGET_MIN_FACTOR = 0.25;
const M_TARGET_MAX_FACTOR = 4;
const DAMPING = 0.5;

/**
 * Correct `mTarget` toward what the server actually served.
 *
 * The model `marks ≈ m_target × tiles` is what design §7.2's threshold delivers: θ is anchored on
 * the occupied-tile count inside the viewer's own mask, so the mean occupied tile draws `m_target`
 * marks at every depth whatever shape the data has. Reading `tiles` as `f · 4^d` is the part that
 * assumes an even spread: measured against the earlier `4^d`-progressed threshold it held to ~1%
 * for broad principals at full extent and drifted −32%/+14% across viewport fractions, and on a
 * bimodal field — a gazetteer's empty ocean and saturated land — not at all, with its correction
 * pinned at the 4× clamp while the response ran to 100 MB. That is what {@link countedMarks}
 * replaced it for; what remains here is the fallback for a view no counts describe, where the
 * occupied count is exactly what is not known.
 *
 * **It is corrected against its own figure, not against the count-driven one.** The observation
 * carries {@link DepthChoice.averageMarks}, so the ratio measures this model's error even on a
 * request whose depth the counts chose. Feeding it the count-driven prediction would have it chase
 * the difference between two predictions, and the fallback would drift on views it never decided.
 *
 * **The correction is bidirectional, but takes effect only across motion.** The one-directional
 * form ("never go shallower") existed because a shallower re-request of the *same* view is a
 * strict subset of what was just drawn — marks popping out while the user does nothing, the
 * count-modulated lever design §7.2 and §7.3 strike as unsound. Its premise — "overshoot is
 * harmless, a payload question" — was falsified at 10⁹: on dense ground `m(T)` scales with local
 * density, overshoot reached 4–8× the budget, and 3.8 × 10⁶ resident marks rasterised at 11 fps.
 * The pop-out objection is honoured by WHERE the correction lands rather than by refusing it: the
 * driver holds the presented depth while the view is still (no re-derivation at rest, so nothing
 * can pop), and a banked shallower choice applies when the user next moves — where the regime
 * changes under their own action, indistinguishable from any other zoom response.
 *
 * **Saturation stops the loop.** When the server has served every visible item in view, a deeper
 * request cannot add marks, and an uncorrected loop would ratchet depth to the `maxTiles` cap
 * permanently for every sparse principal — 262,144 tiles to deliver 1,366 marks.
 */
export function calibrate(observation: Observation, mTarget: number, base: number): number {
  const {predictedMarks, actualMarks, visibleInView} = observation;
  if (actualMarks >= visibleInView) return mTarget; // saturated: deeper cannot help
  if (actualMarks <= 0 || predictedMarks <= 0) return mTarget;

  const ratio = actualMarks / predictedMarks;
  const damped = mTarget * (1 + (ratio - 1) * DAMPING);
  return Math.min(base * M_TARGET_MAX_FACTOR, Math.max(base * M_TARGET_MIN_FACTOR, damped));
}
