import {describe, expect, it} from 'vitest';
import {BandCache, bandsOfResult, isComplete, type Band} from '../src/bands.js';
import {tileContains, tileOfCode} from '../src/coords.js';
import type {ScalarColumn, ViewportResult} from '../src/types.js';

/**
 * A band whose identities are `base, base+1, …` — ascending, as the wire delivers them, which is
 * what every prefix operation here depends on.
 */
function band(overrides: Partial<Band> & {depth: number; prefix: bigint; n: number}): Band {
  const {n, ...rest} = overrides;
  const ids = BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1));
  const codes = BigUint64Array.from({length: n}, () => overrides.prefix << BigInt(64 - 2 * overrides.depth));
  return {
    ids,
    codes,
    positions: new Float64Array(n * 2),
    scalars: {},
    served: n,
    capUsed: 500,
    visible: BigInt(n),
    matched: BigInt(n),
    heldBelow: n === 0 ? 0n : ids[n - 1]! + 1n,
    identityKey: 'ik',
    contentKey: 'ck',
    // What the store itself would compute: 8 for the id, 8 for the code, 16 for the position pair.
    bytes: n * 32,
    touchedAt: 0,
    ...rest
  };
}

describe('tile addressing', () => {
  it('takes the prefix over the Morton cell, not the whole code', () => {
    // The cell is the code's high half. At depth 16 the tile IS the cell, so a shift of 32-2z
    // would return the whole 64-bit code and bucket every point separately.
    const code = (0xdeadbeefn << 32n) | 0x12345678n;
    expect(tileOfCode(code, 16)).toBe(0xdeadbeefn);
    expect(tileOfCode(code, 0)).toBe(0n);
    expect(tileOfCode(code, 8)).toBe(0xdeadbeefn >> 16n);
  });

  it('makes containment prefix containment', () => {
    const parent = 0b1101n;
    expect(tileContains(parent, 2, (parent << 2n) | 0b11n, 3)).toBe(true);
    expect(tileContains(parent, 2, (parent << 2n) | 0b01n, 3)).toBe(true);
    expect(tileContains(parent, 2, 0b1100n, 2)).toBe(false);
    expect(tileContains(parent, 2, parent, 1)).toBe(false);
  });
});

describe('bandsOfResult', () => {
  it('splits by served, which is the only recoverable grouping', () => {
    const scalars: Record<string, ScalarColumn> = {
      w: {arrowType: 'u32', values: Uint32Array.from([10, 11, 12, 13, 14])}
    };
    const result: ViewportResult = {
      tiles: [
        {tile: 7n, visible: 90n, matched: 90n, served: 2n},
        {tile: 8n, visible: 0n, matched: 0n, served: 0n},
        {tile: 9n, visible: 40n, matched: 40n, served: 3n}
      ],
      ids: BigUint64Array.from([1n, 2n, 5n, 6n, 7n]),
      codes: BigUint64Array.from([0n, 0n, 0n, 0n, 0n]),
      positions: Float64Array.from([0, 0, 1, 1, 2, 2, 3, 3, 4, 4]),
      scalars,
      subCells: null
    };

    const bands = bandsOfResult(result, 4, {identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0});

    expect(bands.map((b) => b.prefix)).toEqual([7n, 9n]); // the empty tile yields no band
    expect([...bands[0]!.ids]).toEqual([1n, 2n]);
    expect([...bands[1]!.ids]).toEqual([5n, 6n, 7n]);
    expect([...(bands[1]!.scalars.w!.values as Uint32Array)]).toEqual([12, 13, 14]);
    expect(bands[1]!.positions).toEqual(Float64Array.from([2, 2, 3, 3, 4, 4]));
    expect(bands[0]!.heldBelow).toBe(3n); // one past the largest held identity
  });

  it('copies rather than views, so evicting a band frees its bytes', () => {
    const result: ViewportResult = {
      tiles: [{tile: 1n, visible: 2n, matched: 2n, served: 2n}],
      ids: BigUint64Array.from([1n, 2n, 3n, 4n]),
      codes: BigUint64Array.from([0n, 0n, 0n, 0n]),
      positions: new Float64Array(8),
      scalars: {},
      subCells: null
    };
    const [only] = bandsOfResult(result, 1, {identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0});
    // A subarray would share the 4-element response buffer; a copy owns exactly its own two.
    expect(only!.ids.buffer.byteLength).toBe(2 * 8);
  });
});

describe('isComplete', () => {
  it('requires the whole served set', () => {
    expect(isComplete(band({depth: 3, prefix: 1n, n: 10, served: 10}), 'ck', 500)).toBe(true);
    expect(isComplete(band({depth: 3, prefix: 1n, n: 9, served: 10}), 'ck', 500)).toBe(false);
  });

  it('survives a larger k when the cap was not the binding clause', () => {
    // served < capUsed: theta or the floor decided, and neither moves with k.
    const thetaBound = band({depth: 3, prefix: 1n, n: 40, served: 40, capUsed: 500});
    expect(isComplete(thetaBound, 'ck', 2000)).toBe(true);
  });

  it('is void at a larger k when the cap WAS binding', () => {
    const capped = band({depth: 3, prefix: 1n, n: 500, served: 500, capUsed: 500});
    expect(isComplete(capped, 'ck', 500)).toBe(true);
    expect(isComplete(capped, 'ck', 2000)).toBe(false);
  });

  it('is void under a rotated content key', () => {
    expect(isComplete(band({depth: 3, prefix: 1n, n: 10, served: 10}), 'other', 500)).toBe(false);
  });
});

describe('BandCache.plan', () => {
  it('omits proven-complete tiles and declares a bound for the rest', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 4, prefix: 1n, n: 10, served: 10}));
    cache.put(band({depth: 4, prefix: 2n, n: 6, served: 9})); // partial
    // tile 3 is not held at all

    const plan = cache.plan([1n, 2n, 3n], 4, 'ck', 500);

    expect(plan.omit).toEqual([1n]);
    expect(plan.fetch).toEqual([
      {prefix: 2n, below: 7n, count: 6},
      {prefix: 3n, below: 0n, count: 0}
    ]);
  });

  it('omits nothing on a counts-only request', () => {
    // At k = 0 the definition serves nothing, so every held band trivially holds all of served(T)
    // and the completeness test goes vacuous. Applying it would omit every tile and refresh no
    // counts — which is precisely what the counts-only request exists to do.
    const cache = new BandCache(1e9);
    cache.put(band({depth: 4, prefix: 1n, n: 10, served: 10}));
    cache.put(band({depth: 4, prefix: 2n, n: 10, served: 10}));

    const plan = cache.plan([1n, 2n], 4, 'ck', 0);

    expect(plan.omit).toEqual([]);
    expect(plan.fetch.map((f) => f.prefix)).toEqual([1n, 2n]);
  });

  it('declares nothing from a band whose content key has rotated', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 4, prefix: 1n, n: 10, served: 10, contentKey: 'old'}));

    const plan = cache.plan([1n], 4, 'new', 500);

    // Renderable, but not declarable: a rotation may have added identities below the bound.
    expect(plan.omit).toEqual([]);
    expect(plan.fetch).toEqual([{prefix: 1n, below: 0n, count: 0}]);
    expect(cache.get(4, 1n)).toBeDefined();
  });
});

describe('BandCache.resolve', () => {
  it('prefers the exact band', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 4, prefix: 0b1001n, n: 3}));
    const resolved = cache.resolve(4, 0b1001n)!;
    expect(resolved.provenance).toBe('exact');
    expect(resolved.exact).toBe(true);
  });

  it('falls back to the nearest ancestor, marked inexact', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 2, prefix: 0b10n, n: 3}));
    const resolved = cache.resolve(4, (0b10n << 4n) | 0b0111n)!;
    expect(resolved.provenance).toBe('ancestor');
    expect(resolved.exact).toBe(false);
    expect(resolved.bands[0]!.depth).toBe(2);
  });

  it('falls back to held descendants on zoom-out, marked inexact', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 5, prefix: (0b11n << 4n) | 0b0110n, n: 3}));
    cache.put(band({depth: 5, prefix: (0b11n << 4n) | 0b1001n, n: 3}));
    cache.put(band({depth: 5, prefix: (0b10n << 4n) | 0b0001n, n: 3})); // different parent
    const resolved = cache.resolve(3, 0b11n)!;
    expect(resolved.provenance).toBe('descendants');
    expect(resolved.exact).toBe(false);
    expect(resolved.bands).toHaveLength(2);
  });

  it('returns null when nothing is held', () => {
    expect(new BandCache(1e9).resolve(4, 1n)).toBeNull();
  });
});

describe('BandCache eviction', () => {
  it('truncates tails and never removes a band head', () => {
    const cache = new BandCache(600);
    for (let i = 0; i < 8; i++) {
      cache.put(band({depth: 3 + (i % 3), prefix: BigInt(i), n: 4, touchedAt: i}));
    }
    expect(cache.bytes).toBeGreaterThan(600);

    cache.evict({depth: 3, prefix: 0n});

    expect(cache.size).toBe(8); // nothing dropped
    for (let i = 0; i < 8; i++) {
      const held = cache.get(3 + (i % 3), BigInt(i))!;
      expect(held.ids.length).toBeGreaterThanOrEqual(1); // the head survives
      expect(held.ids[0]).toBe(1n); // and it is the LOW-identity head
    }
  });

  it('lowers a truncated band bound to exactly what remains', () => {
    const cache = new BandCache(24); // forces truncation on the first pass
    cache.put(band({depth: 6, prefix: 1n, n: 8}));
    cache.evict({depth: 6, prefix: 0n});
    const held = cache.get(6, 1n)!;
    expect(held.heldBelow).toBe(held.ids[held.ids.length - 1]! + 1n);
    expect(held.ids.length).toBeLessThan(8);
  });

  it('takes the deepest band first, and stops once under the mark', () => {
    // Two 128-byte bands against a 250-byte budget: truncating the deeper one to 64 reaches the
    // 225-byte low-water mark, so the shallow band is never touched.
    const cache = new BandCache(250);
    cache.put(band({depth: 2, prefix: 1n, n: 4, touchedAt: 0}));
    cache.put(band({depth: 9, prefix: 2n, n: 4, touchedAt: 0}));
    cache.evict({depth: 2, prefix: 0n});
    expect(cache.get(9, 2n)!.ids.length).toBe(2);
    expect(cache.get(2, 1n)!.ids.length).toBe(4);
  });

  it('stops at the heads rather than going under a budget it cannot meet', () => {
    // Every band is already one point; there is nothing left to truncate, and dropping a head is
    // what would blank overview rendering. Overshooting the budget is the correct answer.
    const cache = new BandCache(1);
    for (let i = 0; i < 4; i++) cache.put(band({depth: 5, prefix: BigInt(i), n: 1}));
    cache.evict({depth: 5, prefix: 0n});
    expect(cache.size).toBe(4);
    expect(cache.bytes).toBeGreaterThan(1);
  });
});

describe('BandCache identity partition', () => {
  it('drops the whole partition when the principal changes', () => {
    const cache = new BandCache(1e9);
    cache.put(band({depth: 4, prefix: 1n, n: 4, identityKey: 'alice'}));
    cache.put(band({depth: 4, prefix: 2n, n: 4, identityKey: 'bob'}));
    expect(cache.get(4, 1n)).toBeUndefined();
    expect(cache.get(4, 2n)).toBeDefined();
    expect(cache.bytes).toBe(cache.get(4, 2n)!.bytes);
  });
});
