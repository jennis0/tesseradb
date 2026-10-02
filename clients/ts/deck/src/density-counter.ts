import {WORLD_SIZE, type AggregateResult, type AggregateSpec, type Refusal, type Store} from '@tesseradb/client';
import {worldBbox} from '@tesseradb/client/internal';
import {densityCellsOf, type DensityCounts} from './density.js';

/**
 * The cell sizes on screen density's resolution is chosen from, in CSS pixels, coarse to fine.
 *
 * @category Colour
 */
export const DENSITY_CELL_SIZES: readonly number[] = [32, 24, 16, 12, 8, 6, 4];

/**
 * The cell size density is drawn at until one is chosen, in CSS pixels.
 *
 * @category Colour
 */
export const DEFAULT_DENSITY_CELL_PX = 12;

/**
 * How long the camera rests before {@link DensityCounter} asks for counts, in milliseconds.
 *
 * @category Colour
 */
export const DENSITY_SETTLE_MS = 200;

/**
 * The asked area's half-width and half-height over the viewport's. At 1.5 the area reaches a
 * quarter of the viewport's width and height past each side, so a pan of up to that much draws
 * from the counts held.
 */
const MARGIN = 1.5;

/** The deepest depth asked for: a cell's column and row at this depth fit a 31-bit integer. */
const DEEPEST = 30;

/**
 * A camera over the 512-unit world, as deck's `OrthographicView` view state holds it, with the
 * canvas size.
 *
 * @category Colour
 */
export type DensityCamera = {
  /** The world point at the centre of the canvas, `[x, y]` in world units. A missing coordinate reads as 0. */
  target: readonly number[];
  /** deck's zoom: the canvas shows `2 ** zoom` pixels per world unit. */
  zoom: number;
  /** The canvas width in CSS pixels. */
  width: number;
  /** The canvas height in CSS pixels. */
  height: number;
};

/**
 * One resolution stop: its cell size on screen, the depth whose cells come closest to it at the
 * camera's zoom, and whether that depth's cells over the asked area fit
 * `selection.maxAggregateCells`.
 *
 * @category Colour
 */
export type ResolutionStop = {
  /** The cell size on screen, in CSS pixels. */
  px: number;
  /** The depth whose cells are nearest that size at the camera's zoom. */
  depth: number;
  /** Whether that depth's cells over the area asked for number at most `selection.maxAggregateCells`. */
  enabled: boolean;
};

/**
 * What {@link DensityCounter} asks for.
 *
 * @category Colour
 */
export type DensitySettings = {
  /** Whether density is drawn. Off, nothing is asked for and nothing is held. */
  on: boolean;
  /** The cell size on screen, in CSS pixels, taken as the nearest of {@link DENSITY_CELL_SIZES}. */
  cellPx: number;
  /** Whether the counts are over the items that also satisfy the store's highlight. */
  highlighted: boolean;
};

type Box = [number, number, number, number];

/**
 * The depth whose cells are closest to `cellPx` across on screen at `zoom`, nearest on a log scale.
 * A cell at depth `d` is `WORLD_SIZE · 2^zoom / 2^d` pixels wide.
 *
 * @category Colour
 */
export function cellDepth(zoom: number, cellPx: number): number {
  const depth = Math.round(Math.log2(WORLD_SIZE) + zoom - Math.log2(cellPx));
  return Math.min(DEEPEST, Math.max(0, depth));
}

/** How many cells at `depth` a world-space box intersects, as the server counts them for an `area`. */
export function cellsIn(area: Box, depth: number): number {
  const side = 2 ** depth;
  const span = WORLD_SIZE / side;
  const index = (v: number) => Math.min(side - 1, Math.max(0, Math.floor(v / span)));
  return (index(area[2]) - index(area[0]) + 1) * (index(area[3]) - index(area[1]) + 1);
}

/** The deepest depth whose cells over `area` number at most `limit`; -1 where none does. */
function deepestFitting(area: Box, limit: number): number {
  if (cellsIn(area, 0) > limit) return -1;
  let depth = 0;
  while (depth < DEEPEST && cellsIn(area, depth + 1) <= limit) depth += 1;
  return depth;
}

/** The area asked for at `camera`: the viewport with its margin, clamped to the world. */
function askedArea(camera: DensityCamera): Box {
  return worldBbox({target: [camera.target[0] ?? 0, camera.target[1] ?? 0], zoom: camera.zoom, width: camera.width, height: camera.height}, MARGIN);
}

/**
 * The stops of {@link DENSITY_CELL_SIZES} at `camera`, each with the depth it asks for and whether
 * that depth fits `limit` cells over the area asked for.
 *
 * @category Colour
 */
export function resolutionStops(camera: DensityCamera, limit: number): ResolutionStop[] {
  const deepest = deepestFitting(askedArea(camera), limit);
  return DENSITY_CELL_SIZES.map((px) => {
    const depth = cellDepth(camera.zoom, px);
    return {px, depth, enabled: depth <= deepest};
  });
}

/**
 * The depth asked for at `camera` for `cellPx`: the depth of the stop nearest `cellPx`, or of the
 * finest stop enabled where that one is not, or the deepest that fits where no stop does. `null`
 * where no depth fits.
 */
export function densityDepth(camera: DensityCamera, cellPx: number, limit: number): number | null {
  const area = askedArea(camera);
  const deepest = deepestFitting(area, limit);
  if (deepest < 0) return null;
  return Math.min(cellDepth(camera.zoom, nearestStop(cellPx)), deepest);
}

/** The stop of {@link DENSITY_CELL_SIZES} nearest `px` on a log scale. */
export function nearestStop(px: number): number {
  let best = DENSITY_CELL_SIZES[0]!;
  for (const stop of DENSITY_CELL_SIZES) if (Math.abs(Math.log(stop / px)) < Math.abs(Math.log(best / px))) best = stop;
  return best;
}

const contains = (outer: Box, inner: Box) => outer[0] <= inner[0] && outer[1] <= inner[1] && outer[2] >= inner[2] && outer[3] >= inner[3];

/** Where a cell-limit refusal names the deepest depth that fits, that depth. */
function deepestNamed(refusal: Refusal): number | null {
  const named = /depth (\d+) or less/.exec(refusal.detail);
  return named ? Number(named[1]) : null;
}

let counters = 0;

/**
 * Keeps the counts density is drawn from for a camera: one `POST /v1/aggregate` grouping of cells,
 * registered with the store, which sends its filters with it and asks again when they change.
 *
 * A camera change asks nothing while the camera moves. {@link DENSITY_SETTLE_MS} after the last
 * one, the counter works out the depth whose cells are nearest the chosen size on screen and asks
 * for that depth over the viewport and a margin, unless the counts asked for last are at that depth
 * and their area holds the viewport. The counts held are drawn at their own world positions until
 * the next answer lands, so a zoom scales them with the map and a pan within the margin asks
 * nothing. A depth past `selection.maxAggregateCells` over the area is never asked for; where the
 * server refuses a depth anyway and names the deepest that fits, the counter asks for that one.
 *
 * `onChange` is called when the counts to draw change.
 *
 * @category Colour
 */
export class DensityCounter {
  /** The registrations made, one per request the counter started. */
  requests = 0;
  private readonly id: string;
  private settings: DensitySettings = {on: false, cellPx: DEFAULT_DENSITY_CELL_PX, highlighted: false};
  private camera: DensityCamera | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private asked: {view: string; depth: number; area: Box; highlighted: boolean} | null = null;
  private held: {view: string; counts: DensityCounts} | null = null;
  private seen: AggregateResult | Refusal | null = null;
  private readonly unsubscribe: () => void;

  constructor(
    private readonly store: Store,
    private readonly onChange: () => void,
    private readonly settleMs = DENSITY_SETTLE_MS
  ) {
    counters += 1;
    this.id = `density#${counters}`;
    this.unsubscribe = store.subscribe(() => this.read());
  }

  /** The camera moved or the canvas was resized. Counts are asked for once it has rested. */
  look(camera: DensityCamera): void {
    this.camera = camera;
    if (this.settings.on) this.schedule();
  }

  /** What to draw. Off drops the registration and the counts held at once. */
  set(settings: DensitySettings): void {
    const was = this.settings;
    this.settings = settings;
    if (!settings.on) {
      this.stop();
      return;
    }
    if (!was.on || was.cellPx !== settings.cellPx || was.highlighted !== settings.highlighted) this.schedule();
  }

  /** The counts to draw: the last answer, while it was counted in the store's current view. */
  counts(): DensityCounts | null {
    const held = this.held;
    return held && held.view === this.store.get('view').id ? held.counts : null;
  }

  /** The resolution stops at the camera as it stands; every stop is enabled before a camera and `meta`. */
  stops(): ResolutionStop[] {
    const meta = this.store.get('meta');
    if (!this.camera || !meta) return DENSITY_CELL_SIZES.map((px) => ({px, depth: 0, enabled: true}));
    return resolutionStops(this.camera, meta.selection.maxAggregateCells);
  }

  /** Drop the registration and stop following the store. */
  dispose(): void {
    this.stop();
    this.unsubscribe();
  }

  private schedule(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.ask();
    }, this.settleMs);
  }

  private ask(): void {
    const meta = this.store.get('meta');
    const camera = this.camera;
    if (!this.settings.on || !meta || !camera || !this.store.frame()) return;
    const depth = densityDepth(camera, this.settings.cellPx, meta.selection.maxAggregateCells);
    if (depth === null) return;
    const view = this.store.get('view').id;
    const visible = worldBbox({target: [camera.target[0] ?? 0, camera.target[1] ?? 0], zoom: camera.zoom, width: camera.width, height: camera.height}, 1);
    const a = this.asked;
    if (a && a.view === view && a.depth === depth && a.highlighted === this.settings.highlighted && contains(a.area, visible)) return;
    this.register(view, depth, askedArea(camera));
  }

  private register(view: string, depth: number, area: Box): void {
    const [x0, y0] = this.store.dataXY(area[0], area[1]);
    const [x1, y1] = this.store.dataXY(area[2], area[3]);
    const highlighted = this.settings.highlighted;
    const spec: AggregateSpec = {
      groupings: [{cells: {depth, area: [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)]}}],
      ...(highlighted ? {highlighted} : {})
    };
    this.asked = {view, depth, area, highlighted};
    this.requests += 1;
    this.store.setAggregate(this.id, spec);
  }

  /** Take an answer that landed, or act on a refusal. */
  private read(): void {
    if (this.store.get('meta') === null) {
      // The store has forgotten what the server answered.
      this.asked = null;
      if (this.held) {
        this.held = null;
        this.onChange();
      }
      return;
    }
    const asked = this.asked;
    if (!asked) {
      // `meta` has arrived after the camera: ask now that it can be.
      if (this.settings.on && this.camera && this.timer === null) this.schedule();
      return;
    }
    const entry = this.store.get('aggregates').get(this.id);
    if (!entry) return;
    if (entry.status === 'shown' && entry.result && entry.result !== this.seen) {
      this.seen = entry.result;
      const table = entry.result.tables[0];
      this.held = {view: entry.view ?? asked.view, counts: {depth: asked.depth, cells: table ? densityCellsOf(table, asked.depth) : []}};
      this.onChange();
    } else if (entry.status === 'refused' && entry.refusal && entry.refusal !== this.seen) {
      this.seen = entry.refusal;
      const deepest = deepestNamed(entry.refusal);
      if (deepest !== null && deepest < asked.depth) this.register(asked.view, deepest, asked.area);
      else if (this.held) {
        this.held = null;
        this.onChange();
      }
    }
  }

  private stop(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    if (this.asked) this.store.setAggregate(this.id, null);
    this.asked = null;
    this.seen = null;
    if (this.held) {
      this.held = null;
      this.onChange();
    }
  }
}
