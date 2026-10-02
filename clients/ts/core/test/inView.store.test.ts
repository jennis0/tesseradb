import {Table} from 'apache-arrow';
import {describe, expect, it} from 'vitest';
import type {TesseraClient} from '../src/client.js';
import type {FilterDraft} from '../src/filters.js';
import {createStore} from '../src/store.js';
import type {AggregateRequest, AggregateResult, FilterExpr} from '../src/types.js';
import {TesseraError} from '../src/client.js';
import {tileRectOfBbox} from '../src/budget.js';
import {mortonOfTile, tileXY} from '../src/coords.js';
import {fakeClock, fakeScheduler, meta, response, result as viewportResult, tile, view} from './support.js';

/**
 * The store's counts in view (`view.inView`) against a fake server that counts as the real one
 * does: the reference set is the area alone, so its total does not move with the filters.
 */

const META = meta({
  views: [view('s0')],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

const BOX: [number, number, number, number] = [0.1, 0.2, 0.6, 0.7];
const LEAF = {region: {bbox: BOX}};
const CS: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}};
const LIT: FilterDraft = {filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}};

/** Whether `expr` names the archive filter anywhere. */
function narrows(expr: FilterExpr | undefined): boolean {
  return expr !== undefined && JSON.stringify(expr).includes('archive');
}

/** 1,000 items visible in any area, 250 of them in `cs`. */
function counted(req: AggregateRequest): AggregateResult {
  const total = narrows(req.filters) ? 250 : 1_000;
  return {
    tables: req.groupings.map((_, grouping) => ({grouping, total, referenceTotal: req.reference === undefined ? null : 1_000, groups: null, rows: new Table()})),
    region: {exact: true, depth: null},
    recomposed: false,
    identityKey: 'ik',
    next: null
  };
}

/** One point at the centre of every tile the request covers in the left half of the unit square, the view's extent. */
function onePerTile(req: {zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]}) {
  const span = 512 / 2 ** req.zoom;
  const at = (x: number, y: number) => ({prefix: mortonOfTile(x, y, req.zoom), x: (x + 0.5) * span, y: (y + 0.5) * span});
  const cells: {prefix: bigint; x: number; y: number}[] = [];
  if (req.tiles) {
    for (const t of req.tiles) {
      const {x, y} = tileXY(t, req.zoom);
      cells.push(at(x, y));
    }
  } else {
    const [x0, y0, x1, y1] = req.bbox!;
    const rect = tileRectOfBbox([x0 * 512, y0 * 512, x1 * 512, y1 * 512], req.zoom);
    for (let y = rect.y0; y <= rect.y1; y++) for (let x = rect.x0; x <= rect.x1; x++) cells.push(at(x, y));
  }
  const left = cells.filter((c) => c.x < 256);
  return viewportResult({
    tiles: left.map((c) => tile(c.prefix, 1n, {served: 1n})),
    ids: BigUint64Array.from(left.map((c) => c.prefix + 1n)),
    codes: BigUint64Array.from(left.map(() => 0n)),
    positions: Float64Array.from(left.flatMap((c) => [c.x / 512, c.y / 512])),
    world: Float32Array.from(left.flatMap((c) => [c.x, c.y]))
  });
}

/**
 * A store over a fake server. `refuse` refuses every aggregate; `points` serves one point a tile
 * (see {@link onePerTile}).
 */
async function storeWith(opts: {refuse?: boolean; points?: boolean} = {}) {
  const clock = fakeClock();
  const scheduler = fakeScheduler();
  const asked: AggregateRequest[] = [];
  const client = {
    meta: async () => META,
    viewport: async (_token: string, req: {zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]}) => response(opts.points ? onePerTile(req) : viewportResult()),
    aggregate: async (_token: string, req: AggregateRequest) => {
      asked.push(req);
      if (opts.refuse) throw new TesseraError(422, 'contract', 'refused');
      return counted(req);
    },
    close: () => {}
  } as unknown as TesseraClient;
  const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false});
  await clock.advance(1);
  return {store, asked, clock, scheduler};
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe('the counts in view', () => {
  it('asks once the camera has rested, over the camera’s box, with the box as the reference', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(100);
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(100);
    await flush();
    expect(asked).toHaveLength(0);
    await clock.advance(200);
    await flush();
    expect(asked).toEqual([{view: 's0', groupings: [{}], filters: LEAF, reference: LEAF}]);
    expect(store.get('view').inView).toMatchObject({status: 'shown', visible: {value: 1_000, exact: true}, matched: {value: 1_000, exact: true}});
  });

  it('keeps the visible count when a filter changes and the camera does not', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(300);
    await flush();
    store.setFilters(CS);
    await flush();
    expect(asked.at(-1)!.reference).toEqual(LEAF);
    const n = store.get('view').inView!;
    expect(n.visible.value).toBe(1_000);
    expect(n.matched.value).toBe(250);
  });

  it('counts the highlight beside the filters while one is set', async () => {
    const {store, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(300);
    store.setFilters(LIT);
    await flush();
    const n = store.get('view').inView!;
    expect(n.matched.value).toBe(1_000);
    expect(n.highlighted.value).toBe(250);
  });

  it('counts the selected region in place of the box, and the region carries the same figures', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: [0, 0, 1, 1], width: 400, height: 400});
    await clock.advance(300);
    store.setFilters(CS);
    store.select({kind: 'box', bbox: BOX});
    await flush();
    const last = asked.at(-1)!;
    expect(last.reference).toEqual(LEAF);
    expect(JSON.stringify(last.filters)).toContain(JSON.stringify(LEAF));
    const n = store.get('view').inView!;
    expect(n.visible.value).toBe(1_000);
    expect(n.matched.value).toBe(250);
  });

  it('asks nothing more as the camera moves while a region is selected, and stays shown', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: [0, 0, 1, 1], width: 400, height: 400});
    await clock.advance(300);
    store.select({kind: 'box', bbox: BOX});
    await flush();
    const before = asked.length;
    store.setView({bbox: [0.2, 0.2, 0.8, 0.8], width: 400, height: 400});
    await clock.advance(300);
    await flush();
    expect(asked).toHaveLength(before);
    expect(store.get('view').inView?.status).toBe('shown');
  });

  it('keeps figures on show when the count is refused, marked refused, from the frame where none landed', async () => {
    const {store, clock} = await storeWith({refuse: true});
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(300);
    await flush();
    const n = store.get('view').inView;
    expect(n).not.toBeNull();
    expect(n!.status).toBe('refused');
    expect(n!.visible).toEqual(store.get('view').visible);
  });

  it('counts the marks shown over the box the figures were counted over, until the next count lands', async () => {
    const {store, clock, scheduler} = await storeWith({points: true});
    const shown = () => store.get('view').inView?.shown;
    /** The marks of the frame's bands inside a data box. */
    const inside = (box: [number, number, number, number]) =>
      store.get('marks').bands.reduce((n, b) => n + Array.from({length: b.ids.length}, (_, i) => [b.positions[i * 2]! / 512, b.positions[i * 2 + 1]! / 512]).filter(([x, y]) => x! >= box[0] && x! <= box[2] && y! >= box[1] && y! <= box[3]).length, 0);
    const a: [number, number, number, number] = [0, 0, 0.5, 0.5];
    const b: [number, number, number, number] = [0.25, 0, 0.75, 0.5];
    const settle = async (ms: number) => {
      await clock.advance(ms);
      scheduler.flush();
      await flush();
    };
    store.setView({bbox: a, width: 400, height: 400});
    await settle(1_000);
    expect(shown()).toBe(inside(a));
    expect(shown()).toBeGreaterThan(0);
    // The camera pans; until the next count lands the figures on show are the first box's, and the
    // shown count is taken over that box too.
    store.setView({bbox: b, width: 400, height: 400});
    const frames = store.get('view').composition;
    await settle(200);
    expect(store.get('view').composition).not.toBe(frames);
    expect(shown()).toBe(inside(a));
    await settle(1_000);
    expect(shown()).toBe(inside(b));
    expect(inside(b)).toBeLessThan(inside(a));
  });
});
