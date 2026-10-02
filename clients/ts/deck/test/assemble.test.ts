import {describe, expect, it} from 'vitest';
import {type Band} from '@tesseradb/client';
import {bandsOfResult, mortonOfTile, type ReplicaFrame} from '@tesseradb/client/internal';
import type {ScalarColumn, ViewportResult} from '@tesseradb/client';
import {band as heldBand, refused} from '../../core/test/support.js';
import {
  assemble,
  assembledMarks,
  assertAssemblyMatchesServed,
  foldBandColumn,
  materialiseStandIn,
  refreshExact
} from '../src/assemble.js';

/** A band at `depth`/`prefix` whose points all sit in cell `(cx, cy)`. */
function band(depth: number, prefix: bigint, n: number, served = n, cell = {cx: 0, cy: 0}): Band {
  const visible = BigInt(served * 3);
  return heldBand(depth, prefix, n, {
    // World space already: the cell-to-world conversion happens when a band is built.
    positions: Float32Array.from({length: n * 2}, (_, i) => (i % 2 === 0 ? cell.cx : cell.cy) / 128),
    scalars: {w: {arrowType: 'u32', values: Uint32Array.from({length: n}, (_, i) => i)}},
    served,
    visible,
    matched: visible,
    highlighted: visible
  });
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
    version: 0,
    exact,
    fallback: fallback.map((band) => ({band, clip: want})),
    response: null,
    plan: {wanted: 0, novel: 0, requests: 0, bytes: 0}
  };
}

describe('assemble', () => {
  it('hands exact bands on without copying them, and counts against them', () => {
    const a = band(2, 0n, 3);
    const b = band(2, 1n, 2);
    const out = assemble(frame(2, [a, b]));

    // The bands themselves, by reference; the slab copies each band once.
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
    // 512/64 = 8 world units, so tile (1,0) is x in [8,16) and tile (2,0) is x in [16,24).
    // Containment is decided on the positions, which is what the restriction tests.
    const parent = band(4, 0n, 0);
    const enriched: Band = {
      ...parent,
      ids: BigUint64Array.from([1n, 2n, 3n]),
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
    // Both bands sit under one depth-3 tile, whose own depth would serve about (2+3)/16 = 0.3
    // marks, so the tile gets the one-mark floor, not each band; a per-band floor would give a
    // coarse tile over many tiny bands as many marks. The mark drawn is an id-order prefix of the
    // band with the larger share.
    expect(out.standIn.ids.length).toBe(1);
    expect(out.provisional).toBe(1);
    // The contribution is a prefix: the band's first id, never a sample.
    expect([...out.standIn.ids]).toEqual([1n]);
    expect(out.exactServed).toBe(0); // no exact tile contributed, so nothing to assert against
    expect(out.tiles[0]!.counts).toBeNull();
    assertAssemblyMatchesServed(out);
  });

  it('admits no stand-in over a tile an exact band answers, whatever coverage says', () => {
    // The replica clips stand-ins by coverage rectangles, and an exact band can arrive before its
    // rectangle. The assembly drops a descendant whose drawn tile an exact band answers.
    const exact = band(3, 0n, 4);
    const kid = band(5, 0n, 8); // projects to drawn tile (0,0), the exact band's own tile
    const out = assemble(frame(3, [exact], [kid]));
    expect(out.provisional).toBe(0);
    expect(out.exactDrawn).toBe(4);
    assertAssemblyMatchesServed(out);
  });

  it('bounds a drawn tile by its own density however many deep bands stand in for it', () => {
    // Sixteen five-mark bands four levels down, all under drawn tile 0: the tile's own depth
    // would serve about 16·5/256 = 0.3 marks, so one is drawn, not one per band.
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

  it('carries the membership ordinal of every stand-in mark, indexed and whole alike', () => {
    // A stand-in mark carries the ordinal its band served, so it is not drawn neutral.
    const whole = band(4, 32n, 3);
    whole.membership = {clusters: {ordinals: Uint32Array.from([7, 7, 9]), distinct: Uint32Array.from([7, 9])}};
    const out = assemble(frame(2, [band(2, 0n, 2)], [whole]), [], 'clusters');
    expect(out.provisional).toBe(1);
    // A density-matched prefix takes the prefix of the ordinals, in step with the ids.
    expect([...out.standIn.ordinals]).toEqual([7]);

    // An ancestor stands in through an index list; the ordinals follow the same indices.
    const ancestor: Band = {
      ...band(6, 0n, 3),
      ids: BigUint64Array.from([1n, 2n, 3n]),
      positions: Float32Array.from([10, 4, 12, 4, 18, 4]),
      membership: {clusters: {ordinals: Uint32Array.from([4, 5, 6]), distinct: Uint32Array.from([4, 5, 6])}},
      served: 3
    };
    const indexed = assemble(frame(6, [], [ancestor], {x0: 1, y0: 0, x1: 1, y1: 0}), [], 'clusters');
    expect([...indexed.standIn.ids]).toEqual([1n, 2n]);
    expect([...indexed.standIn.ordinals]).toEqual([4, 5]);
  });

  it('gives a stand-in ordinal 0 for a layer its band never carried', () => {
    const out = assemble(frame(2, [band(2, 0n, 2)], [band(4, 32n, 1)]), [], 'clusters');
    expect(out.provisional).toBe(1);
    expect([...out.standIn.ordinals]).toEqual([0]);
  });

  it('folds a column across exact bands and stand-ins alike', () => {
    // The colour domain and the category ranks are accumulators, so they fold across bands
    // without a concatenated column.
    // The stand-in sits at drawn tile (0,1), which neither exact band answers, so it is kept.
    const out = assemble(frame(2, [band(2, 0n, 2), band(2, 1n, 3)], [band(4, 32n, 1)]), ['w']);
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
        {tile: 0n, visible: 9n, matched: 9n, highlighted: 9n, served: 3n},
        {tile: 1n, visible: 4n, matched: 4n, highlighted: 4n, served: 2n}
      ],
      ids: BigUint64Array.from([1n, 2n, 3n, 4n, 5n]),
      codes: new BigUint64Array(5),
      positions: Float64Array.from([0, 0, 128, 128, 256, 256, 384, 384, 512, 512]),
      world: Float32Array.from([0, 0, 1, 1, 2, 2, 3, 3, 4, 4]),
      scalars: {w: {arrowType: 'u32', values: Uint32Array.from([10, 11, 12, 13, 14])}},
      highlighted: null,
      pointsProjection: 'full',
      subCells: null,
      membership: {},
      artifacts: [],
      artifactsIdentity: null
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
  it('demotes a short band to a stand-in rather than failing the served count', () => {
    // Eviction can truncate a band below `served`. Counted exact, it would make this assertion throw
    // on every paint. Its head draws as a stand-in with no counts, outside the equality's domain.
    const short = band(2, 0n, 2, 3); // holds 2, server said it served 3
    const out = assemble(frame(2, [short]));
    expect(out.exactDrawn).toBe(0);
    expect(out.provisional).toBe(2);
    expect(() => assertAssemblyMatchesServed(out)).not.toThrow();
  });

  it('throws when a provisional tile carries counts', () => {
    const out = assemble(frame(2, [], [band(4, 0n, 2)]));
    out.tiles[0]!.counts = {visible: 1n, matched: 1n, highlighted: 1n, served: 1};
    refused(() => assertAssemblyMatchesServed(out));
  });
});

describe('refreshExact', () => {
  it('folds fresh exact bands in while keeping the stand-ins by reference', () => {
    // The stand-in's marks sit in drawn tile (2,0), which no fresh band covers, so the buffers
    // survive the fold untouched.
    const held = assemble(frame(2, [band(2, 0n, 3)], [band(4, 8n, 2, 2, {cx: 32768, cy: 0})]), ['w']);
    const fresh = [band(2, 0n, 3), band(2, 1n, 2)];

    const out = refreshExact(held, fresh, 7);

    expect(out.version).toBe(7);
    expect(out.exactDrawn).toBe(5);
    expect(out.exactServed).toBe(5);
    expect(out.visibleInView).toBe(15);
    // The stand-in buffers are the same object, so the memoised descriptors and colour buffer stay
    // valid and nothing re-uploads.
    expect(out.standIn).toBe(held.standIn);
    expect(out.provisional).toBe(held.provisional);
    // Non-exact tile entries survive; exact ones are rebuilt from the fresh bands.
    expect(out.tiles.filter((t) => !t.exact)).toEqual(held.tiles.filter((t) => !t.exact));
    expect(out.tiles.filter((t) => t.exact)).toHaveLength(2);
    assertAssemblyMatchesServed(out);
  });

  it('carries the held tiles counted before their points, until their bands arrive', () => {
    const counts = {visible: 30n, matched: 30n, highlighted: 30n, served: 4};
    const held = assemble({...frame(2, [band(2, 0n, 3)]), counted: [{prefix: 1n, counts}, {prefix: 2n, counts}]});
    const counted = (out: typeof held) => out.tiles.filter((t) => !t.exact && t.drawn === 0).map((t) => t.prefix);
    expect(counted(held)).toEqual([1n, 2n]);

    // No fresh band for either: both stay counted.
    expect(counted(refreshExact(held, [band(2, 0n, 3)], 1))).toEqual([1n, 2n]);
    // A band for tile 1 replaces its counted entry rather than adding to it.
    const out = refreshExact(held, [band(2, 0n, 3), band(2, 1n, 4)], 2);
    expect(counted(out)).toEqual([2n]);
    expect(out.tiles.filter((t) => t.prefix === 1n)).toHaveLength(1);
    assertAssemblyMatchesServed(out);
  });

  it('drops an empty band rather than counting a zero-length slot', () => {
    const held = assemble(frame(2, [band(2, 0n, 2)]));
    const out = refreshExact(held, [band(2, 0n, 0, 0), band(2, 1n, 2)], 1);
    expect(out.bands).toHaveLength(1);
    expect(out.exactDrawn).toBe(2);
  });

  it('drops stand-in marks over ground an arriving band now answers exactly', () => {
    // The stand-in's one density-matched mark sits in drawn tile (0,0), and the fold brings an
    // exact band for that tile. Keeping the mark would draw the ground twice, so the fold drops it.
    const held = assemble(frame(2, [], [band(4, 0n, 2)]));
    expect(held.provisional).toBe(1);
    const out = refreshExact(held, [band(2, 0n, 3)], 1);
    expect(out.provisional).toBe(0);
    expect(out.standIn.ids.length).toBe(0);
    assertAssemblyMatchesServed(out);
  });
});

describe('a stand-in column with no value at some points', () => {
  it('keeps each point\'s presence, through an index list, a prefix and a band without the column', () => {
    const heat = (values: number[], present?: number[]): ScalarColumn => ({
      arrowType: 'f64',
      values: Float64Array.from(values),
      ...(present ? {present: Uint8Array.from(present)} : {})
    });
    const whole = {...band(2, 0n, 3), scalars: {heat: heat([1, 2, 3])}};
    const gappy = {...band(2, 1n, 3), scalars: {heat: heat([4, 0, 6], [1, 0, 1])}};
    const bare = {...band(2, 2n, 2), scalars: {}};
    const out = materialiseStandIn(
      [
        {band: whole, indices: null, limit: 2},
        {band: gappy, indices: [2, 1], limit: Infinity},
        {band: bare, indices: null, limit: 1}
      ],
      ['heat']
    );
    const column = out.scalars.heat!;
    const read = Array.from(column.values as Float64Array, (v, i) =>
      !column.present || column.present[i] === 1 ? v : null
    );
    expect(read).toEqual([1, 2, 6, null, null]);
  });
});
