import {describe, expect, it} from 'vitest';
import {MAX_DISPLACEMENT, labelSize, placeLabels, type LabelCandidate} from '../src/labels.js';

const at = (id: number, x: number, y: number, priority: number, width = 60, height = 24): LabelCandidate => ({id: BigInt(id), x, y, width, height, priority});

function rects(candidates: LabelCandidate[]) {
  const placed = placeLabels(candidates);
  return placed.map((p) => {
    const c = candidates.find((k) => k.id === p.id)!;
    return {...p, x0: c.x + p.dx - c.width / 2, y0: c.y + p.dy - c.height / 2, x1: c.x + p.dx + c.width / 2, y1: c.y + p.dy + c.height / 2};
  });
}

describe('label placement (§5.10)', () => {
  it('keeps the higher priority at its centroid and moves the lower with a leader; nothing overlaps', () => {
    const placed = rects([at(1, 100, 100, 5), at(2, 110, 104, 900)]);
    const big = placed.find((p) => p.id === 2n)!;
    const small = placed.find((p) => p.id === 1n)!;
    expect(big.dx).toBe(0);
    expect(big.dy).toBe(0);
    expect(big.leader).toBe(false);
    expect(small.leader).toBe(true);
    expect(small.x0 < big.x1 && big.x0 < small.x1 && small.y0 < big.y1 && big.y0 < small.y1).toBe(false);
  });

  it('places no two labels over each other, and leaves out what fits nowhere', () => {
    // Twenty labels on one spot: within 40 px of the spot only the centroid and the two vertical
    // nudges are free, so three are placed and the rest wait for zoom.
    const many = Array.from({length: 20}, (_, i) => at(i + 1, 300, 300, 20 - i));
    const placed = rects(many);
    expect(placed.length).toBe(3);
    for (let i = 0; i < placed.length; i++) {
      for (let j = i + 1; j < placed.length; j++) {
        const a = placed[i]!;
        const b = placed[j]!;
        expect(a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1, `${a.id} over ${b.id}`).toBe(false);
      }
    }
    // The first to be left out is a low priority, never the highest.
    expect(placed.some((p) => p.id === 1n)).toBe(true);
  });

  it('is translation-invariant — a pan moves every label and changes no placement', () => {
    const base = [at(1, 100, 100, 5), at(2, 110, 104, 900), at(3, 400, 50, 50), at(4, 405, 60, 40)];
    const shifted = base.map((c) => ({...c, x: c.x + 1234.5, y: c.y - 777}));
    expect(placeLabels(shifted)).toEqual(placeLabels(base));
  });

  it('never leads a label across the map: a move beyond 40 px drops the label instead', () => {
    // Two wide labels on one spot: the lower would have to move its own width (206 px) sideways
    // or its height (30 px) up or down. Up and down are taken by two more; nothing else is within
    // reach, so it is left out rather than drawn 206 px away on a leader.
    const wide = (id: number, priority: number) => at(id, 500, 500, priority, 200, 24);
    const placed = placeLabels([wide(1, 100), wide(2, 90), wide(3, 80), wide(4, 70)]);
    expect(placed.map((p) => p.id)).toEqual([1n, 2n, 3n]);
    for (const p of placed) expect(Math.hypot(p.dx, p.dy)).toBeLessThanOrEqual(MAX_DISPLACEMENT);
    // The moved ones carry a leader; the one at its centroid does not.
    expect(placed.find((p) => p.id === 1n)!.leader).toBe(false);
    expect(placed.filter((p) => p.leader).map((p) => p.id)).toEqual([2n, 3n]);
    // A wider bound admits the sideways try — the rule is the bound, not the ring.
    const loose = placeLabels([wide(1, 100), wide(2, 90), wide(3, 80), wide(4, 70)], 300);
    expect(loose.length).toBe(4);
    expect(Math.abs(loose.find((p) => p.id === 4n)!.dx)).toBe(206);
    expect(MAX_DISPLACEMENT).toBe(40);
  });

  it('sizes a name by masked count within a narrow band', () => {
    expect(labelSize(0, 1000)).toBe(10);
    expect(labelSize(1000, 1000)).toBe(15);
    expect(labelSize(250, 1000)).toBeCloseTo(12.5);
  });
});
