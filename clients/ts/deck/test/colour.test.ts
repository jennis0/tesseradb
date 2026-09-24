import {describe, expect, it} from 'vitest';
import type {ScalarColumn} from '@tesseradb/client';
import {UNMAPPED, colourOfFraction, formatScalar, writeColours} from '../src/colour.js';

/** The colour `writeColours` wrote for each mark. */
function colours(column: ScalarColumn, domain: {min: number; max: number}): number[][] {
  const count = column.values.length;
  const out = new Uint8Array(count * 4);
  writeColours(out, 0, count, column, {kind: 'numeric', column: 'heat', domain});
  return Array.from({length: count}, (_, i) => [...out.subarray(i * 4, i * 4 + 4)]);
}

describe('a number with no value', () => {
  // The middle mark has no value; the first is a genuine zero.
  const heat: ScalarColumn = {
    arrowType: 'u32',
    values: Uint32Array.from([0, 0, 10]),
    present: Uint8Array.from([1, 0, 1])
  };

  it('is drawn in the unmapped colour a category with no value takes, and a zero on the ramp', () => {
    const drawn = colours(heat, {min: 0, max: 10});
    expect(drawn[1]).toEqual([...UNMAPPED]);
    expect(drawn[0]).toEqual([...colourOfFraction(0)]);
    expect(drawn[2]).toEqual([...colourOfFraction(1)]);
  });

  it('reads as absent, as a category code 0 does, and a zero reads as 0', () => {
    expect(formatScalar(heat, 1, null)).toBe('absent');
    expect(formatScalar(heat, 0, null)).toBe('0');
    const band: ScalarColumn = {arrowType: 'u8', values: Uint8Array.from([0, 2])};
    expect(formatScalar(band, 0, new Map([[2, 'two']]))).toBe('absent');
  });

  it('is absent for a timestamp too, never the epoch', () => {
    const seen: ScalarColumn = {
      arrowType: 'timestamp_us',
      values: BigInt64Array.from([0n, 0n]),
      present: Uint8Array.from([0, 1])
    };
    expect(formatScalar(seen, 0, null)).toBe('absent');
    expect(formatScalar(seen, 1, null)).toBe('1970-01-01T00:00:00.000Z');
  });
});
