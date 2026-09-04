import {describe, expect, it} from 'vitest';
import {ANTIALIAS_ABOVE_PX, deckOpacity, markStyle} from '../src/marks-style.js';

/** The marks' size and alpha by count and zoom (§5.10; the boards' `datamap2`). */
describe('markStyle', () => {
  it('draws a million marks small and translucent, so a dense region reads as dense', () => {
    // Recalibrated on the owner's review of the 2.4M map: it was 1.2 px at 0.5, which bloomed.
    const s = markStyle(1_000_000, 0);
    expect(s.radius).toBe(1.1);
    expect(s.alpha).toBe(0.34);
  });

  it('drops the half-pixel feather where it would be most of the mark', () => {
    // deck feathers half a pixel either side of the radius, so at 1 px the mark is more ramp than
    // disc and thousands of them bloom. Above the threshold the feather is an edge and stays.
    expect(markStyle(1_000_000, 0).antialiasing).toBe(false);
    expect(markStyle(1_600, 0).antialiasing).toBe(true);
    expect(markStyle(1_000_000, 0).radius).toBeLessThan(ANTIALIAS_ABOVE_PX);
    expect(markStyle(1_600, 0).radius).toBeGreaterThanOrEqual(ANTIALIAS_ABOVE_PX);
  });

  it('keeps the boards’ own count at the boards’ 1.5 px and near their alpha', () => {
    const s = markStyle(1_600, 0);
    expect(s.radius).toBeCloseTo(1.5, 1);
    expect(s.alpha).toBeGreaterThan(0.6);
    expect(s.alpha).toBeLessThanOrEqual(0.78);
  });

  it('grows and solidifies as the count falls, and never past the caps', () => {
    let last = markStyle(2_000_000, 0);
    for (const marks of [1_000_000, 200_000, 50_000, 10_000, 1_000, 100, 10, 0]) {
      const s = markStyle(marks, 0);
      expect(s.radius).toBeGreaterThanOrEqual(last.radius);
      expect(s.alpha).toBeGreaterThanOrEqual(last.alpha);
      last = s;
    }
    expect(last.radius).toBe(1.7);
    expect(last.alpha).toBe(0.78);
    expect(markStyle(0, 10).alpha).toBe(0.9);
  });

  it('adds a little with zoom, and pins the radius where the host fixed one', () => {
    expect(markStyle(1_000_000, 8).radius).toBeCloseTo(1.1 + 0.4, 2);
    expect(markStyle(1_000_000, 8).alpha).toBeCloseTo(0.46, 2);
    expect(markStyle(1_000_000, -3).radius).toBe(1.1);
    const fixed = markStyle(1_000_000, 8, 1.6);
    expect(fixed.radius).toBe(1.6);
    expect(fixed.alpha).toBeCloseTo(0.46, 2);
    // A fixed radius still decides its own feather.
    expect(fixed.antialiasing).toBe(true);
    expect(markStyle(1_000_000, 0, 1).antialiasing).toBe(false);
  });

  it('is smaller and dimmer than the band it replaces, at every count a screen holds', () => {
    // The band before the review: `1.2 + 1.0(1−t) + 0.08z` px at `0.5 + 0.3(1−t) + 0.02z`.
    const before = (marks: number, zoom: number) => {
      const t = Math.min(1, Math.max(0, (Math.log10(Math.max(1, marks)) - 2) / 4));
      const z = Math.min(10, Math.max(0, zoom));
      return {radius: 1.2 + 1.0 * (1 - t) + 0.08 * z, alpha: Math.min(0.95, 0.5 + 0.3 * (1 - t) + 0.02 * z)};
    };
    for (const marks of [2_400_000, 1_000_000, 100_000, 1_600, 100]) {
      for (const zoom of [0, 1, 3, 6]) {
        expect(markStyle(marks, zoom).radius).toBeLessThan(before(marks, zoom).radius);
      }
    }
    // Alpha comes down everywhere the blooming was, and the sparse end lands on the boards'
    // own 0.78 rather than under it — the same figure the old band reached by a different route.
    for (const marks of [2_400_000, 1_000_000, 100_000, 1_600]) {
      for (const zoom of [0, 1, 3, 6]) {
        expect(markStyle(marks, zoom).alpha).toBeLessThan(before(marks, zoom).alpha);
      }
    }
    expect(markStyle(100, 0).alpha).toBeCloseTo(0.78, 6);
  });

  it('deckOpacity undoes deck’s gamma so the shader composites at the alpha asked for', () => {
    expect(Math.pow(deckOpacity(0.5), 1 / 2.2)).toBeCloseTo(0.5, 6);
    expect(deckOpacity(1)).toBe(1);
    expect(deckOpacity(0)).toBe(0);
  });
});
