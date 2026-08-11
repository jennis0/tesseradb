import {describe, expect, it} from 'vitest';
import {MIN_DEPTH, calibrate, chooseDepth, tilesInBbox} from '../src/budget.js';
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
