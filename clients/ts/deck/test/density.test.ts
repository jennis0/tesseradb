import {describe, expect, it} from 'vitest';
import {mortonOfTile, type ComposedTile} from '@tesseradb/client';
import {binDensity} from '../src/density.js';
import {resolvePick} from '../src/pick.js';

/**
 * The wash reads the number channel and only the exact half of it: counts land in the bin of the
 * tile they belong to at the drawn depth, and a non-exact tile contributes nothing at all.
 */

const tile = (x: number, y: number, depth: number, exact: boolean, matched: number, visible = matched, highlighted?: number): ComposedTile => ({
  prefix: mortonOfTile(x, y, depth),
  depth,
  exact,
  drawn: exact ? 10 : 7,
  counts: exact ? {visible: BigInt(visible), matched: BigInt(matched), highlighted: BigInt(highlighted ?? matched), served: 10} : null
});

describe('binDensity', () => {
  /**
   * §5.3's wash: the channel is what the interface chose, and it is what shows the members the cap
   * clause did not draw — a highlight over 27 million articles draws 66,000 of them. What is
   * checked is that the channel is *read*, not that the image looks a particular way: two tiles
   * whose `matched` are equal and whose `highlighted` are not must bin differently under it.
   */
  it('washes the channel it is given, so a highlight and a filter are different pictures', () => {
    const tiles = [tile(0, 0, 1, true, 100, 100, 1), tile(1, 0, 1, true, 100, 100, 100)];
    const matched = binDensity(tiles, 1, 'matched')!;
    const highlighted = binDensity(tiles, 1, 'highlighted')!;
    // Equal `matched` in both bins is one distinct count, so both bins take the same intensity.
    expect(matched.data[3]).toBe(matched.data[7]);
    // The highlight tells them apart: the tile matching one is far below the tile matching a
    // hundred, and neither is empty.
    expect(highlighted.data[3]!).toBeLessThan(highlighted.data[7]!);
    expect(highlighted.filled).toBe(2);
  });


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
  it('answers the mark’s own position, not the pointer’s', () => {
    const positions = Float32Array.from([1, 2, 30, 40]);
    expect(resolvePick({index: 1, sourceLayer: {id: 'm', props: {tesseraIds: ids, tesseraPositions: positions}}, coordinate: [31, 41]})).toEqual({
      kind: 'mark',
      id: 12n,
      worldXY: [30, 40]
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

import {DENSITY_SUPERSAMPLE, filterDensity} from '../src/density.js';

describe('filterDensity — the tile grid is never shown (decision 0097)', () => {
  it('turns a single non-zero bin into a halo with no one-texel step from nothing to full', () => {
    const binned = binDensity([tile(4, 6, 3, true, 100)], 3)!;
    const soft = filterDensity(binned, 3);
    const S = DENSITY_SUPERSAMPLE;
    // One padding cell each side, S texels a cell.
    expect(soft.width).toBe(3 * S);
    expect(soft.height).toBe(3 * S);
    expect(soft.bounds).toEqual([256 - 64, 384 - 64, 320 + 64, 448 + 64]);
    const alpha = (x: number, y: number) => soft.data[(y * soft.width + x) * 4 + 3]!;
    let peak = 0;
    for (let y = 0; y < soft.height; y++) for (let x = 0; x < soft.width; x++) peak = Math.max(peak, alpha(x, y));
    expect(peak).toBeGreaterThan(0);
    // Every neighbouring pair of texels differs by well under the peak: the edge is a ramp.
    let worst = 0;
    for (let y = 0; y < soft.height; y++) {
      for (let x = 1; x < soft.width; x++) worst = Math.max(worst, Math.abs(alpha(x, y) - alpha(x - 1, y)));
    }
    for (let x = 0; x < soft.width; x++) {
      for (let y = 1; y < soft.height; y++) worst = Math.max(worst, Math.abs(alpha(x, y) - alpha(x, y - 1)));
    }
    expect(worst).toBeLessThanOrEqual(peak * 0.35);
    // The halo extends beyond the cell and fades to nothing at the padding's far edge.
    expect(alpha(S + S / 2, S + S / 2)).toBe(peak);
    expect(alpha(0, S + S / 2)).toBe(0);
    expect(alpha(S / 2, S + S / 2)).toBeGreaterThan(0);
    expect(alpha(S / 2, S + S / 2)).toBeLessThan(peak);
  });

  it('keeps an empty bin between two filled ones dimmer than either', () => {
    const binned = binDensity([tile(0, 0, 3, true, 100), tile(2, 0, 3, true, 100)], 3)!;
    const soft = filterDensity(binned, 3);
    const S = DENSITY_SUPERSAMPLE;
    const mid = S + S / 2;
    const alpha = (x: number) => soft.data[(mid * soft.width + x) * 4 + 3]!;
    expect(alpha(2 * S + S / 2)).toBeLessThan(alpha(S + S / 2));
    expect(alpha(2 * S + S / 2)).toBeLessThan(alpha(3 * S + S / 2));
  });
});
