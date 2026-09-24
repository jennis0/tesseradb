import {describe, expect, it} from 'vitest';
import {MIN_DEPTH, calibrate, chooseDepth, countedMarks, tilesInBbox, type CountField} from '../src/budget.js';
import {MAX_DEPTH, WORLD_SIZE} from '../src/coords.js';

const full: [number, number, number, number] = [0, 0, WORLD_SIZE, WORLD_SIZE];
const base = {budget: 50_000, mTarget: 16, maxTiles: 262_144};

describe('tilesInBbox', () => {
  it('counts exactly at every depth', () => {
    expect(tilesInBbox(full, 0)).toBe(1);
    expect(tilesInBbox(full, 1)).toBe(4);
    expect(tilesInBbox(full, 6)).toBe(4096);
    // Strictly inside one tile at depth 1.
    expect(tilesInBbox([1, 1, WORLD_SIZE / 2 - 1, WORLD_SIZE / 2 - 1], 1)).toBe(1);
  });

  it('counts a boundary-touching bbox the way the server does', () => {
    // The server quantises both corners and includes both, so a viewport edge exactly on a tile
    // boundary touches both tiles, and this predicts the server's count.
    expect(tilesInBbox([0, 0, WORLD_SIZE / 2, WORLD_SIZE / 2], 1)).toBe(4);
  });
});

describe('chooseDepth', () => {
  it('asks for roughly budget / mTarget tiles however much is in view', () => {
    const whole = chooseDepth({...base, worldBbox: full});
    const quarter = chooseDepth({...base, worldBbox: [0, 0, WORLD_SIZE / 2, WORLD_SIZE / 2]});
    const sixteenth = chooseDepth({...base, worldBbox: [0, 0, WORLD_SIZE / 4, WORLD_SIZE / 4]});
    for (const r of [whole, quarter, sixteenth]) {
      expect(r.tiles).toBeGreaterThan(base.budget / base.mTarget / 4);
      expect(r.tiles).toBeLessThanOrEqual((base.budget / base.mTarget) * 4);
    }
    // Deeper as less is in view.
    expect(quarter.depth).toBeGreaterThan(whole.depth);
    expect(sixteenth.depth).toBeGreaterThan(quarter.depth);
  });

  it('never returns a depth outside the grid', () => {
    const deep = chooseDepth({...base, budget: 10 ** 12, worldBbox: full});
    expect(deep.depth).toBeLessThanOrEqual(MAX_DEPTH);
  });

  it('never returns a depth below the floor', () => {
    const tiny = chooseDepth({...base, budget: 1, worldBbox: full});
    expect(tiny.depth).toBe(MIN_DEPTH);
  });

  it('reports when the tile guard capped it rather than the budget', () => {
    const capped = chooseDepth({...base, budget: 10 ** 9, maxTiles: 64, worldBbox: full});
    expect(capped.tiles).toBeLessThanOrEqual(64);
    expect(capped.limitedBy).toBe('maxTiles');
  });

  it('stops once the principal’s visible set is exhausted', () => {
    // A sparse principal with 1,366 visible draws all of them from depth 4, so deeper is pure cost.
    const r = chooseDepth({...base, budget: 10 ** 9, worldBbox: full, visibleInView: 1366});
    expect(r.limitedBy).toBe('saturated');
    expect(r.depth).toBeLessThan(MAX_DEPTH);
    expect(r.predictedMarks).toBeLessThanOrEqual(1366);
  });

  it('never predicts more marks than are visible', () => {
    const r = chooseDepth({...base, worldBbox: full, visibleInView: 500});
    expect(r.predictedMarks).toBeLessThanOrEqual(500);
  });
});

/**
 * A gazetteer's shape: a solid block of saturated ground inside an otherwise empty view, at the
 * scale where the average model fails. `MEMBERS` members are spread evenly over the block, so the
 * marks a request costs at any depth are computed independently of the code under test.
 */
const LAND = 32; // the block's side, in tiles at FIELD_DEPTH
const VIEW = 64; // the view's side, in tiles at FIELD_DEPTH
const FIELD_DEPTH = 8;
const MEMBERS = 10 ** 8;
const K = 500;
const BUDGET = 500_000;
/** Sixty-four tiles a side at depth 8, stopping short of the boundary tile; see `tilesInBbox`. */
const view: [number, number, number, number] = [0, 0, 127.9, 127.9];

/** The marks a request at `depth` really costs: `Σ min(k, count)` over the block's own cells. */
function truth(depth: number): number {
  const cells = LAND ** 2 * 4 ** (depth - FIELD_DEPTH);
  return cells * Math.min(K, MEMBERS / cells);
}

/** The counts a response at `FIELD_DEPTH` over the whole view would have left. */
function field(): CountField {
  const cells = [];
  const count = MEMBERS / LAND ** 2;
  for (let x = 0; x < LAND; x++) for (let y = 0; y < LAND; y++) cells.push({x, y, count});
  return {depth: FIELD_DEPTH, cells, covers: {x0: 0, y0: 0, x1: VIEW - 1, y1: VIEW - 1}};
}

describe('chooseDepth on a bimodal field', () => {
  // `mTarget` at its 4x clamp, where the correction has nothing left to give.
  const bimodal = {budget: BUDGET, mTarget: 40, maxTiles: 262_144, worldBbox: view, k: K};

  it('the average model overshoots 4x where the count model fits', () => {
    const average = chooseDepth(bimodal);
    expect(average.source).toBe('average');
    // It predicts a little over the budget and is answered with four times it.
    expect(average.predictedMarks).toBeLessThan(BUDGET * 1.5);
    expect(truth(average.depth)).toBeGreaterThan(BUDGET * 4);

    const counted = chooseDepth({...bimodal, counts: field()});
    expect(counted.source).toBe('bound');
    expect(counted.depth).toBeLessThan(average.depth);
    expect(counted.predictedMarks).toBeLessThanOrEqual(BUDGET);
    // What the server would serve at the chosen depth, not what was predicted for it.
    expect(truth(counted.depth)).toBeLessThanOrEqual(BUDGET);
  });

  it('takes the deepest depth that fits, not the first that stops missing', () => {
    const counted = chooseDepth({...bimodal, counts: field()});
    expect(truth(counted.depth + 1)).toBeGreaterThan(BUDGET);
    expect(counted.limitedBy).toBe('budget');
  });

  it('stops where a step deeper buys tiles and not marks — a few capped cells do not pull the depth down', () => {
    // A field at depth 8 whose 64 x 64 cells hold 6 members each, except one city cell of 3,000.
    // The deepest fitting depth is 12, the first with nothing capped; depth 8 is within 15% of its
    // marks at 4^4 fewer tiles.
    const cells = [];
    for (let x = 0; x < 64; x++) for (let y = 0; y < 64; y++) cells.push({x, y, count: x === 10 && y === 10 ? 3_000 : 6});
    const counts: CountField = {depth: 8, cells, covers: {x0: 0, y0: 0, x1: 63, y1: 63}};
    const choice = chooseDepth({budget: 500_000, mTarget: 40, maxTiles: 262_144, worldBbox: view, k: 500, counts});
    // Depth 8, the field's own, serves all but the city's members, so it is taken. A shallower
    // depth is not, because a fold counts an ancestor's marks outside the view.
    expect(choice.source).toBe('counts');
    expect(choice.depth).toBe(8);
    expect(choice.limitedBy).toBe('saturated');
  });

  it('falls back to the average where the counts do not cover the view', () => {
    // The same field shifted off the view says nothing about the ground asked about.
    const elsewhere = {...field(), covers: {x0: 1_000, y0: 1_000, x1: 1_064, y1: 1_064}};
    expect(chooseDepth({...bimodal, counts: elsewhere}).source).toBe('average');
    expect(chooseDepth({...bimodal, counts: field(), k: undefined}).source).toBe('average');
    expect(chooseDepth(bimodal).source).toBe('average');
    // The fallback is the average model.
    expect(chooseDepth({...bimodal, counts: elsewhere})).toEqual(chooseDepth(bimodal));
  });

  it('stops where nothing is capped rather than ratcheting to the tile guard', () => {
    // Every cell under the cap: the whole visible set is served here, so a deeper request pays four
    // times the tiles for the same marks.
    const sparse: CountField = {
      depth: FIELD_DEPTH,
      cells: [{x: 0, y: 0, count: 12}, {x: 5, y: 9, count: 400}],
      covers: {x0: 0, y0: 0, x1: VIEW - 1, y1: VIEW - 1}
    };
    const counted = chooseDepth({...bimodal, counts: sparse});
    expect(counted.limitedBy).toBe('saturated');
    expect(counted.predictedMarks).toBe(412);
    expect(counted.depth).toBeLessThan(FIELD_DEPTH);
  });

  it('keeps the average model’s figure beside the count-driven one, for the calibration', () => {
    const counted = chooseDepth({...bimodal, counts: field()});
    expect(counted.averageMarks).toBe(tilesInBbox(view, counted.depth) * 40);
    expect(counted.averageMarks).not.toBe(counted.predictedMarks);
  });
});

describe('countedMarks', () => {
  const bbox = view;

  it('is exact at the depth the counts are held at', () => {
    const at = countedMarks(field(), bbox, FIELD_DEPTH, K)!;
    expect(at.exact).toBe(true);
    expect(at.marks).toBe(truth(FIELD_DEPTH));
    expect(at.capped).toBe(true);
  });

  it('bounds a depth finer than the counts from above, and never below the truth', () => {
    for (const depth of [FIELD_DEPTH + 1, FIELD_DEPTH + 2, FIELD_DEPTH + 3]) {
      const bound = countedMarks(field(), bbox, depth, K)!;
      expect(bound.exact).toBe(false);
      expect(bound.marks).toBeGreaterThanOrEqual(truth(depth));
    }
    // Tight where the parents are saturated: every one of the `4^Δ` children can serve its own `k`.
    expect(countedMarks(field(), bbox, FIELD_DEPTH + 1, K)!.marks).toBe(truth(FIELD_DEPTH + 1));

    // Loose where they are not, in the safe direction: a parent of 1,000 bounds the depth below at
    // 1,000; the truth is 1,000 spread across four children and 500 all in one.
    const parent: CountField = {
      depth: FIELD_DEPTH,
      cells: [{x: 0, y: 0, count: 1_000}],
      covers: {x0: 0, y0: 0, x1: VIEW - 1, y1: VIEW - 1}
    };
    expect(countedMarks(parent, bbox, FIELD_DEPTH + 1, K)!.marks).toBe(1_000);
  });

  it('folds into ancestors for a depth coarser than the counts', () => {
    const coarse = countedMarks(field(), bbox, FIELD_DEPTH - 1, K)!;
    expect(coarse.exact).toBe(false);
    // The block's 1,024 cells fold into 256 ancestors, each far above the cap.
    expect(coarse.marks).toBe(256 * K);
  });

  it('says nothing at all where the view is not inside the counts', () => {
    const narrow = {...field(), covers: {x0: 0, y0: 0, x1: 3, y1: 3}};
    expect(countedMarks(narrow, bbox, FIELD_DEPTH, K)).toBeNull();
  });
});

describe('calibrate', () => {
  const observation = (actual: number, predicted = 65_536, visible = 10 ** 7) => ({
    predictedMarks: predicted,
    actualMarks: actual,
    visibleInView: visible
  });

  it('lowers mTarget when fewer marks arrived than predicted, so the next request goes deeper', () => {
    const next = calibrate(observation(55_000), 16, 16);
    expect(next).toBeLessThan(16);
    expect(next).toBeGreaterThan(16 * 0.25);
  });

  it('is a no-op when the prediction was right', () => {
    expect(calibrate(observation(65_536), 16, 16)).toBe(16);
  });

  it('raises mTarget on overshoot — damped and bounded, banked to apply across motion', () => {
    // A 2x overshoot at 0.5 damping corrects halfway, inside the 4x bound. The driver holds the
    // presented depth at rest, so a raised mTarget changes only the next gesture's depth.
    const next = calibrate(observation(200_000), 16, 16);
    expect(next).toBeGreaterThan(16);
    expect(next).toBeLessThanOrEqual(16 * 4);
  });

  it('does nothing once every visible item is already drawn', () => {
    // A sparse principal serves its whole visible set from depth 4, so `actual` stays put while
    // `predicted` climbs; uncorrected, mTarget would fall to the floor and depth rise to the cap.
    expect(calibrate(observation(1366, 10 ** 6, 1366), 16, 16)).toBe(16);
  });

  it('cannot be driven to zero or to infinity by a pathological response', () => {
    expect(calibrate(observation(0), 16, 16)).toBeGreaterThan(0);
    const starved = calibrate(observation(1, 10 ** 9), 16, 16);
    expect(starved).toBeGreaterThanOrEqual(16 * 0.25);
    expect(Number.isFinite(starved)).toBe(true);
  });

  it('converges rather than oscillating, over repeated observations', () => {
    // Depth is an integer, so an undamped correction flips between two depths on alternate frames.
    let mTarget = 16;
    const history: number[] = [];
    for (let i = 0; i < 8; i++) {
      mTarget = calibrate(observation(50_000), mTarget, 16);
      history.push(mTarget);
    }
    // Non-increasing and bounded, with no sawtooth.
    for (let i = 1; i < history.length; i++) {
      expect(history[i]!).toBeLessThanOrEqual(history[i - 1]!);
    }
    expect(history[history.length - 1]!).toBeGreaterThanOrEqual(16 * 0.25);
  });
});
