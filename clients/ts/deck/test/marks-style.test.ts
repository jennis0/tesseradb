import {describe, expect, it} from 'vitest';
import {deckOpacity, markStyle} from '../src/marks-style.js';

/** The marks' size and alpha by count and zoom (§5.10; the boards' `datamap2`). */
describe('markStyle', () => {
  it('draws a million marks small and translucent, so density reads through them', () => {
    const s = markStyle(1_000_000, 0);
    expect(s.radius).toBe(1.2);
    expect(s.alpha).toBe(0.5);
  });

  it('draws the boards’ count near the boards’ 1.5 px at 0.7', () => {
    const s = markStyle(1_600, 0);
    expect(s.radius).toBeGreaterThan(1.5);
    expect(s.radius).toBeLessThan(2.2);
    expect(s.alpha).toBeGreaterThan(0.65);
    expect(s.alpha).toBeLessThan(0.78);
  });

  it('grows and solidifies as the count falls, and never past the caps', () => {
    let last = markStyle(2_000_000, 0);
    for (const marks of [1_000_000, 200_000, 50_000, 10_000, 1_000, 100, 10, 0]) {
      const s = markStyle(marks, 0);
      expect(s.radius).toBeGreaterThanOrEqual(last.radius);
      expect(s.alpha).toBeGreaterThanOrEqual(last.alpha);
      last = s;
    }
    expect(last.radius).toBe(2.2);
    expect(last.alpha).toBe(0.8);
    expect(markStyle(0, 10).alpha).toBe(0.95);
  });

  it('adds a little with zoom, and pins the radius where the host fixed one', () => {
    expect(markStyle(1_000_000, 8).radius).toBeCloseTo(1.2 + 0.64, 2);
    expect(markStyle(1_000_000, 8).alpha).toBeCloseTo(0.66, 2);
    expect(markStyle(1_000_000, -3).radius).toBe(1.2);
    const fixed = markStyle(1_000_000, 8, 1.6);
    expect(fixed.radius).toBe(1.6);
    expect(fixed.alpha).toBeCloseTo(0.66, 2);
  });

  it('deckOpacity undoes deck’s gamma so the shader composites at the alpha asked for', () => {
    expect(Math.pow(deckOpacity(0.5), 1 / 2.2)).toBeCloseTo(0.5, 6);
    expect(deckOpacity(1)).toBe(1);
    expect(deckOpacity(0)).toBe(0);
  });
});
