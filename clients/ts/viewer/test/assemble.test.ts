import {describe, expect, it} from 'vitest';
import {bandsOfResult, mortonOfTile, type Band, type ReplicaFrame} from '@tessera/client';
import type {ScalarColumn, ViewportResult} from '@tessera/client';
import {assemble, assembledMarks, assertAssemblyMatchesServed, foldBandColumn, refreshExact} from '../src/assemble.js';

/** A band at `depth`/`prefix` whose points all sit in cell `(cx, cy)`. */
function band(depth: number, prefix: bigint, n: number, served = n, cell = {cx: 0, cy: 0}): Band {
  let morton = 0n;
  for (let bit = 0; bit < 16; bit++) {
    morton |= BigInt((cell.cx >> bit) & 1) << BigInt(2 * bit);
    morton |= BigInt((cell.cy >> bit) & 1) << BigInt(2 * bit + 1);
  }
  const scalars: Record<string, ScalarColumn> = {
    w: {arrowType: 'u32', values: Uint32Array.from({length: n}, (_, i) => i)}
  };
  let x = 0;
  let y = 0;
  for (let bit = 0; bit < 16; bit++) {
    x |= Number((prefix >> BigInt(2 * bit)) & 1n) << bit;
    y |= Number((prefix >> BigInt(2 * bit + 1)) & 1n) << bit;
  }
  return {
    depth,
    prefix,
    x,
    y,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: n}, () => morton << 32n),
    // World space already — the cell->world conversion happens when a band is built.
    positions: Float32Array.from({length: n * 2}, (_, i) =>
      (i % 2 === 0 ? cell.cx : cell.cy) / 128
    ),
    scalars,
    served,
    capUsed: 500,
    visible: BigInt(served * 3),
    matched: BigInt(served * 3),
    heldBelow: BigInt(n + 1),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0
  };
}

const WHOLE = {x0: 0, y0: 0, x1: 65535, y1: 65535};

function frame(
  depth: number,
  exact: Band[],
  fallback: Band[] = [],
  want = WHOLE
): ReplicaFrame {
  return {
    depth,
    want,
    exact,
    fallback: fallback.map((band) => ({band, clip: want})),
    response: null,
    plan: {wanted: 0, novel: 0, requests: 0}
  };
}

describe('assemble', () => {
  it('hands exact bands on without copying them, and counts against them', () => {
    const a = band(2, 0n, 3);
    const b = band(2, 1n, 2);
    const out = assemble(frame(2, [a, b]));

    // The bands themselves, by reference — the copy is the slab's job and happens once per band.
    expect(out.bands).toEqual([a, b]);
    expect(out.exactDrawn).toBe(5);
    expect(out.exactServed).toBe(5);
    expect(out.provisional).toBe(0);
    expect(out.standIn.ids.length).toBe(0);
    expect(out.visibleInView).toBe(15);
    expect(assembledMarks(out)).toBe(5);
    assertAssemblyMatchesServed(out);
  });

  it('drops an empty band rather than giving the slab a zero-length slot', () => {
    const out = assemble(frame(2, [band(2, 0n, 0, 0), band(2, 1n, 2)]));
    expect(out.bands).toHaveLength(1);
  });

  it('restricts an ancestor band to the region asked for, and marks it provisional', () => {
    // A depth-4 parent holding points in two different depth-6 tiles. At depth 6 a tile spans
    // 512/64 = 8 world units, so tile (1,0) is x in [8,16) and tile (2,0) is x in [16,24) —
    // containment is decided on the positions, which is what the restriction actually tests.
    const parent = band(4, 0n, 0);
    const enriched: Band = {
      ...parent,
      ids: BigUint64Array.from([1n, 2n, 3n]),
      codes: new BigUint64Array(3),
      positions: Float32Array.from([10, 4, 12, 4, 18, 4]),
      scalars: {w: {arrowType: 'u32', values: Uint32Array.from([7, 8, 9])}},
      served: 3
    };

    const out = assemble(frame(6, [], [enriched], {x0: 1, y0: 0, x1: 1, y1: 0}), ['w']);

    expect(out.standIn.ids.length).toBe(2); // only the two points inside that region
    expect([...out.standIn.ids]).toEqual([1n, 2n]);
    expect([...(out.standIn.scalars.w!.values as Uint32Array)]).toEqual([7, 8]);
    expect(out.provisional).toBe(2);
    expect(out.exactDrawn).toBe(0);
    expect(out.tiles[0]!.exact).toBe(false);
    expect(out.tiles[0]!.counts).toBeNull(); // the number channel is suppressed
    assertAssemblyMatchesServed(out);
  });

  it('unions descendant bands on zoom-out, density-matched per drawn tile', () => {
    const kids = [band(5, 0n, 2), band(5, 1n, 3)];
    const out = assemble(frame(3, [], kids));
    // Both bands sit under one depth-3 tile, whose own depth would serve ~(2+3)/16 ≈ 0.3 marks —
    // so the TILE gets the one-mark floor, not each band: a per-band floor is how a thousand tiny
    // deep bands once handed a coarse tile a thousand marks. The mark drawn is an id-order prefix
    // of the band with the larger share, the subset delta-serving.md permits.
    expect(out.standIn.ids.length).toBe(1);
    expect(out.provisional).toBe(1);
    // The contribution is a prefix: the band's first id, never a sample.
    expect([...out.standIn.ids]).toEqual([1n]);
    expect(out.exactServed).toBe(0); // no exact tile contributed, so nothing to assert against
    expect(out.tiles[0]!.counts).toBeNull();
    assertAssemblyMatchesServed(out);
  });

  it('bounds a drawn tile by its own density however many deep bands stand in for it', () => {
    // Sixteen five-mark bands four levels down, all under drawn tile 0: the tile's own depth
    // would serve ~16·5/256 ≈ 0.3 marks. The per-band floor drew sixteen — the "patches at a
    // totally different zoom level" a rapid zoom-out left behind at 10^9 scale.
    const kids = Array.from({length: 16}, (_, i) => band(7, BigInt(i), 5));
    const out = assemble(frame(3, [], kids));
    expect(out.provisional).toBe(1);
  });

  it('mixes exact and provisional tiles without conflating their counts', () => {
    const exact = band(4, 0n, 3);
    const kids = [band(6, 40n, 2)];
    const out = assemble(frame(4, [exact], kids));

    // The depth-6 stand-in's tile would serve ~2/16 marks: the per-tile floor gives it 1.
    expect(assembledMarks(out)).toBe(4);
    expect(out.exactDrawn).toBe(3);
    expect(out.exactServed).toBe(3);
    expect(out.provisional).toBe(1);
    expect(out.visibleInView).toBe(9); // the exact tile alone
    assertAssemblyMatchesServed(out);
  });

  it('folds a column across exact bands and stand-ins alike', () => {
    // The colour domain and the category ranks are accumulators, so they fold rather than needing
    // the concatenated column exact bands no longer build.
    const out = assemble(frame(2, [band(2, 0n, 2), band(2, 1n, 3)], [band(4, 8n, 1)]), ['w']);
    const seen = foldBandColumn(out, 'w', [] as number[], (held, values) => [
      ...held,
      ...(values.values as Uint32Array)
    ]);
    expect(seen).toEqual([0, 1, 0, 1, 2, 0]);
  });

  it('folds nothing for a column no band carries, rather than throwing', () => {
    const out = assemble(frame(2, [band(2, 0n, 2)]));
    expect(foldBandColumn(out, 'absent', 0, (n) => n + 1)).toBe(0);
    expect(foldBandColumn(out, null, 7, (n) => n + 1)).toBe(7);
  });

  it('splits a real response into bands and keeps every served mark', () => {
    const result: ViewportResult = {
      tiles: [
        {tile: 0n, visible: 9n, matched: 9n, served: 3n},
        {tile: 1n, visible: 4n, matched: 4n, served: 2n}
      ],
      ids: BigUint64Array.from([1n, 2n, 3n, 4n, 5n]),
      codes: new BigUint64Array(5),
      positions: Float64Array.from([0, 0, 128, 128, 256, 256, 384, 384, 512, 512]),
      world: Float32Array.from([0, 0, 1, 1, 2, 2, 3, 3, 4, 4]),
      scalars: {w: {arrowType: 'u32', values: Uint32Array.from([10, 11, 12, 13, 14])}},
      subCells: null
    };
    const bands = bandsOfResult(result, 2, {identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0});

    const out = assemble(frame(2, bands), ['w']);

    expect(out.bands.flatMap((b) => [...b.ids])).toEqual([...result.ids]);
    expect(
      out.bands.flatMap((b) => [...(b.scalars.w!.values as Uint32Array)])
    ).toEqual([10, 11, 12, 13, 14]);
    expect(out.exactDrawn).toBe(5);
    assertAssemblyMatchesServed(out);
  });
});

describe('assertAssemblyMatchesServed', () => {
  it('throws when an exact tile draws fewer marks than were served', () => {
    const short = band(2, 0n, 2, 3); // holds 2, server said it served 3
    const out = assemble(frame(2, [short]));
    expect(() => assertAssemblyMatchesServed(out)).toThrow(/assembly: drawing 2 marks/);
  });

  it('throws when a provisional tile carries counts', () => {
    const out = assemble(frame(2, [], [band(4, 0n, 2)]));
    out.tiles[0]!.counts = {visible: 1n, matched: 1n, served: 1};
    expect(() => assertAssemblyMatchesServed(out)).toThrow(/superset of marks must never be read as density/);
  });
});

describe('refreshExact', () => {
  it('folds fresh exact bands in while keeping the stand-ins by reference', () => {
    // The stand-in's marks sit in drawn tile (2,0) — ground no fresh band covers — so the buffers
    // must survive the fold untouched.
    const held = assemble(frame(2, [band(2, 0n, 3)], [band(4, 8n, 2, 2, {cx: 32768, cy: 0})]), ['w']);
    const fresh = [band(2, 0n, 3), band(2, 1n, 2)];

    const out = refreshExact(held, fresh, 7);

    expect(out.version).toBe(7);
    expect(out.exactDrawn).toBe(5);
    expect(out.exactServed).toBe(5);
    expect(out.visibleInView).toBe(15);
    // The stand-in buffers ride along untouched — same object, so the memoised binary descriptors
    // and colour buffer stay valid and nothing re-uploads.
    expect(out.standIn).toBe(held.standIn);
    expect(out.provisional).toBe(held.provisional);
    // Non-exact tile entries survive; exact ones are rebuilt from the fresh bands.
    expect(out.tiles.filter((t) => !t.exact)).toEqual(held.tiles.filter((t) => !t.exact));
    expect(out.tiles.filter((t) => t.exact)).toHaveLength(2);
    assertAssemblyMatchesServed(out);
  });

  it('drops an empty band rather than counting a zero-length slot', () => {
    const held = assemble(frame(2, [band(2, 0n, 2)]));
    const out = refreshExact(held, [band(2, 0n, 0, 0), band(2, 1n, 2)], 1);
    expect(out.bands).toHaveLength(1);
    expect(out.exactDrawn).toBe(2);
  });

  it('drops stand-in marks over ground an arriving band now answers exactly', () => {
    // The stand-in's one density-matched mark sits in drawn tile (0,0); the fold brings an exact
    // band for that very tile. Keeping the mark would draw the ground twice — the ~2x flash the
    // density audit measured on every arrival — so the fold removes it.
    const held = assemble(frame(2, [], [band(4, 0n, 2)]));
    expect(held.provisional).toBe(1);
    const out = refreshExact(held, [band(2, 0n, 3)], 1);
    expect(out.provisional).toBe(0);
    expect(out.standIn.ids.length).toBe(0);
    assertAssemblyMatchesServed(out);
  });
});
