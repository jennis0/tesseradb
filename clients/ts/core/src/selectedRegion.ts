import type {Composition} from './compose.js';
import {dataToWorldXY} from './coords.js';
import {NO_MASKED, type Count, type Masked} from './counts.js';
import type {Clock} from './driver.js';
import type {Refusal} from './presented.js';
import {insideBox, insidePolygon, type WorldPolygon} from './region.js';
import type {Quantisation, RegionVerdict} from './types.js';

/**
 * A selection for {@link Store.select}: a box or a lasso in the current view's data coordinates, or
 * a published artifact's shape. The store sends it on every viewport request as a `region` leaf
 * beside the other filters, so the region's counts are read off the same frame as the rest.
 *
 * - `box`: `bbox` is `[x0, y0, x1, y1]`, with either corner first.
 * - `lasso`: `points` are the drawn polygon's vertices, closed implicitly. A lasso needs at least
 *   three points; one with fewer clears the selection.
 * - `artifact`: `id` is the artifact's `tessera_id`, and the selection is its published shape.
 *
 * @category Projections
 */
export type SelectionShape = (
  | {kind: 'box'; bbox: [number, number, number, number]}
  | {kind: 'lasso'; points: [number, number][]}
  | {kind: 'artifact'; id: bigint}
) & {
  /** Whether to select everything outside the shape that this viewer can see. Defaults to `false`. */
  outside?: boolean;
};

/**
 * The selected region and what it holds, read off the presented frame. The store publishes `null`
 * in its place while nothing is selected.
 *
 * @category Projections
 */
export type RegionProjection = {
  /** The selection, as passed to {@link Store.select}. */
  shape: SelectionShape;
  /**
   * `loading` after a selection, a change of filters or `member_of` clauses, or a refresh, until a
   * frame fetched after it lands; then `shown`. `refused` where the request carrying the region was
   * refused.
   */
  status: 'loading' | 'shown' | 'refused';
  /** The refusal while `status` is `refused`, else `null`. */
  refusal: Refusal | null;
  /**
   * The items inside the shape that this viewer can see, from the store's counts in view (see
   * `ViewProjection.inView`). Before those land, the frame's count where no other filter narrows
   * it, else `null`. `null` while the region is not `shown`.
   */
  visible: Masked | null;
  /**
   * The items inside the shape that the other filters admit, from the store's counts in view.
   * Before those land, the sum of the frame's `matched` counts, exact where the server counted the
   * shape exactly and the frame's tiles cover the shape's extent. A complement (`outside`) is never
   * exact. Zero while the region is not `shown`.
   */
  matched: Masked;
  /**
   * The held marks inside the shape (`shown`) against `matched` (`total`). Not exact, with a
   * `total` of zero, while the region is not `shown`.
   */
  served: Count;
  /**
   * How the server counted the shape, from the `x-mosaica-region` header: exactly, or over a cover
   * of the shape at a stated depth. `null` until a response has carried it.
   */
  verdict: RegionVerdict | null;
  /**
   * The held marks inside the shape: the `tessera_id`s and world positions (`x, y` pairs) of the
   * first {@link REGION_HELD_LIMIT}, and the `count` of every one.
   */
  held: {ids: BigUint64Array; positions: Float32Array; count: number};
};

/**
 * How many held marks a region lists in `held.ids` and `held.positions`. `held.count` counts them
 * all.
 *
 * @category Projections
 */
export const REGION_HELD_LIMIT = 500;

/** The marks the store holds, which the region's sample is taken from. */
export type HeldMarks = {bands: Composition['exact']; standIn: Composition['standIn']};

type WorldShape = {kind: 'box'; box: [number, number, number, number]} | {kind: 'lasso'; polygon: WorldPolygon} | {kind: 'artifact'};

export type SelectedRegionDeps = {
  clock: Clock;
  /** The current view's frame, or null before meta. */
  frame: () => Quantisation | null;
  /** A data bbox for a served artifact, or null. */
  extentOf: (id: bigint) => [number, number, number, number] | null;
  /** Whether the replica holds every tile of a world box at a depth. */
  covered: (box: [number, number, number, number], depth: number) => boolean;
  publish: (region: RegionProjection | null) => void;
  trace: (kind: string, fields: Record<string, number | string>) => void;
};

/** The selection and the `region` projection read from it. */
export class SelectedRegion {
  private shape: SelectionShape | null = null;
  private verdict: RegionVerdict | null = null;
  private selectedAt = 0;
  private region: RegionProjection | null = null;
  /** The counts in view for the current question, once they land. */
  private counts: {visible: Masked | null; matched: Masked} | null = null;

  constructor(private readonly deps: SelectedRegionDeps) {}

  get selected(): SelectionShape | null {
    return this.shape;
  }

  /**
   * Select a shape, or clear the selection with `null` or a lasso of fewer than three points.
   * Returns whether the request changed, which is always the case for a shape it keeps.
   */
  select(shape: SelectionShape | null, marks: HeldMarks): boolean {
    const changed = shape !== this.shape;
    this.shape = shape;
    this.verdict = null;
    this.counts = null;
    this.selectedAt = this.deps.clock.now();
    if (!shape || (shape.kind === 'lasso' && shape.points.length < 3)) {
      this.shape = null;
      this.set(null);
      return changed;
    }
    this.loading(marks);
    return true;
  }

  /** The question changed under the selection: its sample stands and its numbers wait for a frame. */
  loading(marks: HeldMarks): void {
    const shape = this.shape;
    if (!shape) return;
    this.counts = null;
    const world = this.worldOf(shape);
    if (!world) {
      this.set(null);
      return;
    }
    const held = heldInside(world, shape.outside ?? false, marks);
    this.set({
      shape,
      status: 'loading',
      refusal: null,
      visible: null,
      matched: NO_MASKED,
      served: {shown: held.count, total: 0, exact: false},
      verdict: this.verdict,
      held
    });
  }

  /**
   * Read the region's numbers off a presented frame. A frame that was not fetched for this
   * selection answers the previous question, so a loading region waits for one that was.
   * `narrowed` says another filter is on, so the frame does not count the region alone.
   */
  answer(frame: {marks: HeldMarks; depth: number; verdict: RegionVerdict | null; fetched: boolean; matched: number; narrowed: boolean}): void {
    const shape = this.shape;
    const current = this.region;
    if (!shape || !current || current.shape !== shape) return;
    if (frame.verdict) this.verdict = frame.verdict;
    if (current.status === 'loading' && !frame.fetched) return;
    const world = this.worldOf(shape);
    if (!world) return;
    const outside = shape.outside ?? false;
    // The complement of a shape is never covered by one frame.
    const extent = outside ? null : this.worldExtentOf(shape);
    const covered = extent !== null && this.deps.covered(extent, frame.depth);
    const exact = (this.verdict?.exact ?? false) && covered;
    const held = heldInside(world, outside, frame.marks);
    const matched = this.counts?.matched ?? {value: frame.matched, exact};
    this.set({
      ...current,
      status: 'shown',
      refusal: null,
      matched,
      visible: this.counts ? this.counts.visible : frame.narrowed ? null : {value: frame.matched, exact},
      served: {shown: held.count, total: matched.value, exact: true},
      verdict: this.verdict,
      held
    });
    if (current.status === 'loading') this.deps.trace('region', {answerMs: this.deps.clock.now() - this.selectedAt, exact: exact ? 1 : 0, depth: this.verdict?.depth ?? -1});
  }

  /**
   * The store's counts in view landed for the current question. A region still waiting for its
   * frame keeps them for when it lands.
   */
  counted(counts: {visible: Masked | null; matched: Masked; verdict: RegionVerdict | null}): void {
    this.counts = {visible: counts.visible, matched: counts.matched};
    if (counts.verdict) this.verdict = counts.verdict;
    const region = this.region;
    if (!region || region.status !== 'shown') return;
    this.set({...region, matched: counts.matched, visible: counts.visible, served: {...region.served, total: counts.matched.value}, verdict: this.verdict});
  }

  /** The request carrying the region leaf was refused, so the region's numbers are that refusal. */
  refuse(refusal: Refusal): void {
    const region = this.region;
    if (!region || region.status === 'refused') return;
    this.set({...region, status: 'refused', refusal, visible: null, matched: NO_MASKED, served: {shown: region.held.count, total: 0, exact: false}});
  }

  /** Drop the selection. */
  drop(): void {
    this.shape = null;
    this.verdict = null;
    this.counts = null;
    this.set(null);
  }

  private set(region: RegionProjection | null): void {
    this.region = region;
    this.deps.publish(region);
  }

  private worldOf(shape: SelectionShape): WorldShape | null {
    const q = this.deps.frame();
    if (!q) return null;
    if (shape.kind === 'artifact') return {kind: 'artifact'};
    if (shape.kind === 'box') return {kind: 'box', box: worldBox(shape.bbox, q)};
    if (shape.points.length < 3) return null;
    return {kind: 'lasso', polygon: shape.points.map(([x, y]) => dataToWorldXY(x, y, q))};
  }

  /** The world box the count is over, or null for an artifact the store holds no extent for. */
  private worldExtentOf(shape: SelectionShape): [number, number, number, number] | null {
    const world = this.worldOf(shape);
    if (!world) return null;
    if (world.kind === 'box') return world.box;
    if (world.kind === 'lasso') {
      let x0 = Infinity;
      let y0 = Infinity;
      let x1 = -Infinity;
      let y1 = -Infinity;
      for (const [x, y] of world.polygon) {
        if (x < x0) x0 = x;
        if (y < y0) y0 = y;
        if (x > x1) x1 = x;
        if (y > y1) y1 = y;
      }
      return [x0, y0, x1, y1];
    }
    const extent = shape.kind === 'artifact' ? this.deps.extentOf(shape.id) : null;
    const q = this.deps.frame();
    if (!extent || !q) return null;
    return worldBox(extent, q);
  }
}

function worldBox(bbox: [number, number, number, number], q: Quantisation): [number, number, number, number] {
  const [x0, y0] = dataToWorldXY(bbox[0], bbox[1], q);
  const [x1, y1] = dataToWorldXY(bbox[2], bbox[3], q);
  return [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)];
}

/**
 * The held marks inside the shape, by the server's own predicate over the quantised grid. An
 * artifact selection has no client-side predicate: the request already narrowed the marks to its
 * members, so every held mark is inside.
 */
function heldInside(world: WorldShape, outside: boolean, marks: HeldMarks): RegionProjection['held'] {
  const ids: bigint[] = [];
  const xy: number[] = [];
  let count = 0;
  const inside =
    world.kind === 'box' ? (x: number, y: number) => insideBox(x, y, world.box) : world.kind === 'lasso' ? (x: number, y: number) => insidePolygon(x, y, world.polygon) : () => true;
  const take = (band: {ids: BigUint64Array; positions: Float32Array}, i: number) => {
    const x = band.positions[i * 2]!;
    const y = band.positions[i * 2 + 1]!;
    if (world.kind !== 'artifact' && inside(x, y) === outside) return;
    count++;
    if (ids.length < REGION_HELD_LIMIT) {
      ids.push(band.ids[i]!);
      xy.push(x, y);
    }
  };
  for (const band of marks.bands) {
    for (let i = 0; i < band.ids.length; i++) take(band, i);
  }
  for (const piece of marks.standIn) {
    if (piece.indices) for (const i of piece.indices.slice(0, piece.limit)) take(piece.band, i);
    else for (let i = 0; i < Math.min(piece.limit, piece.band.ids.length); i++) take(piece.band, i);
  }
  return {ids: BigUint64Array.from(ids), positions: Float32Array.from(xy), count};
}
