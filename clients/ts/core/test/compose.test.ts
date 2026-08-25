import {describe, expect, it} from 'vitest';
import {compose, fold} from '../src/compose.js';
import type {Band} from '../src/bands.js';
import type {ReplicaFrame} from '../src/replica.js';
import type {ScalarColumn} from '../src/types.js';

function band(depth: number, prefix: bigint, n: number, served = n): Band {
  let x = 0;
  let y = 0;
  for (let bit = 0; bit < 16; bit++) {
    x |= Number((prefix >> BigInt(2 * bit)) & 1n) << bit;
    y |= Number((prefix >> BigInt(2 * bit + 1)) & 1n) << bit;
  }
  const scalars: Record<string, ScalarColumn> = {};
  return {
    depth,
    prefix,
    x,
    y,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    positions: new Float32Array(n * 2),
    scalars,
    served,
    capUsed: 500,
    visible: BigInt(served * 3),
    matched: BigInt(served * 3),
    membership: {},
    heldBelow: BigInt(n + 1),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0
  };
}

const WHOLE = {x0: 0, y0: 0, x1: 65535, y1: 65535};

function frame(depth: number, exact: Band[], fallback: Band[] = []): ReplicaFrame {
  return {
    depth,
    want: WHOLE,
    exact,
    fallback: fallback.map((b) => ({band: b, clip: WHOLE})),
    version: 1,
    response: null,
    plan: {wanted: 1, novel: 0, requests: 0, bytes: 0}
  };
}

describe('compose: truncated bands', () => {
  it('demotes an eviction-truncated band to a stand-in instead of failing the served count', () => {
    // Eviction keeps a band's head and its `served`; counting it exact would throw the
    // drawn-equals-served fidelity check on every paint until refetch — a crash loop under
    // exactly the memory pressure eviction exists for (review finding 2).
    const whole = band(3, 0n, 4);
    const cut = {...band(3, 1n, 6), ids: BigUint64Array.from([1n, 2n, 3n])}; // served 6, holds 3
    const c = compose(frame(3, [whole, cut]));
    expect(c.exactDrawn).toBe(4);
    expect(c.exactServed).toBe(4); // the truncated band is out of the equality's domain
    expect(c.provisional).toBe(3); // its head still draws, as a stand-in
    expect(c.tiles.filter((t) => !t.exact)).toHaveLength(1);
    expect(c.tiles.find((t) => !t.exact)?.counts).toBeNull();
  });

  it('a truncated head supersedes deeper stand-ins over its own tile', () => {
    const cut = {...band(3, 0n, 6), ids: BigUint64Array.from([1n, 2n])};
    const kid = band(5, 0n, 8); // projects to the same drawn tile (0,0)
    const c = compose(frame(3, [cut], [kid]));
    expect(c.provisional).toBe(2); // the head alone; the descendant is skipped
  });

  it('fold demotes truncation the same way and rebuilds tile entries from surviving pieces', () => {
    const held = compose(frame(3, [], [band(5, 0n, 8), band(5, 320n, 8)]));
    expect(held.provisional).toBe(2); // two drawn tiles, one density floor each
    const cut = {...band(3, 1n, 6), ids: BigUint64Array.from([1n, 2n, 3n])};
    const folded = fold(held, [band(3, 0n, 4), cut], 2);
    expect(folded.exactDrawn).toBe(4);
    expect(folded.exactServed).toBe(4);
    // Tile (0,0) became exact: its stand-in is gone. Tile (1,0) is the truncated head. The
    // second descendant (tile (0,4)) survives. Every non-exact tile entry's drawn matches a
    // surviving piece — no carried entry can overstate (review finding 5).
    const nonExact = folded.tiles.filter((t) => !t.exact);
    expect(folded.provisional).toBe(nonExact.reduce((a, t) => a + t.drawn, 0));
  });
});
