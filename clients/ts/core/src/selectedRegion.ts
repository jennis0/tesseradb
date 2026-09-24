import type {Composition} from './compose.js';
import {dataToWorldXY} from './coords.js';
import {NO_MASKED, type Count, type Masked} from './counts.js';
import type {Clock} from './driver.js';
import type {Refusal} from './presented.js';
import {insideBox, insidePolygon, type WorldPolygon} from './region.js';
import type {Quantisation, RegionVerdict} from './types.js';

/**
 * A selection, in data coordinates, or a published shape named by its `tessera_id`. A selection
 * is a filter: it goes on every viewport request as a `region` leaf beside the other filters, so
 * the region's counts are read off the same frame as everything else. `outside` negates it,
 * selecting the complement within what this principal can see.
 */
export type SelectionShape = (
  | {kind: 'box'; bbox: [number, number, number, number]}
  | {kind: 'lasso'; points: [number, number][]}
  | {kind: 'artifact'; id: bigint}
) & {outside?: boolean};

/**
 * The selected region and what it holds, read off the presented frame.
 *
 * `matched` is the sum of the frame's `matched` counts: the items inside the shape that the other
 * filters admit. It is exact when the server's verdict says so and the frame's tiles cover the
 * shape, and a cover otherwise. `visible` is the region alone, the same figure while no other filter
 * is on and `null` while one is. `served` is the held marks inside against `matched`. `status` is
 * `loading` until a frame fetched after the selection lands; a refused request is the region's
 * refusal.
 */
export type RegionProjection = {
  shape: SelectionShape;
  status: 'loading' | 'shown' | 'refused';
  refusal: Refusal | null;
  visible: Masked | null;
  matched: Masked;
  served: Count;
  /** `x-tessera-region`: exact for the shape, or a cover at a depth; `null` until it has answered. */
  verdict: RegionVerdict | null;
  /** The held marks inside the shape: ids and world positions, the first {@link REGION_HELD_LIMIT}. */
  held: {ids: BigUint64Array; positions: Float32Array; count: number};
};

/** How many held marks a region lists. The count is always whole. */
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
    this.set({
      ...current,
      status: 'shown',
      refusal: null,
      matched: {value: frame.matched, exact},
      visible: frame.narrowed ? null : {value: frame.matched, exact},
      served: {shown: held.count, total: frame.matched, exact: true},
      verdict: this.verdict,
      held
    });
    if (current.status === 'loading') this.deps.trace('region', {answerMs: this.deps.clock.now() - this.selectedAt, exact: exact ? 1 : 0, depth: this.verdict?.depth ?? -1});
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
