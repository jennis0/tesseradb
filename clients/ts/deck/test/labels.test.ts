import {describe, expect, it} from 'vitest';
import {LABEL_SIZE_MAX, LABEL_SIZE_MIN, MAX_DISPLACEMENT, MAX_LABEL_LINE_CHARS, labelSize, placeLabels, wrapLabel, type LabelCandidate} from '../src/labels.js';

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

  it('sizes a name by its masked count, on a logarithmic band over the range drawn', () => {
    // The ends are the range's ends: the largest count on screen is the largest name on screen.
    expect(labelSize(380_069, 176, 380_069)).toBeCloseTo(LABEL_SIZE_MAX, 6);
    expect(labelSize(176, 176, 380_069)).toBeCloseTo(LABEL_SIZE_MIN, 6);
    // Monotone in the count, and never outside the band.
    let previous = 0;
    for (const count of [176, 1_000, 29_369, 87_295, 380_069]) {
      const size = labelSize(count, 176, 380_069);
      expect(size).toBeGreaterThan(previous);
      expect(size).toBeGreaterThanOrEqual(LABEL_SIZE_MIN);
      expect(size).toBeLessThanOrEqual(LABEL_SIZE_MAX);
      previous = size;
    }
    // **Logarithmic, not linear**: the owner's pair. 29,369 against 380,069 is 7.7% of the range
    // linearly — a name on the floor — and half of it on the band, which is what the eye reads.
    const midway = (labelSize(29_369, 176, 380_069) - LABEL_SIZE_MIN) / (LABEL_SIZE_MAX - LABEL_SIZE_MIN);
    expect(midway).toBeGreaterThan(0.55);
    expect(midway).toBeLessThan(0.75);
    // A count ten times another is a fixed step whatever the decade — the point of the band.
    const step = (n: number) => labelSize(n * 10, 1, 1e6) - labelSize(n, 1, 1e6);
    expect(step(10)).toBeCloseTo(step(10_000), 6);
    // Out-of-range and absurd inputs land inside the band rather than off it: a count of zero is
    // a count of one, and one outside the range extends it rather than escaping the ends.
    expect(labelSize(0, 176, 380_069)).toBe(LABEL_SIZE_MIN);
    expect(labelSize(1e9, 176, 380_069)).toBe(LABEL_SIZE_MAX);
    expect(labelSize(-5, 176, 380_069)).toBe(LABEL_SIZE_MIN);
  });

  it('a frontier with no range — one name, or every count equal — takes the top of the band', () => {
    // Nothing to divide by, and no division done: the largest count on screen draws largest, and
    // where every count is the largest that holds of all of them.
    expect(labelSize(4_812, 4_812, 4_812)).toBe(LABEL_SIZE_MAX);
    expect(labelSize(1, 1, 1)).toBe(LABEL_SIZE_MAX);
    expect(labelSize(0, 0, 0)).toBe(LABEL_SIZE_MAX);
    // And an empty frontier's sentinel range (no candidate at all) does not produce a NaN.
    expect(Number.isFinite(labelSize(500, Number.POSITIVE_INFINITY, 0))).toBe(true);
  });
});

describe('wrapping a name (§5.10)', () => {
  it('breaks on words, keeps every line short, and never hyphenates', () => {
    expect(wrapLabel('quantum error correction')).toEqual(['quantum error', 'correction']);
    expect(wrapLabel('graph')).toEqual(['graph']);
    // A word longer than the line takes a line of its own rather than being cut.
    expect(wrapLabel('electroencephalography signals')).toEqual(['electroencephalography', 'signals']);
    for (const line of wrapLabel('dark matter haloes in cosmological simulations')) {
      expect(line.length).toBeLessThanOrEqual(MAX_LABEL_LINE_CHARS + 2);
    }
  });

  it('elides rather than silently truncating what will not fit in three lines', () => {
    const lines = wrapLabel('one two three four five six seven eight nine ten eleven twelve');
    expect(lines.length).toBe(3);
    expect(lines[2]!.endsWith('…')).toBe(true);
  });

  it('places a wrapped label by the box it actually draws', () => {
    // Two labels a line apart on the same anchor: the taller wrapped box refuses the overlap the
    // one-line box would have allowed.
    const box = (id: bigint, y: number, height: number): LabelCandidate => ({id, x: 0, y, width: 100, height, priority: Number(id)});
    const tall = placeLabels([box(2n, 0, 40), box(1n, 30, 40)]);
    expect(tall.length).toBe(1);
    const short = placeLabels([box(2n, 0, 12), box(1n, 30, 12)]);
    expect(short.length).toBe(2);
  });
});
