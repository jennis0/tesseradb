import {describe, expect, it} from 'vitest';
import {
  coverageAdd,
  coverageAt,
  rectArea,
  rectContains,
  rectSubtract,
  rectSubtractAll,
  rectsIntersect,
  type Coverage,
  type TileRect
} from '../src/rects.js';

const R = (x0: number, y0: number, x1: number, y1: number): TileRect => ({x0, y0, x1, y1});

/** Every tile of a rect, as "x,y" — the ground truth subtraction is checked against. */
function tilesOf(r: TileRect): Set<string> {
  const s = new Set<string>();
  for (let y = r.y0; y <= r.y1; y++) for (let x = r.x0; x <= r.x1; x++) s.add(`${x},${y}`);
  return s;
}

function unionOf(rects: TileRect[]): string[] {
  const all: string[] = [];
  for (const r of rects) all.push(...tilesOf(r));
  return all;
}

describe('rectSubtract', () => {
  it('returns the whole rect when they do not touch', () => {
    expect(rectSubtract(R(0, 0, 4, 4), R(10, 10, 12, 12))).toEqual([R(0, 0, 4, 4)]);
  });

  it('returns nothing when the hole swallows it', () => {
    expect(rectSubtract(R(2, 2, 3, 3), R(0, 0, 9, 9))).toEqual([]);
  });

  it('is exact, and its pieces are disjoint, for every overlap of two small rects', () => {
    // Exhaustive over a 6x6 grid: the union of the pieces must equal want-minus-hole exactly, and
    // no tile may appear twice — a duplicated tile would be requested and absorbed twice.
    for (let ax0 = 0; ax0 < 4; ax0++)
      for (let ax1 = ax0; ax1 < 4; ax1++)
        for (let ay0 = 0; ay0 < 4; ay0++)
          for (let ay1 = ay0; ay1 < 4; ay1++)
            for (let bx0 = 0; bx0 < 4; bx0++)
              for (let bx1 = bx0; bx1 < 4; bx1++)
                for (let by0 = 0; by0 < 4; by0++)
                  for (let by1 = by0; by1 < 4; by1++) {
                    const want = R(ax0, ay0, ax1, ay1);
                    const hole = R(bx0, by0, bx1, by1);
                    const pieces = rectSubtract(want, hole);
                    const got = unionOf(pieces);
                    expect(new Set(got).size).toBe(got.length); // disjoint
                    const expected = [...tilesOf(want)].filter((t) => !tilesOf(hole).has(t));
                    expect(new Set(got)).toEqual(new Set(expected)); // exact
                    expect(pieces.length).toBeLessThanOrEqual(4);
                  }
  });

  it('produces the L-shape a pan actually generates', () => {
    // A viewport shifted right: what is novel is the strip on the right, and nothing else.
    const held = R(0, 0, 99, 99);
    const want = R(20, 0, 119, 99);
    const novel = rectSubtract(want, held);
    expect(novel).toHaveLength(1);
    expect(novel[0]).toEqual(R(100, 0, 119, 99));
    expect(rectArea(novel[0]!)).toBe(20 * 100);
  });
});

describe('rectSubtractAll', () => {
  it('removes every hole', () => {
    const pieces = rectSubtractAll(R(0, 0, 9, 9), [R(0, 0, 4, 9), R(5, 0, 9, 4)]);
    const got = unionOf(pieces);
    expect(new Set(got).size).toBe(got.length);
    expect(new Set(got)).toEqual(new Set(unionOf([R(5, 5, 9, 9)])));
  });

  it('gives up rather than fragmenting, and gives up towards MORE', () => {
    // Nine scattered holes would shatter the rect. The bound stops subtracting and returns a
    // superset: extra bytes, never a hole. Anything less than a superset would be a silent gap.
    const holes: TileRect[] = [];
    for (let i = 0; i < 9; i++) holes.push(R(i * 3 + 1, i * 3 + 1, i * 3 + 1, i * 3 + 1));
    const want = R(0, 0, 29, 29);
    const pieces = rectSubtractAll(want, holes, 4);
    const covered = new Set(unionOf(pieces));
    const trulyNovel = [...tilesOf(want)].filter((t) => !holes.some((h) => tilesOf(h).has(t)));
    for (const t of trulyNovel) expect(covered.has(t)).toBe(true);
  });

  it('returns nothing when the holes cover everything', () => {
    expect(rectSubtractAll(R(0, 0, 9, 9), [R(0, 0, 9, 4), R(0, 5, 9, 9)])).toEqual([]);
  });
});

describe('coverage', () => {
  const cov = (r: TileRect, depth = 5, contentKey = 'ck', capUsed = 500): Coverage =>
    ({rect: r, depth, contentKey, capUsed});

  it('drops a rect already covered, and replaces one it subsumes', () => {
    let list: Coverage[] = [];
    list = coverageAdd(list, cov(R(0, 0, 9, 9)));
    list = coverageAdd(list, cov(R(2, 2, 3, 3))); // inside — no growth
    expect(list).toHaveLength(1);
    list = coverageAdd(list, cov(R(0, 0, 19, 19))); // subsumes — replaces
    expect(list).toHaveLength(1);
    expect(list[0]!.rect).toEqual(R(0, 0, 19, 19));
  });

  it('keeps depth and content key apart', () => {
    let list: Coverage[] = [];
    list = coverageAdd(list, cov(R(0, 0, 9, 9), 5, 'ck1'));
    list = coverageAdd(list, cov(R(0, 0, 9, 9), 6, 'ck1'));
    list = coverageAdd(list, cov(R(0, 0, 9, 9), 5, 'ck2'));
    expect(list).toHaveLength(3);
    expect(coverageAt(list, 5, 'ck1', 500)).toEqual([R(0, 0, 9, 9)]);
    expect(coverageAt(list, 7, 'ck1', 500)).toEqual([]);
  });

  it('will not reuse coverage fetched at a smaller cap', () => {
    // Where the cap bound the selection, a larger k yields more points for the same tiles — so
    // coverage bought at k=100 cannot answer a k=500 request.
    const list = coverageAdd([], cov(R(0, 0, 9, 9), 5, 'ck', 100));
    expect(coverageAt(list, 5, 'ck', 100)).toHaveLength(1);
    expect(coverageAt(list, 5, 'ck', 500)).toHaveLength(0);
  });

  it('fuses a pan sequence into one rectangle', () => {
    // The property that keeps one pan to one request: each strip extends the covered rectangle
    // rather than lengthening a chain the next subtraction has to shatter against.
    let list: Coverage[] = [];
    for (let i = 0; i < 8; i++) list = coverageAdd(list, cov(R(i * 10, 0, i * 10 + 19, 99)));
    expect(list).toHaveLength(1);
    expect(list[0]!.rect).toEqual(R(0, 0, 89, 99));
  });

  it('fuses to a fixed point, so a strip bridging two rects merges all three', () => {
    let list: Coverage[] = [];
    list = coverageAdd(list, cov(R(0, 0, 9, 9)));
    list = coverageAdd(list, cov(R(20, 0, 29, 9)));
    expect(list).toHaveLength(2);
    list = coverageAdd(list, cov(R(10, 0, 19, 9))); // the bridge
    expect(list).toHaveLength(1);
    expect(list[0]!.rect).toEqual(R(0, 0, 29, 9));
  });

  it('never fuses two rects into ground neither covered', () => {
    // The one failure this structure must not have: claiming emptiness for tiles nobody asked
    // about. Two adjacent-but-not-nested rects stay two.
    // Offset rows: the union is L-shaped, so these must stay two.
    let list: Coverage[] = [];
    list = coverageAdd(list, cov(R(0, 0, 9, 9)));
    list = coverageAdd(list, cov(R(10, 5, 19, 14)));
    expect(list).toHaveLength(2);
    const holes = coverageAt(list, 5, 'ck', 500);
    // The row below the first was never covered, so it must still be novel.
    expect(rectSubtractAll(R(0, 10, 9, 10), holes)).toHaveLength(1);
  });
});

describe('rectsIntersect / rectContains', () => {
  it('agree with the tile sets they abbreviate', () => {
    const a = R(2, 2, 6, 6);
    for (let x0 = 0; x0 < 9; x0++)
      for (let y0 = 0; y0 < 9; y0++) {
        const b = R(x0, y0, x0 + 1, y0 + 1);
        const ta = tilesOf(a);
        const tb = tilesOf(b);
        const overlap = [...tb].some((t) => ta.has(t));
        expect(rectsIntersect(a, b)).toBe(overlap);
        expect(rectContains(a, b)).toBe([...tb].every((t) => ta.has(t)));
      }
  });
});
