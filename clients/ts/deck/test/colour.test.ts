import {describe, expect, it} from 'vitest';
import type {ScalarColumn} from '@tesseradb/client';
import {CATEGORY_PALETTES, RAMPS, UNMAPPED, colourOfFraction, formatScalar, fractionOf, hexOf, rgbOfHex, writeColours, type CategoryPaletteName, type Encoding, type RampName, type Rgb} from '../src/colour.js';

/** The colour `writeColours` wrote for each mark. */
function colours(column: ScalarColumn, domain: {min: number; max: number}): number[][] {
  const count = column.values.length;
  const out = new Uint8Array(count * 4);
  writeColours(out, 0, count, column, {kind: 'numeric', column: 'heat', domain, ramp: 'viridis', scale: 'linear', reverse: false});
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

describe('named palettes and ramps', () => {
  const codes: ScalarColumn = {arrowType: 'u16', values: Uint16Array.from([1, 2, 3])};
  const drawn = (encoding: Encoding) => {
    const out = new Uint8Array(3 * 4);
    writeColours(out, 0, 3, encoding.kind === 'numeric' ? {arrowType: 'f64', values: Float64Array.from([0, 10, 1000])} : codes, encoding);
    return [0, 1, 2].map((i) => [...out.subarray(i * 4, i * 4 + 3)]);
  };
  const category = (palette: CategoryPaletteName, chosen = new Map<number, Rgb>()): Encoding => ({kind: 'category', column: 'c', rankOfCode: {1: 0, 2: 1, 3: 2}, palette, chosen});

  it('colours category values by rank from the palette chosen', () => {
    for (const name of Object.keys(CATEGORY_PALETTES) as CategoryPaletteName[]) {
      expect(drawn(category(name))).toEqual(CATEGORY_PALETTES[name].colours.slice(0, 3).map((c) => [...c]));
    }
  });

  it('draws a value in the colour chosen for it, and the others from the palette', () => {
    const colours = drawn(category('okabe-ito', new Map([[2, [1, 2, 3]]])));
    expect(colours[1]).toEqual([1, 2, 3]);
    expect(colours[0]).toEqual([...CATEGORY_PALETTES['okabe-ito'].colours[0]!]);
  });

  it('places numbers on the ramp chosen, reversed, and on a log scale', () => {
    const numeric = (over: Partial<Extract<Encoding, {kind: 'numeric'}>>): Encoding => ({kind: 'numeric', column: 'n', domain: {min: 0, max: 1000}, ramp: 'viridis', scale: 'linear', reverse: false, ...over});
    const ends = (ramp: RampName) => [RAMPS[ramp].stops[0]!, RAMPS[ramp].stops.at(-1)!].map((c) => [...c]);
    expect([drawn(numeric({ramp: 'magma'}))[0], drawn(numeric({ramp: 'magma'}))[2]]).toEqual(ends('magma'));
    const reversed = drawn(numeric({ramp: 'magma', reverse: true}));
    expect([reversed[2], reversed[0]]).toEqual(ends('magma'));
    // 10 of 1,000 is 1% of the way on a linear scale and about a third on a log one.
    expect(fractionOf(10, {min: 0, max: 1000}, 'linear', false)).toBeCloseTo(0.01);
    expect(fractionOf(10, {min: 0, max: 1000}, 'log', false)).toBeCloseTo(Math.log1p(10) / Math.log1p(1000));
  });

  it('centres a diverging ramp on zero where the range spans it', () => {
    expect(fractionOf(0, {min: -10, max: 30}, 'linear', true)).toBe(0.5);
    expect(fractionOf(30, {min: -10, max: 30}, 'linear', true)).toBe(1);
    expect(fractionOf(-10, {min: -10, max: 30}, 'linear', true)).toBeCloseTo(1 / 3);
    expect(fractionOf(15, {min: 10, max: 20}, 'linear', true)).toBe(0.5);
  });

  it('reads and writes #rrggbb', () => {
    expect(rgbOfHex('#F28E2B')).toEqual([242, 142, 43]);
    expect(rgbOfHex('f28e2b')).toBeNull();
    expect(hexOf([242, 142, 43])).toBe('#f28e2b');
  });
});
