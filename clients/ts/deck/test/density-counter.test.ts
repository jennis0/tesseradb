import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {tableFromArrays} from 'apache-arrow';
import {TesseraError, createStore, type AggregateRequest, type AggregateResult, type TesseraClient} from '@tesseradb/client';
import {mortonOfTile} from '@tesseradb/client/internal';
import {SELECTION, fakeClock, fakeScheduler, meta, response, result as viewportResult, view} from '../../core/test/support.js';
import {DENSITY_CELL_SIZES, DENSITY_SETTLE_MS, DensityCounter, cellDepth, resolutionStops, type DensityCamera} from '../src/density-counter.js';

/**
 * The counter against a real store over a fake client: which cells it asks for at a camera, when
 * it asks, and what it hands the layer to draw. The view's extent is the world's, so an asked area
 * reads in world units.
 */

const WIDTH = 1000;
const HEIGHT = 800;
const camera = (target: [number, number], zoom: number): DensityCamera => ({target, zoom, width: WIDTH, height: HEIGHT});

/** A store whose `aggregate` answers each request with one cell at the asked depth, unless told to refuse. */
async function setUp(maxAggregateCells = SELECTION.maxAggregateCells) {
  const asked: AggregateRequest[] = [];
  const refusals: Error[] = [];
  const aggregate = vi.fn(async (_token: string, req: AggregateRequest): Promise<AggregateResult> => {
    asked.push(req);
    const refusal = refusals.shift();
    if (refusal) throw refusal;
    const depth = req.groupings[0]!.cells!.depth;
    return {
      tables: [{grouping: 0, total: 9, referenceTotal: null, groups: null, rows: tableFromArrays({cell: BigUint64Array.from([mortonOfTile(1, 1, depth)]), count: BigUint64Array.from([9n])})}],
      region: null,
      recomposed: false,
      identityKey: 'ik',
      next: null
    };
  });
  const client = {
    meta: async () => meta({views: [view('s0', {quantisation: {xMin: 0, xMax: 512, yMin: 0, yMax: 512}})], selection: {...SELECTION, maxAggregateCells}}),
    viewport: async () => response(viewportResult()),
    aggregate,
    close: () => {}
  } as unknown as TesseraClient;
  const clock = fakeClock();
  const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler: fakeScheduler(), prefetch: false});
  await clock.advance(1);
  await vi.advanceTimersByTimeAsync(0);
  const changes = {n: 0};
  const counter = new DensityCounter(store, () => (changes.n += 1));
  /** Let the settle timer run and the answer land. */
  const settle = () => vi.advanceTimersByTimeAsync(DENSITY_SETTLE_MS);
  const depths = () => asked.map((r) => r.groupings[0]!.cells!.depth);
  return {store, counter, asked, refusals, settle, depths, changes};
}

describe('the depth a stop asks for', () => {
  it('is the depth whose cells come nearest the stop’s size on screen at the zoom', () => {
    // At zoom 0 the world is 512 px wide: depth 5 cells are 16 px, depth 6 cells 8 px.
    expect(cellDepth(0, 16)).toBe(5);
    expect(cellDepth(0, 8)).toBe(6);
    // 12 px is nearer 16 than 8 on a log scale.
    expect(cellDepth(0, 12)).toBe(5);
    // Each zoom level doubles the cells' size, so the depth follows the zoom one for one.
    expect(cellDepth(3, 16)).toBe(8);
    expect(cellDepth(3.4, 16)).toBe(8);
    expect(cellDepth(3.6, 16)).toBe(9);
  });

  it('disables the stops whose cells over the asked area pass the server’s limit', () => {
    const at = camera([256, 256], 2);
    const all = resolutionStops(at, SELECTION.maxAggregateCells);
    expect(all.every((s) => s.enabled)).toBe(true);
    expect(all.map((s) => s.px)).toEqual([...DENSITY_CELL_SIZES]);
    // 1000 × 800 px with the margin is 1500 × 1200 px asked: at 32 px about 47 × 38 cells, at
    // 4 px about 375 × 300. A limit of 20,000 cells admits the coarse stops and not the fine.
    const limited = resolutionStops(at, 20_000);
    const enabled = limited.filter((s) => s.enabled).map((s) => s.px);
    expect(enabled.length).toBeGreaterThan(0);
    expect(enabled.length).toBeLessThan(DENSITY_CELL_SIZES.length);
    // The enabled stops are the coarse end: once one is disabled every finer one is.
    expect(limited.map((s) => s.enabled)).toEqual(DENSITY_CELL_SIZES.map((_, i) => i < enabled.length));
  });
});

describe('DensityCounter', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('asks nothing while the camera moves, and once when it has rested', async () => {
    const {counter, asked, settle} = await setUp();
    counter.set({on: true, cellPx: 12, highlighted: false});
    // A two-second zoom gesture: a camera change each frame.
    for (let frame = 0; frame < 120; frame++) {
      counter.look(camera([256, 256], 1 + frame / 40));
      await vi.advanceTimersByTimeAsync(16);
    }
    expect(asked).toHaveLength(0);
    await settle();
    expect(asked).toHaveLength(1);
    expect(asked[0]!.groupings).toEqual([{cells: {depth: cellDepth(1 + 119 / 40, 12), area: expect.any(Array) as unknown}}]);
    await settle();
    expect(asked).toHaveLength(1);
  });

  it('draws the last answer while the next is asked for, and the new one once it lands', async () => {
    const {counter, settle, changes} = await setUp();
    counter.set({on: true, cellPx: 12, highlighted: false});
    counter.look(camera([256, 256], 1));
    expect(counter.counts()).toBeNull();
    await settle();
    const first = counter.counts()!;
    expect(first.depth).toBe(cellDepth(1, 12));
    expect(first.cells.map((c) => c.count)).toEqual([9]);
    expect(changes.n).toBe(1);
    counter.look(camera([256, 256], 3));
    await vi.advanceTimersByTimeAsync(DENSITY_SETTLE_MS - 1);
    expect(counter.counts()).toBe(first);
    await settle();
    expect(counter.counts()!.depth).toBe(cellDepth(3, 12));
  });

  it('asks nothing for a pan inside the area held, and asks again past it or at another depth', async () => {
    const {counter, asked, settle} = await setUp();
    counter.set({on: true, cellPx: 12, highlighted: false});
    counter.look(camera([256, 256], 2));
    await settle();
    expect(asked).toHaveLength(1);
    // At zoom 2 the viewport is 250 world units wide and the area reaches 62.5 past each side.
    counter.look(camera([300, 256], 2));
    await settle();
    expect(asked).toHaveLength(1);
    // A small zoom out that keeps the depth and the viewport inside the area asks nothing either.
    counter.look(camera([300, 256], 1.9));
    await settle();
    expect(asked).toHaveLength(1);
    counter.look(camera([400, 256], 2));
    await settle();
    expect(asked).toHaveLength(2);
    counter.look(camera([400, 256], 3));
    await settle();
    expect(asked).toHaveLength(3);
  });

  it('asks again when the resolution or the highlight changes, and drops everything when turned off', async () => {
    const {store, counter, asked, settle, depths} = await setUp();
    counter.set({on: true, cellPx: 12, highlighted: false});
    counter.look(camera([256, 256], 2));
    await settle();
    counter.set({on: true, cellPx: 4, highlighted: false});
    await settle();
    expect(depths()).toEqual([cellDepth(2, 12), cellDepth(2, 4)]);
    counter.set({on: true, cellPx: 4, highlighted: true});
    await settle();
    expect(asked).toHaveLength(3);
    counter.set({on: false, cellPx: 4, highlighted: true});
    expect(counter.counts()).toBeNull();
    expect([...store.get('aggregates').keys()]).toEqual([]);
    counter.look(camera([100, 100], 4));
    await settle();
    expect(asked).toHaveLength(3);
  });

  it('never asks for a stop past the cell limit: it asks for the finest stop that fits', async () => {
    const limit = 20_000;
    const {counter, settle, depths} = await setUp(limit);
    const at = camera([256, 256], 2);
    const stops = resolutionStops(at, limit);
    const finest = stops.filter((s) => s.enabled).at(-1)!;
    counter.set({on: true, cellPx: 4, highlighted: false});
    counter.look(at);
    await settle();
    expect(stops.find((s) => s.px === 4)!.enabled).toBe(false);
    expect(depths()).toEqual([finest.depth]);
    expect(counter.stops()).toEqual(stops);
  });

  it('asks again at the depth a cell-limit refusal names', async () => {
    const {counter, refusals, settle, depths} = await setUp();
    refusals.push(new TesseraError(422, 'contract', '40000 cells at depth 9 in this area is more than selection.max_aggregate_cells allows (30000); ask for depth 8 or less, or a smaller area'));
    counter.set({on: true, cellPx: 4, highlighted: false});
    counter.look(camera([256, 256], 2));
    await settle();
    expect(depths()).toEqual([cellDepth(2, 4), 8]);
    expect(counter.counts()!.depth).toBe(8);
  });

  it('draws nothing after any other refusal', async () => {
    const {counter, refusals, settle} = await setUp();
    counter.set({on: true, cellPx: 12, highlighted: false});
    counter.look(camera([256, 256], 2));
    await settle();
    expect(counter.counts()).not.toBeNull();
    refusals.push(new TesseraError(403, 'bad-credential', 'the token is not valid'));
    counter.look(camera([256, 256], 4));
    await settle();
    expect(counter.counts()).toBeNull();
  });
});
