import {describe, expect, it} from 'vitest';
import {
  countCodes,
  extendRanks,
  numericValues,
  rankedValues,
  widenDomain,
  widenDomainOver
} from '../src/encoding.js';
import type {CategoryValue, ScalarColumn} from '../src/types.js';

const u32 = (values: number[]): ScalarColumn => ({arrowType: 'u32', values: Uint32Array.from(values)});

describe('the encoding accumulators', () => {
  it('widens a numeric domain and never narrows it', () => {
    const first = widenDomain(null, u32([3, 5, 9]));
    expect(first).toEqual({min: 3, max: 9});
    // A later, narrower view must not shrink the ramp under the reader.
    expect(widenDomain(first, u32([6, 7]))).toEqual({min: 3, max: 9});
    // A wider one extends it.
    expect(widenDomain(first, u32([1, 12]))).toEqual({min: 1, max: 12});
  });

  it('widens over a prefix or an index list without materialising the column', () => {
    const col = u32([100, 2, 3, 4, 5]);
    // A prefix of one takes only the first value.
    expect(widenDomainOver(null, col, null, 1)).toEqual({min: 100, max: 100});
    // An index list takes exactly those.
    expect(widenDomainOver(null, col, [1, 2], Infinity)).toEqual({min: 2, max: 3});
  });

  it('has no ramp for a bool or utf8 column', () => {
    expect(numericValues({arrowType: 'bool', values: [true, false]})).toBeNull();
    expect(widenDomain(null, {arrowType: 'utf8', values: ['a']})).toBeNull();
  });

  it('counts codes on screen, skipping the absent sentinel', () => {
    const counts = countCodes(u32([5, 5, 0, 7]));
    expect(counts.get(5)).toBe(2);
    expect(counts.get(7)).toBe(1);
    expect(counts.has(0)).toBe(false); // 0 is *absent*, not a value
  });

  it('ranks by frequency, most frequent first, and never reorders what it assigned', () => {
    const ranks = extendRanks({}, new Map([[5, 2], [7, 10], [9, 1]]));
    expect(ranks).toEqual({7: 0, 5: 1, 9: 2});
    // A later count that makes 9 commonest must not move 7 or 5 — the palette is sticky.
    const next = extendRanks(ranks, new Map([[9, 100], [11, 3]]));
    expect(next[7]).toBe(0);
    expect(next[5]).toBe(1);
    expect(next[9]).toBe(2);
    expect(next[11]).toBe(3);
  });

  it('lists only the values that hold one of the palette colours, in rank order', () => {
    const values: CategoryValue[] = [
      {code: 5, key: 'a', title: null},
      {code: 7, key: 'b', title: null},
      {code: 9, key: 'c', title: null}
    ];
    const ranks = {7: 0, 5: 1, 9: 2};
    const listed = rankedValues(values, ranks, 2);
    expect(listed.map((v) => v.value.key)).toEqual(['b', 'a']); // ranks 0 and 1, in order
  });
});
