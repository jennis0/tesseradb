import {describe, expect, it} from 'vitest';
import {Replica} from '../src/replica.js';
import {mortonOfTile, tileOfCode, tileToCellBox, tileXY} from '../src/coords.js';
import {tilesInBbox, tilesOfBbox} from '../src/budget.js';

import type {Quantisation, ViewportResponse, ViewportResult} from '../src/types.js';

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

/** A response serving `n` points for each named tile, identities ascending across the whole batch. */
function response(tiles: {tile: bigint; served: number; visible?: number}[], pin = 'p1'): ViewportResponse {
  const total = tiles.reduce((sum, t) => sum + t.served, 0);
  const result: ViewportResult = {
    tiles: tiles.map((t) => ({
      tile: t.tile,
      visible: BigInt(t.visible ?? t.served),
      matched: BigInt(t.visible ?? t.served),
      served: BigInt(t.served)
    })),
    ids: BigUint64Array.from({length: total}, (_, i) => BigInt(i + 1)),
    codes: new BigUint64Array(total),
    positions: new Float64Array(total * 2),
    scalars: {},
    subCells: null
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: pin,
    pin,
    stale: false,
    bytes: 0
  };
}

/**
 * A server that answers only for the tiles its bbox actually covers — which is what makes the
 * "requests only what it does not hold" test mean anything. A fixture returning every tile
 * regardless would seed the cache with tiles no request ever asked for.
 */
function serveCovered(tiles: {tile: bigint; served: number}[], pin: () => string) {
  return (req: {bbox: [number, number, number, number]; zoom: number}) => {
    const covered = tiles.filter(({tile}) => {
      const {x, y} = tileXY(tile, req.zoom);
      const box = tileToCellBox({x, y, z: req.zoom});
      const [x0, y0, x1, y1] = req.bbox;
      return (
        box.cx0 / 65536 <= x1 && box.cx1 / 65536 >= x0 && box.cy0 / 65536 <= y1 && box.cy1 / 65536 >= y0
      );
    });
    return response(covered, pin());
  };
}

function replica(
  serve: (req: {bbox: [number, number, number, number]; zoom: number; k?: number}) => ViewportResponse,
  opts: {cache?: boolean; revalidateAfterMs?: number} = {}
) {
  const calls: {bbox: [number, number, number, number]; zoom: number; k?: number}[] = [];
  const r = new Replica(
    async (req) => {
      calls.push({bbox: req.bbox, zoom: req.zoom, k: req.k});
      return serve(req);
    },
    Q,
    {slice: 's', now: () => 0, revalidateAfterMs: Infinity, ...opts}
  );
  r.reset();
  return {r, calls};
}

describe('tileXY', () => {
  it('inverts the Morton interleave of a tile prefix', () => {
    // x occupies the even bits, y the odd — the same convention decode.ts compacts positions under.
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
    for (const [cx, cy] of [[0, 0], [65535, 65535], [12345, 54321], [40000, 7]]) {
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

describe('tilesOfBbox', () => {
  it('enumerates exactly what tilesInBbox counts', () => {
    for (const box of [
      [0, 0, 512, 512],
      [100, 100, 140, 260],
      [0, 0, 1, 1],
      [511, 511, 512, 512]
    ] as [number, number, number, number][]) {
      for (const z of [1, 3, 5]) {
        const tiles = tilesOfBbox(box, z);
        expect(tiles).toHaveLength(tilesInBbox(box, z));
        expect(new Set(tiles).size).toBe(tiles.length); // no duplicates: a tile served twice doubles it
      }
    }
  });
});

describe('Replica.fetchTiles', () => {
  it('issues one request and answers every tile from it', async () => {
    const {r, calls} = replica(() => response([{tile: 0n, served: 2}, {tile: 1n, served: 3}]));

    const frame = await r.fetchTiles([0n, 1n], 1, 500);

    expect(calls).toHaveLength(1);
    expect(frame.missing).toEqual([]);
    expect(frame.tiles.map((t) => t.prefix)).toEqual([0n, 1n]);
    expect(frame.tiles[0]!.resolved.bands[0]!.ids.length).toBe(2);
    expect(frame.tiles[1]!.resolved.bands[0]!.ids.length).toBe(3);
  });

  it('makes a revisit free', async () => {
    const {r, calls} = replica(() => response([{tile: 0n, served: 2}]));

    await r.fetchTiles([0n], 1, 500);
    const second = await r.fetchTiles([0n], 1, 500);

    expect(calls).toHaveLength(1); // the second ask never reached the wire
    expect(second.response).toBeNull();
    expect(second.tiles[0]!.resolved.provenance).toBe('exact');
  });

  it('revalidates an all-held ask, and re-fetches once the key has rotated', async () => {
    let pin = 'p1';
    const {r, calls} = replica(serveCovered([{tile: 0n, served: 2}], () => pin), {
      revalidateAfterMs: 0
    });

    await r.fetchTiles([0n], 1, 500);
    expect(calls).toHaveLength(1);

    // Everything is held, so this ask costs a counts-only request — and nothing more. That is what
    // keeps an accepted change from staying invisible to a client that pans entirely from cache.
    await r.fetchTiles([0n], 1, 500);
    expect(calls).toHaveLength(2);
    expect(calls[1]!.k).toBe(0);

    pin = 'p2'; // a flush moved geometry, and the revalidation observes it
    await r.fetchTiles([0n], 1, 500);
    // The held band still carries p1: renderable, but no longer declarable, so marks come again.
    const marksAgain = await r.fetchTiles([0n], 1, 500);
    expect(marksAgain.response).not.toBeNull();
    expect(calls[calls.length - 1]!.k).toBe(500);
  });

  it('remembers that a tile is empty, so a mostly-empty view stops re-asking', async () => {
    // The view wants four tiles; only one holds anything. Without negative caching the other three
    // are re-requested forever and no revisit is ever free.
    const {r, calls} = replica(serveCovered([{tile: 0n, served: 2}], () => 'p1'));

    const first = await r.fetchTiles([0n, 1n, 2n, 3n], 1, 500);
    expect(first.plan).toEqual({omitted: 0, fetched: 4});
    expect(calls).toHaveLength(1);

    const second = await r.fetchTiles([0n, 1n, 2n, 3n], 1, 500);
    expect(second.plan).toEqual({omitted: 4, fetched: 0});
    expect(calls).toHaveLength(1); // nothing on the wire at all
  });

  it('re-asks about an empty tile once the content key rotates', async () => {
    // A flush can put rows in a tile that had none, so emptiness expires exactly as a band does.
    let pin = 'p1';
    const {r, calls} = replica(serveCovered([{tile: 0n, served: 2}], () => pin));

    await r.fetchTiles([0n, 1n], 1, 500);
    expect(calls).toHaveLength(1);
    await r.fetchTiles([0n, 1n], 1, 500);
    expect(calls).toHaveLength(1);

    pin = 'p2';
    // Nothing observes the rotation until a request happens, so force one with a fresh tile.
    await r.fetchTiles([0n, 1n, 2n], 1, 500);
    const after = await r.fetchTiles([0n, 1n], 1, 500);
    expect(after.plan.fetched).toBeGreaterThan(0);
  });

  it('never touches the wire for an all-held ask inside the revalidation window', async () => {
    const {r, calls} = replica(serveCovered([{tile: 0n, served: 2}], () => 'p1'));

    await r.fetchTiles([0n], 1, 500);
    await r.fetchTiles([0n], 1, 500);

    expect(calls).toHaveLength(1);
  });

  it('requests only the tiles it does not hold', async () => {
    const {r, calls} = replica(
      serveCovered([{tile: 0n, served: 2}, {tile: 3n, served: 2}], () => 'p1')
    );

    await r.fetchTiles([0n], 1, 500);
    await r.fetchTiles([0n, 3n], 1, 500);

    expect(calls).toHaveLength(2);
    // The second request's box covers tile 3 alone, not the union with the held tile 0.
    const {x, y} = tileXY(3n, 1);
    const cells = tileToCellBox({x, y, z: 1});
    expect(calls[1]!.bbox[0]).toBeCloseTo((cells.cx0 + 0.5) / 65536, 6);
  });

  it('serves bands but retains nothing when the cache is bypassed', async () => {
    const {r, calls} = replica(() => response([{tile: 0n, served: 2}]), {cache: false});

    const first = await r.fetchTiles([0n], 1, 500);
    const second = await r.fetchTiles([0n], 1, 500);

    expect(first.tiles[0]!.resolved.bands[0]!.ids.length).toBe(2); // still renderable
    expect(second.tiles[0]!.resolved.bands[0]!.ids.length).toBe(2);
    expect(calls).toHaveLength(2); // and byte-for-byte the traffic of a client with no replica
    expect(r.bytes).toBe(0);
  });

  it('drops everything on an explicit reset', async () => {
    const {r, calls} = replica(() => response([{tile: 0n, served: 2}]));

    await r.fetchTiles([0n], 1, 500);
    r.reset();
    await r.fetchTiles([0n], 1, 500);

    expect(calls).toHaveLength(2);
  });

  it('drops everything when the server reports a different identity coordinate', async () => {
    // Belt to reset()'s braces: the partition key comes from the server, so a client cannot hold
    // one principal's bands under another's by forgetting to call anything.
    let identity = 'alice';
    const {r, calls} = replica((req) => ({
      ...serveCovered([{tile: 0n, served: 2}], () => 'p1')(req),
      identityKey: identity
    }));

    await r.fetchTiles([0n], 1, 500);
    expect(r.bytes).toBeGreaterThan(0);

    identity = 'bob';
    await r.fetchTiles([0n, 1n], 1, 500);
    // Tile 0's band was dropped when the coordinate moved, so it is asked for again.
    const after = await r.fetchTiles([0n], 1, 500);
    expect(calls.length).toBeGreaterThanOrEqual(2);
    expect(after.tiles[0]?.resolved.bands[0]?.identityKey).toBe('bob');
  });
});

describe('Replica.tile coalescing', () => {
  it('answers many per-tile asks with one request', async () => {
    const {r, calls} = replica(() =>
      response([
        {tile: 0n, served: 1},
        {tile: 1n, served: 1},
        {tile: 2n, served: 1},
        {tile: 3n, served: 1}
      ])
    );

    // What a deck.gl TileLayer does: one getTileData per tile, all in the same turn.
    const asks = [r.tile(0n, 1, 500), r.tile(1n, 1, 500), r.tile(2n, 1, 500), r.tile(3n, 1, 500)];
    const resolved = await Promise.all(asks);

    expect(calls).toHaveLength(1);
    expect(resolved.every((x) => x !== null)).toBe(true);
  });

  it('resolves a batch to null rather than rejecting when the request fails', async () => {
    const r = new Replica(
      async () => {
        throw new Error('transport');
      },
      Q,
      {slice: 's', now: () => 0}
    );
    r.reset();

    const resolved = await Promise.all([r.tile(0n, 1, 500), r.tile(1n, 1, 500)]);

    expect(resolved).toEqual([null, null]);
  });
});
