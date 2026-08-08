import {describe, expect, it} from 'vitest';
import {MARGIN, RING_MARGIN, deeperFetch, plan, worldBbox, type PlannerInputs} from '../src/prefetch.js';
import {tileOfCode} from '../src/coords.js';

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

  it('fetches the margined box in the foreground', () => {
    const p = plan(BASE);
    const tight = plan({...BASE, viewport: {...BASE.viewport, width: 1, height: 1}});
    expect(p.foreground.tiles.length).toBeGreaterThan(tight.foreground.tiles.length);
    expect(p.foreground.depth).toBe(p.choice.depth);
  });

  it('rings wider than the foreground, at the same depth', () => {
    const p = plan(BASE);
    const ring = p.background.find((b) => b.kind === 'ring')!;
    expect(ring.depth).toBe(p.foreground.depth);
    expect(ring.tiles.length).toBeGreaterThan(p.foreground.tiles.length);
    // Every foreground tile is inside the ring, so the ring only ever adds work, never replaces it.
    const inRing = new Set(ring.tiles);
    expect(p.foreground.tiles.every((t) => inRing.has(t))).toBe(true);
  });

  it('biases the ring downwind of recent movement', () => {
    const still = plan(BASE);
    // Panning right: the ring should reach further right than it does left.
    const moving = plan({...BASE, velocity: [1, 0]});
    const stillRing = still.background.find((b) => b.kind === 'ring')!;
    const movingRing = moving.background.find((b) => b.kind === 'ring')!;

    const maxX = (tiles: bigint[], depth: number) =>
      Math.max(...tiles.map((t) => Number(t & ((1n << BigInt(2 * depth)) - 1n))));
    expect(movingRing.tiles.length).toBe(stillRing.tiles.length); // same cost...
    expect(maxX(movingRing.tiles, movingRing.depth)).toBeGreaterThan(
      maxX(stillRing.tiles, stillRing.depth)
    ); // ...different place
  });

  it('drops the ring rather than breaching the tile ceiling', () => {
    const p = plan({...BASE, maxTiles: 4});
    expect(p.background.find((b) => b.kind === 'ring')).toBeUndefined();
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
    expect(deeper.tiles.length).toBeLessThanOrEqual(p.foreground.tiles.length * 2);
  });

  it('covers the centre of the view', () => {
    const p = plan(BASE);
    const deeper = deeperFetch(BASE, p.choice)!;
    // The tile under the viewport centre, at the deeper depth, must be in the set.
    const centreTiles = deeper.tiles;
    expect(centreTiles.length).toBeGreaterThan(0);
    // And every one of them sits inside the foreground's coverage when folded back up a level.
    const foreground = new Set(p.foreground.tiles);
    expect(centreTiles.every((t) => foreground.has(t >> 2n))).toBe(true);
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
