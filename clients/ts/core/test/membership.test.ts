import {describe, expect, it} from 'vitest';
import {makeData, makeVector, tableToIPC, Table, Uint64, vectorFromArray} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';
import {bandsOfResult, BandCache, distinctOrdinals} from '../src/bands.js';
import {NO_ORDINAL, SessionArtifactTable} from '../src/artifactTable.js';
import {GRID32_CENTRE, artifactColours} from '../src/palette.js';
import type {Artifact, ViewportResult} from '../src/types.js';

/**
 * The per-point membership column (D12, §5.10): a nullable `u64` named `membership:<layer>`
 * after the render scalars. The decoder hashes it to a response-local index plus the distinct
 * ids — never a scalar, never a `tessera_id` on the palette — and the main thread names the
 * distinct list through the session table and remaps each band as it is built.
 */

/** A framed `/v1/viewport` body: `u8 kind, u32 LE length, payload`, repeated (frame.ts). */
function frame(parts: {kind: number; payload: Uint8Array}[]): Uint8Array {
  const total = parts.reduce((n, p) => n + 5 + p.payload.length, 0);
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let at = 0;
  for (const {kind, payload} of parts) {
    out[at] = kind;
    view.setUint32(at + 1, payload.length, true);
    out.set(payload, at + 5);
    at += 5 + payload.length;
  }
  return out;
}

function u64(values: bigint[]) {
  return makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
}

/** A nullable u64 vector: `null` entries are nulls on the wire. */
function u64n(values: (bigint | null)[]) {
  return vectorFromArray(values, new Uint64());
}

function body(points: Table[], tilesServed: bigint[]): Uint8Array {
  const total = points.reduce((n, t) => n + t.numRows, 0);
  const tiles = tableToIPC(
    new Table({
      tile: u64(tilesServed.map((_, i) => BigInt(i))),
      visible: u64(tilesServed.map((s) => s + 5n)),
      matched: u64(tilesServed.map((s) => s + 5n)),
      served: u64(tilesServed)
    }),
    'stream'
  );
  const trailer = new TextEncoder().encode(JSON.stringify({arrow_serialise_ns: 0, flushes: points.length, points: total, stream_us: 0}));
  return frame([{kind: 1, payload: tiles}, ...points.map((t) => ({kind: 3, payload: tableToIPC(t, 'stream')})), {kind: 4, payload: trailer}]);
}

describe('the membership column in the decoder', () => {
  it('is skipped as a scalar and hashed to a local index with the distinct-id list', () => {
    const r = decodeViewport(
      body(
        [
          new Table({
            tessera_id: u64([1n, 2n, 3n, 4n]),
            code: u64([0n, 0n, 0n, 0n]),
            w: vectorFromArray(Uint32Array.from([10, 11, 12, 13])),
            'membership:clusters/x': u64n([5n, null, 2n ** 63n + 7n, 5n])
          })
        ],
        [4n]
      )
    );
    expect(Object.keys(r.scalars)).toEqual(['w']);
    const m = r.membership['clusters/x']!;
    // Local indices from 1 in first-seen order; 0 for null; a repeated id shares its index.
    expect([...m.index]).toEqual([1, 0, 2, 1]);
    expect([...m.ids]).toEqual([5n, 2n ** 63n + 7n]);
    expect(m.index).toBeInstanceOf(Uint16Array);
  });

  it('hashes across frames as one response, so an id seen in two frames has one local index', () => {
    const r = decodeViewport(
      body(
        [
          new Table({tessera_id: u64([1n, 2n]), code: u64([0n, 0n]), 'membership:l': u64n([9n, null])}),
          new Table({tessera_id: u64([3n, 4n]), code: u64([0n, 0n]), 'membership:l': u64n([8n, 9n])})
        ],
        [2n, 2n]
      )
    );
    expect([...r.membership['l']!.index]).toEqual([1, 0, 2, 1]);
    expect([...r.membership['l']!.ids]).toEqual([9n, 8n]);
  });

  it('carries one column per layer, and none where no layer was served', () => {
    const two = decodeViewport(
      body([new Table({tessera_id: u64([1n]), code: u64([0n]), 'membership:a': u64n([1n]), 'membership:b': u64n([null])})], [1n])
    );
    expect(Object.keys(two.membership).sort()).toEqual(['a', 'b']);
    expect([...two.membership['b']!.index]).toEqual([0]);
    const none = decodeViewport(body([new Table({tessera_id: u64([1n]), code: u64([0n])})], [1n]));
    expect(none.membership).toEqual({});
  });

  it('widens the index to u32 past 65,535 points, and grows its hash past a few thousand distinct', () => {
    const n = 70_000;
    const ids = Array.from({length: n}, (_, i) => BigInt(i + 1));
    // 9,000 distinct artifacts, above the hash's initial half-load.
    const members = Array.from({length: n}, (_, i) => BigInt(1_000_000 + (i % 9_000)));
    const r = decodeViewport(body([new Table({tessera_id: u64(ids), code: u64(ids.map(() => 0n)), 'membership:l': u64n(members)})], [BigInt(n)]));
    const m = r.membership['l']!;
    expect(m.index).toBeInstanceOf(Uint32Array);
    expect(m.ids.length).toBe(9_000);
    for (let i = 0; i < n; i += 977) expect(m.ids[m.index[i]! - 1]).toBe(members[i]);
  });
});

const artifact = (id: bigint, parentId: bigint | null = null, layer = 'l', rung = 0): Artifact => ({
  layer,
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 1n,
  centroid: null,
  box: null,
  hull: null,
  content: [],
  parentId,
  rung
});

/** A response of `tiles.length` tiles, each of its own served count, with a membership column. */
function result(tiles: number[], local: number[], ids: bigint[], artifacts: Artifact[]): ViewportResult {
  const n = tiles.reduce((a, b) => a + b, 0);
  return {
    tiles: tiles.map((served, i) => ({tile: BigInt(i), visible: BigInt(served), matched: BigInt(served), served: BigInt(served)})),
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    codes: new BigUint64Array(n),
    positions: new Float64Array(n * 2),
    world: new Float32Array(n * 2),
    scalars: {},
    membership: {l: {index: Uint16Array.from(local), ids: BigUint64Array.from(ids)}},
    subCells: null,
    artifacts,
    artifactsIdentity: null
  };
}

const meta = (table: SessionArtifactTable) => ({identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0, table});

describe('naming on the main thread', () => {
  it('remaps each band from local index to session ordinal, and takes one reference per distinct ordinal per band', () => {
    const table = new SessionArtifactTable();
    // Two tiles of two points; ids 10 and 20 as locals 1 and 2; parent link 20 → 10 in the frame.
    const bands = bandsOfResult(result([2, 2], [1, 1, 2, 0], [10n, 20n], [artifact(10n), artifact(20n, 10n)]), 3, meta(table));
    const o10 = table.ordinalOf('l', 10n);
    const o20 = table.ordinalOf('l', 20n);
    expect(o10).toBeGreaterThan(NO_ORDINAL);
    expect(o20).toBeGreaterThan(NO_ORDINAL);
    expect([...bands[0]!.membership['l']!.ordinals]).toEqual([o10, o10]);
    expect([...bands[1]!.membership['l']!.ordinals]).toEqual([o20, 0]);
    expect([...bands[0]!.membership['l']!.distinct]).toEqual([o10]);
    expect([...bands[1]!.membership['l']!.distinct]).toEqual([o20]);
    // The parent link came from the same response's artifacts frame.
    expect(table.entry(o20)?.parentOrdinal).toBe(o10);
    // One reference per band that carries it; the response's own temporary ones are gone.
    table.release(bands[0]!.membership['l']!.distinct);
    expect(table.ordinalOf('l', 10n)).toBe(NO_ORDINAL);
    expect(table.ordinalOf('l', 20n)).toBe(o20);
    // The ledger counts the column.
    expect(bands[0]!.bytes).toBe(2 * 8 + 2 * 8 + 2 * 4 + 1 * 4);
  });

  it('names the same artifact with the same ordinal across two responses', () => {
    const table = new SessionArtifactTable();
    const first = bandsOfResult(result([1], [1], [10n], [artifact(10n)]), 3, meta(table));
    // A second response lists the id second: its local index differs, its ordinal does not.
    const second = bandsOfResult(result([2], [2, 1], [99n, 10n], [artifact(99n), artifact(10n)]), 3, meta(table));
    expect(second[0]!.membership['l']!.ordinals[0]).toBe(first[0]!.membership['l']!.ordinals[0]);
    expect(table.live).toBe(2);
  });

  it('names nothing without a table, and carries no column', () => {
    const bands = bandsOfResult(result([1], [1], [10n], [artifact(10n)]), 3, {identityKey: 'ik', contentKey: 'ck', capUsed: 500, now: 0});
    expect(bands[0]!.membership).toEqual({});
  });
});

describe('the cache releases what a band held', () => {
  it('on replacement, truncation and an identity drop — and carries a layer over a refetch that did not name it', () => {
    const table = new SessionArtifactTable();
    const cache = new BandCache(1e9, table);
    const [a] = bandsOfResult(result([4], [1, 1, 2, 2], [10n, 20n], [artifact(10n), artifact(20n)]), 3, meta(table));
    cache.put(a!);
    expect(table.live).toBe(2);

    // The same tile refetched for another layer's column: same set, same content key. The `l`
    // column and its references carry over; the new layer's references are added.
    const refetched = bandsOfResult(result([4], [0, 0, 0, 0], [], []), 3, meta(table))[0]!;
    refetched.membership = {m: {ordinals: Uint32Array.from([0, 0, 0, 0]), distinct: new Uint32Array(0)}};
    cache.put(refetched);
    expect(cache.get(3, 0n)!.membership['l']).toBe(a!.membership['l']);
    expect(table.live).toBe(2);

    // Truncated to its head: the distinct list shrinks and the tail's ordinal is released.
    const tiny = new BandCache(1, table);
    const [b] = bandsOfResult(result([4], [1, 1, 2, 2], [10n, 20n], [artifact(10n), artifact(20n)]), 3, meta(table));
    tiny.put(b!);
    tiny.evict({depth: 3, prefix: 0n});
    const kept = tiny.get(3, 0n)!;
    expect(kept.ids.length).toBe(2);
    expect([...kept.membership['l']!.distinct]).toEqual([table.ordinalOf('l', 10n)]);
    expect(table.entry(b!.membership['l']!.distinct[1]!)).not.toBeNull(); // still held by `cache`

    // A different principal: everything released.
    cache.dropIdentity();
    tiny.dropIdentity();
    expect(table.live).toBe(0);
  });

  it('distinctOrdinals lists each non-zero ordinal once, ascending', () => {
    expect([...distinctOrdinals(Uint32Array.from([3, 0, 1, 3, 1]))]).toEqual([1, 3]);
  });
});

describe('the membership golden (captured against the notebook layer, the layer named with points)', () => {
  // An r44 capture (2026-08-28; `artifacts.client.test.ts` says how). Everything above covers the
  // column against bodies this test file builds; this is the one check that the column and the
  // artifacts frame agree in a body the server actually sent.
  it('names members in the same response’s artifacts frame, and several artifacts with different geometry', () => {
    const {readFileSync} = require('node:fs') as typeof import('node:fs');
    const {join} = require('node:path') as typeof import('node:path');
    const r = decodeViewport(new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', 'viewport-membership.bin'))));
    expect(r.artifacts.length).toBeGreaterThanOrEqual(3);
    const centroids = new Set(r.artifacts.map((a) => a.centroid?.join(',')));
    expect(centroids.size).toBe(r.artifacts.length);
    const layers = Object.keys(r.membership);
    expect(layers.length).toBe(1);
    const m = r.membership[layers[0]!]!;
    expect(m.index.length).toBe(r.ids.length);
    let named = 0;
    for (let i = 0; i < m.index.length; i++) if (m.index[i] !== 0) named++;
    expect(named).toBeGreaterThan(0);
    const servedIds = new Set(r.artifacts.map((a) => a.tesseraId));
    for (const id of m.ids) expect(servedIds.has(id)).toBe(true);
    // Every layer the column names is a layer in the artifacts frame.
    expect(r.artifacts.every((a) => a.layer === layers[0])).toBe(true);
  });
});

describe('a band is coloured by the response that carried it (§5.10)', () => {
  /** The same artifact, with a centroid — what a positional colour is a function of. */
  const placed = (id: bigint, dx: number, parentId: bigint | null = null, rung = 0): Artifact => ({
    ...artifact(id, parentId, 'l', rung),
    centroid: [GRID32_CENTRE + dx, GRID32_CENTRE]
  });

  it('takes each artifact’s centroid from the frame that named it, the point path’s included', () => {
    const table = new SessionArtifactTable();
    bandsOfResult(result([2], [1, 1], [10n], [placed(10n, 1e9)]), 3, meta(table));
    const parent = table.ordinalOf('l', 10n);
    expect(table.entry(parent)!.centroid).toEqual([GRID32_CENTRE + 1e9, GRID32_CENTRE]);
    // Every live ordinal is colourable, and the colour is the centroid's.
    const colours = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      'positional'
    );
    expect(colours.get(parent)).toBeDefined();
  });

  it('keeps a coarse band coloured when a finer response moves the cut under it', () => {
    const table = new SessionArtifactTable();
    // The coarse cut: one parent, and a band whose points belong to it.
    const coarse = bandsOfResult(result([2], [1, 1], [10n], [placed(10n, 1e9)]), 3, meta(table));
    const parent = table.ordinalOf('l', 10n);
    // A zoom in. The point response's own artifacts frame carries the children — the debounced
    // `k = 0` channel is still two hundred milliseconds behind on the coarse cut.
    const fine = bandsOfResult(
      result([2], [1, 2], [20n, 21n], [placed(20n, 9e8, 10n, 1), placed(21n, 11e8, 10n, 1)]),
      3,
      meta(table)
    );
    const children = [table.ordinalOf('l', 20n), table.ordinalOf('l', 21n)];
    expect(children.every((o) => o !== NO_ORDINAL)).toBe(true);

    const colours = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      'positional'
    );
    // Every ordinal on screen — the coarse band's and the finer band's alike — resolves to
    // something coloured. Nothing draws neutral, which is the banding the owner saw.
    const onScreen = [...coarse[0]!.membership['l']!.distinct, ...fine[0]!.membership['l']!.distinct];
    expect(onScreen.length).toBe(3);
    for (const ordinal of onScreen) expect(table.resolve(ordinal, colours)).not.toBe(NO_ORDINAL);
    // The finer band's points wear their own artifacts' colours, not the parent's.
    expect(colours.get(children[0]!)).not.toEqual(colours.get(parent));

    // Against the finer cut's served set alone — which is what the lookup texture used to walk
    // to — the coarse band's ordinal resolves to nothing, and its points drew grey.
    expect(table.resolve(parent, new Set(children))).toBe(NO_ORDINAL);
    // And a level chosen coarser still walks up: the children colour as their parent.
    expect(table.resolve(children[0]!, colours, 0)).toBe(parent);
  });
});
