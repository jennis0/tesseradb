import {describe, expect, it} from 'vitest';
import {ANTIALIAS_ABOVE_PX, deckOpacity, markStyle} from '../src/marks-style.js';

/** The marks' size and alpha by count and zoom. */
describe('markStyle', () => {
  it('draws a million marks small and translucent, so a dense region reads as dense', () => {
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

  it('draws up to ten thousand marks at 2 px and 0.8 alpha, and shrinks them only past that', () => {
    for (const marks of [100, 1_600, 10_000]) expect(markStyle(marks, 0)).toMatchObject({radius: 2, alpha: 0.8});
    const dense = markStyle(100_000, 0);
    expect(dense.radius).toBeLessThan(2);
    expect(dense.alpha).toBeLessThan(0.8);
  });

  it('grows and solidifies as the count falls, and never past the caps', () => {
    let last = markStyle(2_000_000, 0);
    for (const marks of [1_000_000, 200_000, 50_000, 10_000, 1_000, 100, 10, 0]) {
      const s = markStyle(marks, 0);
      expect(s.radius).toBeGreaterThanOrEqual(last.radius);
      expect(s.alpha).toBeGreaterThanOrEqual(last.alpha);
      last = s;
    }
    expect(last.radius).toBe(2);
    expect(last.alpha).toBe(0.8);
    expect(markStyle(0, 10).alpha).toBe(0.9);
  });

  it('adds a little with zoom, and pins the radius where the host fixed one', () => {
    expect(markStyle(1_000_000, 8).radius).toBeCloseTo(1.1 + 0.4, 2);
    expect(markStyle(1_000_000, 8).alpha).toBeCloseTo(0.46, 2);
    expect(markStyle(1_000_000, -3).radius).toBe(1.1);
    // The zoom term is the same at a sparse count as at a dense one.
    const sparse = markStyle(1_600, 0);
    expect(markStyle(1_600, 6).radius).toBeCloseTo(sparse.radius + 0.3, 1);
    expect(markStyle(1_600, 6).alpha).toBeCloseTo(Math.min(0.9, sparse.alpha + 0.09), 2);
    const fixed = markStyle(1_000_000, 8, 1.6);
    expect(fixed.radius).toBe(1.6);
    expect(fixed.alpha).toBeCloseTo(0.46, 2);
    // A fixed radius still decides its own feather.
    expect(fixed.antialiasing).toBe(true);
    expect(markStyle(1_000_000, 0, 1).antialiasing).toBe(false);
  });

  it('deckOpacity undoes deck’s gamma so the shader composites at the alpha asked for', () => {
    expect(Math.pow(deckOpacity(0.5), 1 / 2.2)).toBeCloseTo(0.5, 6);
    expect(deckOpacity(1)).toBe(1);
    expect(deckOpacity(0)).toBe(0);
  });
});
