import {describe, expect, it} from 'vitest';
import {
  MARGIN,
  RING_MARGIN,
  RING_MARGIN_MAX,
  deeperFetch,
  plan,
  ringMargin,
  worldBbox,
  type PlannerInputs
} from '../src/prefetch.js';
import {tileOfCode} from '../src/coords.js';
import {rectArea, rectContains} from '../src/rects.js';

const BASE: PlannerInputs = {
  viewport: {target: [256, 256], zoom: 4, width: 1280, height: 800},
  budget: 50_000,
  mTarget: 16,
  maxTiles: 262_144
};

describe('worldBbox', () => {
  it('covers the viewport at margin 1, and grows with the margin', () => {
    const tight = worldBbox(BASE.viewport, 1);
    const wide = worldBbox(BASE.viewport, 2);
    expect(tight[2] - tight[0]).toBeCloseTo(1280 / 2 ** 4, 6);
    expect(wide[2] - wide[0]).toBeCloseTo(2 * (1280 / 2 ** 4), 6);
  });

  it('clamps to the world rather than running off it', () => {
    const corner = worldBbox({target: [0, 0], zoom: 0, width: 1280, height: 800}, 1);
    expect(corner[0]).toBe(0);
    expect(corner[1]).toBe(0);
    expect(corner[2]).toBeLessThanOrEqual(512);
  });
});

describe('plan', () => {
  it('leads the background with the zoom shadow: depth+L over the visible box', () => {
    // The one region anticipation can be certain about — a zoom lands on ground already on
    // screen — and the one the pan ring cannot help. The layer count is a knob (D5 posture:
    // design budgets, then measurement); 0 disables the shadow without touching the ring.
    const p = plan({...BASE, depthLayers: 2});
    const deeper = p.background.filter((b) => b.kind === 'deeper');
    expect(deeper.length).toBeGreaterThan(0);
    expect(deeper.length).toBeLessThanOrEqual(2);
    expect(deeper[0]!.depth).toBe(p.choice.depth + 1);
    expect(p.background[0]!.kind).toBe('deeper');
    const none = plan({...BASE, depthLayers: 0});
    expect(none.background.filter((b) => b.kind === 'deeper')).toHaveLength(0);
  });

  it('chooses depth for the visible box, not the margined one', () => {
    // Depth must not drop just because the request covers more than the screen: that would spend
    // the budget on off-screen marks and lower the resolution of what is actually being looked at.
    const visibleOnly = plan({...BASE});
    const asIfMargined = plan({
      ...BASE,
      viewport: {...BASE.viewport, width: BASE.viewport.width * MARGIN, height: BASE.viewport.height * MARGIN}
    });
    expect(visibleOnly.choice.depth).toBeGreaterThanOrEqual(asIfMargined.choice.depth);
  });

  it('plans the visible box separately, and inside the margined one', () => {
    // The screen is fetched first so it fills in its own time rather than the margin's; the margin
    // is then a subtraction away and costs a second, off-critical-path request.
    const p = plan(BASE);
    expect(rectArea(p.visible.rect)).toBeLessThan(rectArea(p.foreground.rect));
    expect(rectContains(p.foreground.rect, p.visible.rect)).toBe(true);
    expect(p.visible.depth).toBe(p.choice.depth);
  });

  it('fetches the margined box in the foreground', () => {
    const p = plan(BASE);
    const tight = plan({...BASE, viewport: {...BASE.viewport, width: 1, height: 1}});
    expect(rectArea(p.foreground.rect)).toBeGreaterThan(rectArea(tight.foreground.rect));
    expect(p.foreground.depth).toBe(p.choice.depth);
  });

  it('rings wider than the foreground, at the same depth, and contains it', () => {
    const p = plan(BASE);
    const ring = p.background.find((b) => b.kind === 'ring')!;
    expect(ring.depth).toBe(p.foreground.depth);
    expect(rectArea(ring.rect)).toBeGreaterThan(rectArea(p.foreground.rect));
    // The ring only ever adds ground, never replaces it.
    expect(rectContains(ring.rect, p.foreground.rect)).toBe(true);
  });

  it('biases the ring downwind of recent movement', () => {
    const stillRing = plan(BASE).background.find((b) => b.kind === 'ring')!;
    const movingRing = plan({...BASE, velocity: [1, 0]}).background.find((b) => b.kind === 'ring')!;
    expect(rectArea(movingRing.rect)).toBe(rectArea(stillRing.rect)); // same cost...
    expect(movingRing.rect.x1).toBeGreaterThan(stillRing.rect.x1); // ...different place
  });

  it('reaches further while the replica is empty, and pulls in as it fills', () => {
    // Reach is spent on extra, coarser bands rather than on a bigger fine one, so it is the band
    // COUNT that responds to how full the cache is.
    const bands = (heldBytes: number) =>
      plan({...BASE, heldBytes, budgetBytes: 512e6}).background.length;
    expect(bands(0)).toBeGreaterThan(bands(154e6));
    expect(bands(154e6)).toBeGreaterThanOrEqual(bands(512e6));
    expect(bands(512e6)).toBeGreaterThanOrEqual(1);
  });

  it('grades the ring by depth, each band coarser and wider than the last', () => {
    const p = plan({...BASE, heldBytes: 0, budgetBytes: 512e6});
    const ring = p.background.filter((b) => b.kind === 'ring');
    expect(ring.length).toBeGreaterThan(1);
    for (let i = 1; i < ring.length; i++) {
      expect(ring[i]!.depth).toBe(ring[i - 1]!.depth - 1);
    }
    // Each band is about the tile count of the one before: twice the reach at a quarter the
    // density. That is what makes a wide ring affordable rather than quadratic.
    for (let i = 1; i < ring.length; i++) {
      const ratio = rectArea(ring[i]!.rect) / rectArea(ring[i - 1]!.rect);
      expect(ratio).toBeGreaterThan(0.3);
      expect(ratio).toBeLessThan(3);
    }
  });

  it('never reaches past the fixed floor, however full', () => {
    expect(ringMargin(1e12, 512e6)).toBe(RING_MARGIN);
    expect(ringMargin(0, 512e6)).toBe(RING_MARGIN_MAX);
    // No budget declared is the conservative answer, not the aggressive one.
    expect(ringMargin(0, 0)).toBe(RING_MARGIN);
  });

  it('drops the ring rather than breaching the tile ceiling', () => {
    const p = plan({...BASE, maxTiles: 4});
    expect(p.background.find((b) => b.kind === 'ring')).toBeUndefined();
  });

  it('costs the same whatever the viewport spans', () => {
    // The point of planning in rectangles: a view covering a quarter of a million tiles plans in
    // the same handful of arithmetic as one covering four.
    const wide = {...BASE, viewport: {...BASE.viewport, zoom: 0}};
    const t = performance.now();
    for (let i = 0; i < 200; i++) plan(wide);
    expect(performance.now() - t).toBeLessThan(100); // 200 plans, not 200 enumerations
  });

  it('is deterministic', () => {
    expect(plan(BASE)).toEqual(plan(BASE));
  });
});

describe('deeperFetch', () => {
  it('asks one depth down over the centre, at the tile cost of the foreground', () => {
    const p = plan(BASE);
    const deeper = deeperFetch(BASE, p.choice)!;
    expect(deeper.depth).toBe(p.choice.depth + 1);
    // Four times the tile density over a quarter of the area: the same count, give or take the
    // rounding of a box onto a grid.
    expect(rectArea(deeper.rect)).toBeLessThanOrEqual(rectArea(p.foreground.rect) * 2);
  });

  it('folds back inside the foreground one level up', () => {
    const p = plan(BASE);
    const deeper = deeperFetch(BASE, p.choice)!;
    const folded = {
      x0: deeper.rect.x0 >> 1,
      y0: deeper.rect.y0 >> 1,
      x1: deeper.rect.x1 >> 1,
      y1: deeper.rect.y1 >> 1
    };
    expect(rectContains(p.foreground.rect, folded)).toBe(true);
  });

  it('stops at the grid floor', () => {
    const atFloor = {...BASE, viewport: {...BASE.viewport, zoom: 16}};
    const choice = {depth: 16, tiles: 1, predictedMarks: 0, limitedBy: 'maxDepth' as const};
    expect(deeperFetch(atFloor, choice)).toBeNull();
  });

  it('declines rather than breaching the tile ceiling', () => {
    const p = plan(BASE);
    expect(deeperFetch({...BASE, maxTiles: 4}, p.choice)).toBeNull();
  });
});

describe('nesting', () => {
  it('a deeper tile folds into the shallower one covering it', () => {
    // The property look-ahead rests on: requesting depth d+1 under a view that needs d is a
    // superset, so nothing pops when the user arrives there.
    const code = (0xabcd1234n << 32n) | 0x5678n;
    for (let z = 1; z <= 16; z++) {
      expect(tileOfCode(code, z) >> 2n).toBe(tileOfCode(code, z - 1));
    }
  });
});
