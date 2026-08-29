import {describe, expect, it} from 'vitest';
import {WORLD_SIZE} from '../src/coords.js';
import {insideBox, insidePolygon, parseRegionVerdict, quantise, regionOperand, withRegion} from '../src/region.js';

/**
 * The client's half of the region contract (`selection-operand.md` §8): the leaf's spelling, the
 * verdict read off the wire, and the highlight's predicate — the server's own even-odd rule over
 * the quantised grid, with a point on an edge inside and the half-open ray for a tie.
 */

/** A world coordinate exactly on grid unit `n`. */
const at = (n: number) => (n / 4294967296) * WORLD_SIZE;

describe('quantise — the 32-bit grid the server tests on', () => {
  it('is fixed32 restated over the world: floor, clamped to the grid', () => {
    expect(quantise(0)).toBe(0);
    expect(quantise(-1)).toBe(0);
    expect(quantise(WORLD_SIZE)).toBe(4294967295);
    expect(quantise(WORLD_SIZE / 2)).toBe(2147483648);
    expect(quantise(at(1000))).toBe(1000);
  });
});

describe('insidePolygon — even-odd on the grid, an edge inside, the half-open ray', () => {
  const square = [
    [at(1000), at(1000)],
    [at(3000), at(1000)],
    [at(3000), at(3000)],
    [at(1000), at(3000)]
  ] as const;

  it('answers the interior and the exterior', () => {
    expect(insidePolygon(at(2000), at(2000), square)).toBe(true);
    expect(insidePolygon(at(500), at(2000), square)).toBe(false);
    expect(insidePolygon(at(2000), at(3500), square)).toBe(false);
  });

  it('counts a point on an edge, and on a vertex, as inside', () => {
    expect(insidePolygon(at(1000), at(2000), square)).toBe(true);
    expect(insidePolygon(at(2000), at(3000), square)).toBe(true);
    expect(insidePolygon(at(3000), at(3000), square)).toBe(true);
    expect(insidePolygon(at(1000), at(1000), square)).toBe(true);
  });

  it('is exact on the grid: one unit outside an edge is outside', () => {
    expect(insidePolygon(at(999), at(2000), square)).toBe(false);
    expect(insidePolygon(at(3001), at(2000), square)).toBe(false);
  });

  it('answers a self-crossing lasso by parity', () => {
    // A bow tie: the two lobes are inside, the crossing's pinch is an edge.
    const bowtie = [
      [at(0), at(0)],
      [at(4000), at(4000)],
      [at(4000), at(0)],
      [at(0), at(4000)]
    ] as const;
    expect(insidePolygon(at(1000), at(2000), bowtie)).toBe(true);
    expect(insidePolygon(at(3000), at(2000), bowtie)).toBe(true);
    expect(insidePolygon(at(2000), at(1000), bowtie)).toBe(false);
  });

  it('handles a ray through a vertex once, not twice', () => {
    // A diamond: a horizontal ray from its centre-left passes through the left vertex's level
    // only at the vertex itself; the half-open rule counts exactly one of the two edges there.
    const diamond = [
      [at(2000), at(1000)],
      [at(3000), at(2000)],
      [at(2000), at(3000)],
      [at(1000), at(2000)]
    ] as const;
    expect(insidePolygon(at(2000), at(2000), diamond)).toBe(true);
    expect(insidePolygon(at(500), at(2000), diamond)).toBe(false);
    expect(insidePolygon(at(3500), at(2000), diamond)).toBe(false);
  });
});

describe('insideBox — closed on every side, on the grid', () => {
  it('includes its edges and excludes one unit past them', () => {
    const box: [number, number, number, number] = [at(10), at(20), at(30), at(40)];
    expect(insideBox(at(10), at(20), box)).toBe(true);
    expect(insideBox(at(30), at(40), box)).toBe(true);
    expect(insideBox(at(31), at(30), box)).toBe(false);
    expect(insideBox(at(20), at(19), box)).toBe(false);
  });
});

describe('regionOperand — the leaf as the wire takes it', () => {
  it('normalises a box drawn from either corner', () => {
    expect(regionOperand({kind: 'box', bbox: [5, 6, 1, 2]})).toEqual({bbox: [1, 2, 5, 6]});
  });
  it('sends a lasso as drawn and an artifact as its id string', () => {
    expect(regionOperand({kind: 'lasso', points: [[0, 0], [1, 0], [0, 1]]})).toEqual({polygon: [[0, 0], [1, 0], [0, 1]]});
    expect(regionOperand({kind: 'artifact', id: 12345678901234567890n})).toEqual({artifact: '12345678901234567890'});
  });
});

describe('withRegion — the leaf composed with the other filters', () => {
  const leaf = {bbox: [0, 0, 1, 1] as [number, number, number, number]};
  it('is the leaf alone, the expression alone, or all_of the two', () => {
    expect(withRegion(null, null)).toBeNull();
    expect(withRegion(null, leaf)).toEqual({region: leaf});
    expect(withRegion({a: {eq: 1}}, null)).toEqual({a: {eq: 1}});
    expect(withRegion({a: {eq: 1}}, leaf)).toEqual({all_of: [{a: {eq: 1}}, {region: leaf}]});
  });
  it('spells outside as none_of over the leaf', () => {
    expect(withRegion(null, leaf, true)).toEqual({none_of: [{region: leaf}]});
    expect(withRegion({a: {eq: 1}}, leaf, true)).toEqual({all_of: [{a: {eq: 1}}, {none_of: [{region: leaf}]}]});
  });
});

describe('parseRegionVerdict — x-tessera-region', () => {
  it('reads exact, a cover at a depth, and nothing', () => {
    expect(parseRegionVerdict('exact')).toEqual({exact: true, depth: null});
    expect(parseRegionVerdict('cover; depth=11')).toEqual({exact: false, depth: 11});
    expect(parseRegionVerdict(null)).toBeNull();
    expect(parseRegionVerdict('something else')).toBeNull();
  });
});
