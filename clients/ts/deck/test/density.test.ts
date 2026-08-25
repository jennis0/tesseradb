import {describe, expect, it} from 'vitest';
import {mortonOfTile, type ComposedTile} from '@tesseradb/client';
import {binDensity} from '../src/density.js';
import {resolvePick} from '../src/pick.js';

/**
 * The wash reads the number channel and only the exact half of it: counts land in the bin of the
 * tile they belong to at the drawn depth, and a non-exact tile contributes nothing at all.
 */

const tile = (x: number, y: number, depth: number, exact: boolean, matched: number, visible = matched): ComposedTile => ({
  prefix: mortonOfTile(x, y, depth),
  depth,
  exact,
  drawn: exact ? 10 : 7,
  counts: exact ? {visible: BigInt(visible), matched: BigInt(matched), served: 10} : null
});

describe('binDensity', () => {
  it('lands each exact tile’s count in its own bin, over the rectangle the tiles span', () => {
    const image = binDensity([tile(4, 6, 3, true, 100), tile(6, 7, 3, true, 5), tile(5, 6, 3, true, 0)], 3)!;
    expect(image.width).toBe(3);
    expect(image.height).toBe(2);
    // Depth 3: 64 world units a tile; x 4..6, y 6..7.
    expect(image.bounds).toEqual([256, 384, 448, 512]);
    const alphaAt = (x: number, y: number) => image.data[((y - 6) * 3 + (x - 4)) * 4 + 3]!;
    expect(alphaAt(4, 6)).toBeGreaterThan(alphaAt(6, 7)); // the larger count is the stronger bin
    expect(alphaAt(6, 7)).toBeGreaterThan(0);
    expect(alphaAt(5, 6)).toBe(0); // a zero count is transparent
    expect(alphaAt(5, 7)).toBe(0); // a tile not on screen is transparent
    expect(image.filled).toBe(2);
  });

  it('gives a non-exact tile no bin — a superset must not read as density', () => {
    const image = binDensity([tile(1, 1, 3, true, 50), tile(2, 1, 3, false, 999)], 3)!;
    expect(image.width).toBe(1);
    expect(image.filled).toBe(1);
    expect(image.bounds).toEqual([64, 64, 128, 128]);
  });

  it('ignores an exact tile at another depth, and washes nothing when none is at the drawn one', () => {
    expect(binDensity([tile(1, 1, 4, true, 50)], 3)).toBeNull();
    expect(binDensity([tile(2, 1, 3, false, 999)], 3)).toBeNull();
    expect(binDensity([], 3)).toBeNull();
  });

  it('washes the chosen channel', () => {
    const tiles = [tile(0, 0, 2, true, 1, 1000), tile(1, 0, 2, true, 1000, 1000)];
    const matched = binDensity(tiles, 2, 'matched')!;
    const visible = binDensity(tiles, 2, 'visible')!;
    expect(matched.data[3]).toBeLessThan(matched.data[7]!);
    expect(visible.data[3]).toBe(visible.data[7]);
  });
});

describe('resolvePick — a miss is not a broken pick', () => {
  const ids = BigUint64Array.from([11n, 12n]);
  it('resolves a mark by its sublayer’s identity array', () => {
    expect(resolvePick({index: 1, sourceLayer: {id: 'm', props: {tesseraIds: ids}}, coordinate: [3, 4]})).toEqual({
      kind: 'mark',
      id: 12n,
      worldXY: [3, 4]
    });
  });
  it('resolves an artifact marker on its own route', () => {
    expect(resolvePick({index: 0, sourceLayer: {id: 'a', props: {artifactIds: [99n]}}})).toEqual({kind: 'artifact', id: 99n});
  });
  it('reports a miss for nothing under the cursor', () => {
    expect(resolvePick({index: -1})).toEqual({kind: 'miss'});
  });
  it('reports a hit it cannot resolve as broken, naming the layer', () => {
    expect(resolvePick({index: 5, sourceLayer: {id: 'm', props: {tesseraIds: ids}}})).toEqual({
      kind: 'broken',
      index: 5,
      layer: 'm',
      hasIds: true,
      idCount: 2
    });
    expect(resolvePick({index: 0, sourceLayer: {id: 'x', props: {}}}).kind).toBe('broken');
  });
});
