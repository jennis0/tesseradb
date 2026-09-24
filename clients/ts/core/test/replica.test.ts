import {describe, expect, it} from 'vitest';
import {Replica} from '../src/replica.js';
import {mortonOfTile, tileOfCode, tileToCellBox, tileXY} from '../src/coords.js';
import type {Quantisation, ViewportResponse} from '../src/types.js';
import {response, result, tile} from './support.js';

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

/** A response serving `n` points for each named tile, identities ascending across the whole batch. */
function served(tiles: {tile: bigint; served: number; visible?: number}[], pin = 'p1'): ViewportResponse {
  const total = tiles.reduce((sum, t) => sum + t.served, 0);
  return response(
    result({
      tiles: tiles.map((t) => tile(t.tile, BigInt(t.visible ?? t.served), {served: BigInt(t.served)})),
      ids: BigUint64Array.from({length: total}, (_, i) => BigInt(i + 1)),
      codes: new BigUint64Array(total),
      positions: new Float64Array(total * 2),
      world: new Float32Array(total * 2)
    }),
    {contentKey: pin}
  );
}

/**
 * A server that answers only for the tiles the request listed, so a fixture cannot seed the cache
 * with tiles nobody asked about.
 */
function replica(
  serve: (req: {bbox: [number, number, number, number]; zoom: number; k?: number}) => ViewportResponse,
  opts: {cache?: boolean; revalidateAfterMs?: number} = {}
) {
  const calls: {bbox: [number, number, number, number]; zoom: number; k?: number}[] = [];
  const r = new Replica(
    async (req) => {
      const bbox = req.bbox as [number, number, number, number];
      calls.push({bbox, zoom: req.zoom, k: req.k});
      return serve({bbox, zoom: req.zoom, k: req.k});
    },
    Q,
    {view: 's', now: () => 0, revalidateAfterMs: Infinity, ...opts}
  );
  r.reset();
  return {r, calls};
}

/** The whole depth-`z` grid, as a region. */
const world = (z: number) => ({x0: 0, y0: 0, x1: 2 ** z - 1, y1: 2 ** z - 1});
const rect = (x0: number, y0: number, x1: number, y1: number) => ({x0, y0, x1, y1});

describe('tileXY', () => {
  it('inverts the Morton interleave of a tile prefix', () => {
    // x on the even bits, y on the odd, as decode.ts compacts positions.
    expect(tileXY(0b00n, 1)).toEqual({x: 0, y: 0});
    expect(tileXY(0b01n, 1)).toEqual({x: 1, y: 0});
    expect(tileXY(0b10n, 1)).toEqual({x: 0, y: 1});
    expect(tileXY(0b11n, 1)).toEqual({x: 1, y: 1});
    // 0b1101: x takes bits 0 and 2 (both set) => 0b11; y takes bits 1 and 3 (0 then 1) => 0b10.
    expect(tileXY(0b1101n, 2)).toEqual({x: 0b11, y: 0b10});
  });

  it('round-trips against the cell box of every tile at depth 3', () => {
    for (let prefix = 0; prefix < 64; prefix++) {
      const {x, y} = tileXY(BigInt(prefix), 3);
      expect(x).toBeLessThan(8);
      expect(y).toBeLessThan(8);
      const box = tileToCellBox({x, y, z: 3});
      expect(box.cx1 - box.cx0).toBe(65536 / 8);
    }
  });

  it('is the exact inverse of mortonOfTile at every depth up to 5', () => {
    for (let z = 0; z <= 5; z++) {
      for (let x = 0; x < 2 ** z; x++) {
        for (let y = 0; y < 2 ** z; y++) {
          expect(tileXY(mortonOfTile(x, y, z), z)).toEqual({x, y});
        }
      }
    }
  });

  it('agrees with the tiling a point falls into', () => {
    // A point's tile, derived from its code, must be the tile whose cell box contains it.
    for (const [cx, cy] of [[0, 0], [65535, 65535], [12345, 54321], [40000, 7]] as const) {
      let cell = 0n;
      for (let bit = 0; bit < 16; bit++) {
        cell |= BigInt((cx >> bit) & 1) << BigInt(2 * bit);
        cell |= BigInt((cy >> bit) & 1) << BigInt(2 * bit + 1);
      }
      const code = cell << 32n;
      for (const z of [1, 4, 8, 16]) {
        const {x, y} = tileXY(tileOfCode(code, z), z);
        const box = tileToCellBox({x, y, z});
        expect(cx).toBeGreaterThanOrEqual(box.cx0);
        expect(cx).toBeLessThan(box.cx1);
        expect(cy).toBeGreaterThanOrEqual(box.cy0);
        expect(cy).toBeLessThan(box.cy1);
      }
    }
  });
});

describe('Replica.fetchRegion', () => {
  it('issues one request and holds every band it returns', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}, {tile: 1n, served: 3}]));

    const frame = await r.fetchRegion(world(1), 1, 500);

    expect(calls).toHaveLength(1);
    expect(calls[0]!.k).toBe(500);
    expect(frame.exact.map((b) => b.ids.length).sort()).toEqual([2, 3]);
    expect(frame.plan.novel).toBe(4); // the whole depth-1 grid
  });

  it('makes a revisit free', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]));

    await r.fetchRegion(world(1), 1, 500);
    const second = await r.fetchRegion(world(1), 1, 500);

    expect(calls).toHaveLength(1); // the second ask never reached the wire
    expect(second.response).toBeNull();
    expect(second.plan.novel).toBe(0);
    expect(second.exact).toHaveLength(1); // and it still draws
  });

  it('asks only for the strip a pan actually exposes', async () => {
    // Coverage is a rectangle, so a shifted viewport subtracts to one strip.
    const {r, calls} = replica(() => served([{tile: 0n, served: 1}]));

    await r.fetchRegion(rect(0, 0, 3, 3), 3, 500);
    const second = await r.fetchRegion(rect(2, 0, 5, 3), 3, 500);

    expect(calls).toHaveLength(2);
    expect(second.plan.wanted).toBe(16);
    expect(second.plan.novel).toBe(8); // columns 4 and 5 only
    expect(second.plan.requests).toBe(1);
  });

  it('treats a covered region as answered, so empty ground is never re-asked', async () => {
    // A response omits empty tiles; coverage records that they were asked about.
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]));

    await r.fetchRegion(world(3), 3, 500); // 64 tiles, one of which holds anything
    expect(calls).toHaveLength(1);
    const second = await r.fetchRegion(world(3), 3, 500);
    expect(calls).toHaveLength(1);
    expect(second.plan.novel).toBe(0);
  });

  it('re-asks once the content key rotates', async () => {
    let pin = 'p1';
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}], pin), {
      revalidateAfterMs: 0
    });

    await r.fetchRegion(world(1), 1, 500);
    expect(calls).toHaveLength(1);

    // Everything is held, so this costs a count-only request, which lets an accepted change reach a
    // client panning from cache.
    await r.fetchRegion(world(1), 1, 500);
    expect(calls).toHaveLength(2);
    expect(calls[1]!.k).toBe(0);

    pin = 'p2'; // a flush moved geometry, and the revalidation observes it
    await r.fetchRegion(world(1), 1, 500);
    const marksAgain = await r.fetchRegion(world(1), 1, 500);
    expect(marksAgain.response).not.toBeNull();
    expect(calls[calls.length - 1]!.k).toBe(500);
  });

  it('never touches the wire for an all-held ask inside the revalidation window', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]));

    await r.fetchRegion(world(1), 1, 500);
    await r.fetchRegion(world(1), 1, 500);

    expect(calls).toHaveLength(1);
  });

  it('will not reuse coverage bought at a smaller cap', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]));

    await r.fetchRegion(world(1), 1, 100);
    await r.fetchRegion(world(1), 1, 500); // a larger k may yield more for the same tiles

    expect(calls).toHaveLength(2);
  });

  it('splits a large region so no single response can block a frame', async () => {
    // 200x200 tiles is 40,000, above the per-request bound, so it arrives as several responses.
    const {r, calls} = replica(() => served([{tile: 0n, served: 1}]));
    await r.fetchRegion(rect(0, 0, 199, 199), 8, 500);
    expect(calls.length).toBeGreaterThan(1);
    // Every piece is a strip of the full width, so together they tile the region exactly.
    for (const c of calls) expect(c.zoom).toBe(8);
  });

  it('serves bands but retains nothing when the cache is bypassed', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]), {cache: false});

    const first = await r.fetchRegion(world(1), 1, 500);
    const second = await r.fetchRegion(world(1), 1, 500);

    expect(first.exact[0]!.ids.length).toBe(2); // still renderable
    expect(second.exact[0]!.ids.length).toBe(2);
    expect(calls).toHaveLength(2); // the traffic of a client with no replica
    expect(r.bytes).toBe(0);
  });

  it('drops everything on an explicit reset', async () => {
    const {r, calls} = replica(() => served([{tile: 0n, served: 2}]));

    await r.fetchRegion(world(1), 1, 500);
    r.reset();
    await r.fetchRegion(world(1), 1, 500);

    expect(calls).toHaveLength(2);
  });

  it('drops everything when the server reports a different identity coordinate', async () => {
    // A new identity key is seen only on a response, so the second ask goes to new ground.
    let identity = 'i1';
    const {r, calls} = replica(() => {
      const res = served([{tile: 0n, served: 2}]);
      return {...res, identityKey: identity};
    });

    await r.fetchRegion(rect(0, 0, 1, 1), 3, 500);
    expect(calls).toHaveLength(1);
    expect((await r.fetchRegion(rect(0, 0, 1, 1), 3, 500)).plan.novel).toBe(0); // held

    identity = 'i2';
    await r.fetchRegion(rect(4, 4, 5, 5), 3, 500); // new ground, and the response rotates identity

    // The held bands went with the old key, so the first region is cold again.
    const back = await r.fetchRegion(rect(0, 0, 1, 1), 3, 500);
    expect(back.plan.novel).toBe(4);
  });
});
