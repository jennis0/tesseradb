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
    // `tessera-spatial`'s tile_corners quantises both corners and iterates INCLUSIVELY, so a
    // viewport whose edge lands exactly on a tile boundary genuinely touches both tiles. This
    // function predicts the server's tile count, so it must agree — including here, where the
    // intuitive answer (1) is the wrong one.
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
    // Deeper as less is in view — the whole point.
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
    // 1e9 `narrow`: 1,366 visible. Measured — it draws all of them from depth 4 onward, so
    // anything deeper is pure cost. Without this the loop ratchets to the maxTiles cap forever.
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
 * A gazetteer's shape, at the scale the average model was measured failing on: a solid block of
 * saturated ground inside an otherwise empty view.
 *
 * The truth is generative — `MEMBERS` members spread evenly over the block — so the marks a request
 * at any depth costs can be computed independently of the code under test, which is what makes the
 * assertions below about the prediction rather than about themselves. GeoNames' own numbers are the
 * calibration: 13.5 x 10^6 points, `k` = 500, budget 500,000, and a depth the average model chose
 * answered with 4x the budget.
 */
const LAND = 32; // the block's side, in tiles at FIELD_DEPTH
const VIEW = 64; // the view's side, in tiles at FIELD_DEPTH
const FIELD_DEPTH = 8;
const MEMBERS = 10 ** 8;
const K = 500;
const BUDGET = 500_000;
/** Sixty-four tiles a side at depth 8, stopping short of the boundary tile. See `tilesInBbox`. */
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
  // `mTarget` at its 4x clamp, which is where the measured session's correction sat: the average
  // model had nothing left to give before the response reached 100 MB.
  const bimodal = {budget: BUDGET, mTarget: 40, maxTiles: 262_144, worldBbox: view, k: K};

  it('the average model overshoots 4x where the count model fits', () => {
    const average = chooseDepth(bimodal);
    expect(average.source).toBe('average');
    // It predicts a little over the budget and is answered with four times it — the whole defect.
    expect(average.predictedMarks).toBeLessThan(BUDGET * 1.5);
    expect(truth(average.depth)).toBeGreaterThan(BUDGET * 4);

    const counted = chooseDepth({...bimodal, counts: field()});
    expect(counted.source).toBe('bound');
    expect(counted.depth).toBeLessThan(average.depth);
    expect(counted.predictedMarks).toBeLessThanOrEqual(BUDGET);
    // The figure that matters: what the server would serve at the chosen depth, not what was
    // predicted for it.
    expect(truth(counted.depth)).toBeLessThanOrEqual(BUDGET);
  });

  it('takes the deepest depth that fits, not the first that stops missing', () => {
    const counted = chooseDepth({...bimodal, counts: field()});
    expect(truth(counted.depth + 1)).toBeGreaterThan(BUDGET);
    expect(counted.limitedBy).toBe('budget');
  });

  it('stops where a step deeper buys tiles and not marks — a few capped cells do not pull the depth down', () => {
    // A field at depth 8 whose 64 x 64 cells hold 6 members each — nothing capped — except one
    // city cell of 3,000. The deepest fitting depth is 12, the first where nothing is capped;
    // its marks are the whole field's, and depth 8 is already within 15% of them at 4^4 fewer
    // tiles. The old rule walked to 12.
    const cells = [];
    for (let x = 0; x < 64; x++) for (let y = 0; y < 64; y++) cells.push({x, y, count: x === 10 && y === 10 ? 3_000 : 6});
    const counts: CountField = {depth: 8, cells, covers: {x0: 0, y0: 0, x1: 63, y1: 63}};
    const choice = chooseDepth({budget: 500_000, mTarget: 40, maxTiles: 262_144, worldBbox: view, k: 500, counts});
    // Depth 8 — the field's own — already serves all but the city's members, so it is taken and
    // not the first uncapped depth. (Depth 5 would show the same marks by the fold, but a fold
    // counts an ancestor's marks outside the view too, so the rule never goes above the field.)
    expect(choice.source).toBe('counts');
    expect(choice.depth).toBe(8);
    expect(choice.limitedBy).toBe('saturated');
  });

  it('falls back to the average where the counts do not cover the view', () => {
    // The same field shifted off the view: present, and silent about the ground being asked about.
    const elsewhere = {...field(), covers: {x0: 1_000, y0: 1_000, x1: 1_064, y1: 1_064}};
    expect(chooseDepth({...bimodal, counts: elsewhere}).source).toBe('average');
    expect(chooseDepth({...bimodal, counts: field(), k: undefined}).source).toBe('average');
    expect(chooseDepth(bimodal).source).toBe('average');
    // And the fallback is the model it always was — the first request of a session is unchanged.
    expect(chooseDepth({...bimodal, counts: elsewhere})).toEqual(chooseDepth(bimodal));
  });

  it('stops where nothing is capped rather than ratcheting to the tile guard', () => {
    // Every cell under the cap: the whole visible set is served here, so a deeper request pays four
    // times the tiles for the same marks. The average model needed `visibleInView` to learn this;
    // the counts say it outright.
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

    // Loose where they are not, and loose in the safe direction. A parent of 1,000 bounds the depth
    // below at 1,000; the truth is 1,000 with the members spread across four children and 500 with
    // them all in one, and the bound covers both.
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
    // The one-directional rule's premise — overshoot is a payload question — died at 10^9, where
    // density-scaled m(T) reached 4-8x the budget and 3.8e6 resident marks rasterised at 11 fps.
    // The pop-out objection (§7.2/§7.3) is honoured by WHERE the correction lands: the driver
    // holds the presented depth at rest, so a raised mTarget changes only the next gesture's
    // depth choice. Here: 2x overshoot at 0.5 damping corrects halfway, inside the 4x bound.
    const next = calibrate(observation(200_000), 16, 16);
    expect(next).toBeGreaterThan(16);
    expect(next).toBeLessThanOrEqual(16 * 4);
  });

  it('does nothing once every visible item is already drawn', () => {
    // The runaway the reviewer found: a sparse principal serves its whole visible set from depth 4,
    // so `actual` is pinned while `predicted` climbs, and an uncorrected loop drives mTarget to the
    // floor and depth to the tile cap — permanently, for 1,366 marks.
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
    // Monotone non-increasing and bounded — never a sawtooth.
    for (let i = 1; i < history.length; i++) {
      expect(history[i]!).toBeLessThanOrEqual(history[i - 1]!);
    }
    expect(history[history.length - 1]!).toBeGreaterThanOrEqual(16 * 0.25);
  });
});
