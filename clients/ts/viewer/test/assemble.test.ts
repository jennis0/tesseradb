import {describe, expect, it} from 'vitest';
import {bandsOfResult, mortonOfTile, tileXY, type Band, type ReplicaFrame} from '@tessera/client';
import type {ScalarColumn, ViewportResult} from '@tessera/client';
import {assemble, assertDrawsEveryServedMark} from '../src/assemble.js';

/** A band at `depth`/`prefix` whose points all sit in cell `(cx, cy)`. */
function band(depth: number, prefix: bigint, n: number, served = n, cell = {cx: 0, cy: 0}): Band {
  let morton = 0n;
  for (let bit = 0; bit < 16; bit++) {
    morton |= BigInt((cell.cx >> bit) & 1) << BigInt(2 * bit);
    morton |= BigInt((cell.cy >> bit) & 1) << BigInt(2 * bit + 1);
  }
  const scalars: Record<string, ScalarColumn> = {
    w: {arrowType: 'u32', values: Uint32Array.from({length: n}, (_, i) => i)}
  };
  return {
    depth,
    prefix,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: n}, () => morton << 32n),
    positions: Float64Array.from({length: n * 2}, (_, i) => (i % 2 === 0 ? cell.cx : cell.cy)),
    scalars,
    served,
    capUsed: 500,
    visible: BigInt(served * 3),
    matched: BigInt(served * 3),
    heldBelow: BigInt(n + 1),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0
  };
}

const WHOLE = {x0: 0, y0: 0, x1: 65535, y1: 65535};

function frame(depth: number, exact: Band[], fallback: Band[] = [], want = WHOLE): ReplicaFrame {
  return {
    depth,
    want,
    exact,
    fallback,
    response: null,
    plan: {wanted: 0, novel: 0, requests: 0}
  };
}

describe('assemble', () => {
  it('packs exact tiles and reports the served total against them', () => {
    const a = band(2, 0n, 3);
    const b = band(2, 1n, 2);
    const out = assemble(frame(2, [a, b]));

    expect(out.ids.length).toBe(5);
    expect(out.exactDrawn).toBe(5);
    expect(out.exactServed).toBe(5);
    expect(out.provisional).toBe(0);
    expect(out.tiles.map((t) => [t.from, t.to])).toEqual([
      [0, 3],
      [3, 5]
    ]);
    expect(out.visibleInView).toBe(15);
    assertDrawsEveryServedMark(out);
  });

  it('narrows cell space to world units once', () => {
    const out = assemble(frame(2, [band(2, 0n, 1, 1, {cx: 256, cy: 128})]));
    expect(out.positions[0]).toBeCloseTo(2, 6); // 256 cells / 128 cells-per-world-unit
    expect(out.positions[1]).toBeCloseTo(1, 6);
  });

  it('restricts an ancestor band to the tile asked for, and marks it provisional', () => {
    // A depth-4 parent holding points in two different depth-6 children.
    const parent = band(4, 0n, 0);
    const inChild = mortonOfTile(1, 0, 6); // one specific depth-6 tile
    const other = mortonOfTile(2, 0, 6);
    const codes = [inChild, inChild, other];
    const enriched: Band = {
      ...parent,
      ids: BigUint64Array.from([1n, 2n, 3n]),
      codes: BigUint64Array.from(codes.map((c) => c << BigInt(64 - 12))),
      positions: new Float64Array(6),
      scalars: {w: {arrowType: 'u32', values: Uint32Array.from([7, 8, 9])}},
      served: 3
    };

    const {x, y} = tileXY(inChild, 6);
    const out = assemble(frame(6, [], [enriched], {x0: x, y0: y, x1: x, y1: y}));

    expect(out.ids.length).toBe(2); // only the two points inside that region
    expect([...out.ids]).toEqual([1n, 2n]);
    expect([...(out.scalars.w!.values as Uint32Array)]).toEqual([7, 8]);
    expect(out.provisional).toBe(2);
    expect(out.exactDrawn).toBe(0);
    expect(out.tiles[0]!.exact).toBe(false);
    expect(out.tiles[0]!.counts).toBeNull(); // the number channel is suppressed
    assertDrawsEveryServedMark(out);
  });

  it('unions descendant bands on zoom-out, marked provisional', () => {
    const kids = [band(5, 0n, 2), band(5, 1n, 3)];
    const out = assemble(frame(3, [], kids));
    expect(out.ids.length).toBe(5);
    expect(out.provisional).toBe(5);
    expect(out.exactServed).toBe(0); // no exact tile contributed, so nothing to assert against
    expect(out.tiles[0]!.counts).toBeNull();
    assertDrawsEveryServedMark(out);
  });

  it('mixes exact and provisional tiles without conflating their counts', () => {
    const exact = band(4, 0n, 3);
    const kids = [band(6, 40n, 2)];
    const out = assemble(frame(4, [exact], kids));

    expect(out.ids.length).toBe(5);
    expect(out.exactDrawn).toBe(3);
    expect(out.exactServed).toBe(3);
    expect(out.provisional).toBe(2);
    expect(out.visibleInView).toBe(9); // the exact tile alone
    assertDrawsEveryServedMark(out);
  });

  it('carries scalars through a mixed assembly in point order', () => {
    const a = band(2, 0n, 2);
    const b = band(2, 1n, 3);
    const out = assemble(frame(2, [a, b]));
    expect([...(out.scalars.w!.values as Uint32Array)]).toEqual([0, 1, 0, 1, 2]);
  });

  it('splits a real response into bands and reassembles it identically', () => {
    const result: ViewportResult = {
      tiles: [
        {tile: 0n, visible: 9n, matched: 9n, served: 3n},
        {tile: 1n, visible: 4n, matched: 4n, served: 2n}
      ],
      ids: BigUint64Array.from([1n, 2n, 3n, 4n, 5n]),
      codes: new BigUint64Array(5),
      positions: Float64Array.from([0, 0, 1, 1, 2, 2, 3, 3, 4, 4]),
      scalars: {w: {arrowType: 'u32', values: Uint32Array.from([10, 11, 12, 13, 14])}},
      subCells: null
    };
    const bands = bandsOfResult(result, 2, {identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0});

    const out = assemble(frame(2, bands));

    expect([...out.ids]).toEqual([...result.ids]);
    expect([...(out.scalars.w!.values as Uint32Array)]).toEqual([10, 11, 12, 13, 14]);
    expect(out.exactDrawn).toBe(5);
    assertDrawsEveryServedMark(out);
  });
});

describe('assertDrawsEveryServedMark', () => {
  it('throws when an exact tile draws fewer marks than were served', () => {
    const short = band(2, 0n, 2, 3); // holds 2, server said it served 3
    const out = assemble(frame(2, [short]));
    expect(() => assertDrawsEveryServedMark(out)).toThrow(/I7: drawing 2 marks/);
  });

  it('throws when a provisional tile carries counts', () => {
    const out = assemble(frame(2, [], [band(4, 0n, 2)]));
    out.tiles[0]!.counts = {visible: 1n, matched: 1n, served: 1};
    expect(() => assertDrawsEveryServedMark(out)).toThrow(/superset of marks must never be read as density/);
  });
});
