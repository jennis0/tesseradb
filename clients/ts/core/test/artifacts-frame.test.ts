import {describe, expect, it} from 'vitest';
import {Bool, Dictionary, Field, Float64, List, Table, Type, Uint16, Uint32, Uint64, Utf8, makeData, makeVector, tableFromIPC, tableToIPC, vectorFromArray} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';
import {splitFramedStreams} from '../src/frame.js';

/**
 * The artifacts frame as contracts §3.2 r43 cuts it: `layer` dictionary-encoded, the fourteen
 * fixed columns `layer` through `matched` at their positions, the two hull columns trailing and
 * **absent from the schema** when no served layer declares a hull, `level` renamed and re-meant as
 * `rung`, and the identity projection (`artifact_rows: "identity"`) — the same rows in four
 * columns. Every body here is assembled with apache-arrow's own writer, so what these tests pin is
 * the decoder's reading of the layout, not the server's framing; the captured goldens cover that.
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

const u64 = (values: bigint[]) => makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
const RINGS = new List(new Field('item', new List(new Field('item', new Uint32(), false)), true));
const TEXTS = new List(new Field('item', new Utf8(), true));
const LAYER = new Dictionary(new Utf8(), new Uint16());

type Row = {layer: string; id: bigint; rung: number; matched: boolean | null; parentId?: bigint | null; hull?: number[][] | null};

const TILES = tableToIPC(new Table({tile: u64([0n]), visible: u64([1n]), matched: u64([1n]), served: u64([0n])}), 'stream');
const TRAILER = new TextEncoder().encode(JSON.stringify({arrow_serialise_ns: 0, flushes: 0, points: 0, stream_us: 0}));

/** The fixed fourteen columns, `layer` through `matched`, in contract order. */
function fixedColumns(rows: Row[], layerType: unknown = LAYER) {
  return {
    layer: vectorFromArray(rows.map((r) => r.layer), layerType as never),
    tessera_id: u64(rows.map((r) => r.id)),
    key: vectorFromArray(rows.map((r) => `c-${r.id}`), new Utf8()),
    masked_count: u64(rows.map(() => 7n)),
    centroid_x: vectorFromArray(rows.map(() => 4), new Float64()),
    centroid_y: vectorFromArray(rows.map(() => 4), new Float64()),
    box_min_x: vectorFromArray(rows.map(() => 0), new Uint32()),
    box_min_y: vectorFromArray(rows.map(() => 0), new Uint32()),
    box_max_x: vectorFromArray(rows.map(() => 9), new Uint32()),
    box_max_y: vectorFromArray(rows.map(() => 9), new Uint32()),
    content: vectorFromArray(rows.map(() => [] as string[]), TEXTS),
    parent_id: vectorFromArray(rows.map((r) => r.parentId ?? null), new Uint64()),
    rung: vectorFromArray(rows.map((r) => r.rung), new Uint32()),
    matched: vectorFromArray(rows.map((r) => r.matched), new Bool())
  };
}

/** A full-projection body; the hull columns trail, and only when `hulls` says a layer declares one. */
function fullBody(rows: Row[], opts: {hulls?: boolean; layerType?: unknown; oneAxis?: boolean; renameRung?: string} = {}): Uint8Array {
  const columns: Record<string, unknown> = fixedColumns(rows, opts.layerType);
  if (opts.renameRung) {
    columns[opts.renameRung] = columns['rung'];
    delete columns['rung'];
  }
  if (opts.hulls) {
    columns['hull_x'] = vectorFromArray(rows.map((r) => (r.hull ? r.hull.map((ring) => ring.map((v) => v)) : null)), RINGS);
    if (!opts.oneAxis) columns['hull_y'] = vectorFromArray(rows.map((r) => (r.hull ? r.hull.map((ring) => ring.map((v) => v + 1)) : null)), RINGS);
  }
  const artifacts = tableToIPC(new Table(columns as never), 'stream');
  return frame([{kind: 1, payload: TILES}, {kind: 5, payload: artifacts}, {kind: 4, payload: TRAILER}]);
}

/** The identity projection's body: exactly `(layer, tessera_id, rung, matched)`. */
function identityBody(rows: Row[]): Uint8Array {
  const artifacts = tableToIPC(
    new Table({
      layer: vectorFromArray(rows.map((r) => r.layer), LAYER),
      tessera_id: u64(rows.map((r) => r.id)),
      rung: vectorFromArray(rows.map((r) => r.rung), new Uint32()),
      matched: vectorFromArray(rows.map((r) => r.matched), new Bool())
    }),
    'stream'
  );
  return frame([{kind: 1, payload: TILES}, {kind: 5, payload: artifacts}, {kind: 4, payload: TRAILER}]);
}

const ROWS: Row[] = [
  {layer: 'clusters/x', id: 1n, rung: 0, matched: true},
  {layer: 'clusters/x', id: 2n, rung: 1, matched: false, parentId: 1n},
  {layer: 'labels/x', id: 3n, rung: 0, matched: null}
];

describe('the dictionary-encoded layer column', () => {
  it('travels as a dictionary on the wire, and reads back as the layer name — apache-arrow resolves it on get()', () => {
    const body = fullBody(ROWS);
    // First, that the body really carries a dictionary: a test that built a plain utf8 column by
    // mistake would prove nothing about the encoding the server sends.
    const table = tableFromIPC(splitFramedStreams(body).artifacts!);
    const column = table.getChild('layer')!;
    expect(column.type.typeId).toBe(Type.Dictionary);
    expect((column.type as {indices: {bitWidth: number}}).indices.bitWidth).toBe(16);
    // Then, that `.get()` hands back the value and never the key — the reading the decoder relies on.
    expect(column.get(0)).toBe('clusters/x');
    expect(column.get(2)).toBe('labels/x');

    const r = decodeViewport(body);
    expect(r.artifacts.map((a) => a.layer)).toEqual(['clusters/x', 'clusters/x', 'labels/x']);
  });

  it('reads a plain utf8 layer the same way — the encoding moves no information', () => {
    const r = decodeViewport(fullBody(ROWS, {layerType: new Utf8()}));
    expect(r.artifacts.map((a) => a.layer)).toEqual(['clusters/x', 'clusters/x', 'labels/x']);
  });
});

describe('the hull columns trail, and are absent when no served layer declares a hull', () => {
  it('reads an absent pair as no artifact carrying a hull, with every other column intact', () => {
    const r = decodeViewport(fullBody(ROWS));
    expect(r.artifacts.length).toBe(3);
    expect(r.artifacts.every((a) => a.hull === null)).toBe(true);
    // The rest of the row is untouched by the absence: the box is still the box.
    expect(r.artifacts[0]!.box).toEqual([0, 0, 9, 9]);
    expect(r.artifacts[0]!.centroid).toEqual([4, 4]);
    expect(r.artifacts[1]!.parentId).toBe(1n);
  });

  it('reads a present pair after the fixed prefix — per-row null still meaning the layer declares none', () => {
    const rows: Row[] = [
      {...ROWS[0]!, hull: [[0, 3, 3], [6, 9, 9]]},
      {...ROWS[1]!, hull: [[1, 2]]},
      {...ROWS[2]!, hull: null}
    ];
    const body = fullBody(rows, {hulls: true});
    const fields = tableFromIPC(splitFramedStreams(body).artifacts!).schema.fields.map((f) => f.name);
    // The layout the body carries is the contract's: fourteen fixed, then the two hull columns.
    expect(fields.slice(0, 14)).toEqual(['layer', 'tessera_id', 'key', 'masked_count', 'centroid_x', 'centroid_y', 'box_min_x', 'box_min_y', 'box_max_x', 'box_max_y', 'content', 'parent_id', 'rung', 'matched']);
    expect(fields.slice(14)).toEqual(['hull_x', 'hull_y']);

    const r = decodeViewport(body);
    expect(r.artifacts[0]!.hull).toEqual([
      [
        [0, 1],
        [3, 4],
        [3, 4]
      ],
      [
        [6, 7],
        [9, 10],
        [9, 10]
      ]
    ]);
    expect(r.artifacts[1]!.hull).toEqual([
      [
        [1, 2],
        [2, 3]
      ]
    ]);
    expect(r.artifacts[2]!.hull).toBeNull();
  });

  it('refuses one hull column without the other — the pair travels together by contract', () => {
    expect(() => decodeViewport(fullBody([{...ROWS[0]!, hull: [[0, 1]]}], {hulls: true, oneAxis: true}))).toThrow(/one hull column and not the other/);
  });
});

describe('the rung column', () => {
  it('is read as the rung every layer kind is drawn at, whatever the parent links say', () => {
    // A treed layer's rung is the response-local chain depth, computed server-side; a levelled
    // layer's is its declared level. Both are simply read here — never counted from `parent_id`.
    const r = decodeViewport(fullBody([{layer: 'admin', id: 1n, rung: 0, matched: null}, {layer: 'admin', id: 2n, rung: 2, matched: null, parentId: 1n}]));
    expect(r.artifacts.map((a) => a.rung)).toEqual([0, 2]);
  });

  it('refuses a body that still carries `level` — a server older than the rename', () => {
    // Reading a missing rung as 0 would draw a whole hierarchy at its coarsest and look like data.
    expect(() => decodeViewport(fullBody(ROWS, {renameRung: 'level'}))).toThrow(/no `rung` column/);
  });

  it('reads the filter bit beside it: true, false, and null where there was no question', () => {
    const r = decodeViewport(fullBody(ROWS));
    expect(r.artifacts.map((a) => a.matched)).toEqual([true, false, null]);
  });
});

describe('the identity projection', () => {
  it('decodes the four-column frame to identity rows and no full artifacts', () => {
    const r = decodeViewport(identityBody(ROWS));
    expect(r.artifacts).toEqual([]);
    expect(r.artifactsIdentity).toEqual([
      {layer: 'clusters/x', tesseraId: 1n, rung: 0, matched: true},
      {layer: 'clusters/x', tesseraId: 2n, rung: 1, matched: false},
      {layer: 'labels/x', tesseraId: 3n, rung: 0, matched: null}
    ]);
    // The identifier stays a u64: it is what the caller resolves its store by.
    expect(r.artifactsIdentity![0]!.tesseraId).toBeTypeOf('bigint');
  });

  it('is read off the schema, not the request: a full frame yields no identity rows', () => {
    const r = decodeViewport(fullBody(ROWS));
    expect(r.artifactsIdentity).toBeNull();
    expect(r.artifacts.length).toBe(3);
  });

  it('is null where the response carried no artifacts frame at all', () => {
    const r = decodeViewport(frame([{kind: 1, payload: TILES}, {kind: 4, payload: TRAILER}]));
    expect(r.artifactsIdentity).toBeNull();
    expect(r.artifacts).toEqual([]);
  });
});
