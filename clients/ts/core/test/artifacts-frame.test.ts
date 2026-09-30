import {describe, expect, it} from 'vitest';
import {Bool, Dictionary, Field, Float64, List, Table, Type, Uint16, Uint32, Uint64, Utf8, makeData, makeVector, tableFromIPC, tableToIPC, vectorFromArray} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';
import {splitFramedStreams} from '../src/frame.js';
import {refused} from './support.js';

/**
 * The artifacts frame: `layer` dictionary-encoded, the fixed columns `layer` through `target` in
 * order, the two shape columns after them and absent when no served layer draws a shape, and the
 * identity projection (`artifact_rows: "identity"`) with the same rows in five columns. Bodies are
 * built with apache-arrow's own writer, so these check the decoder's reading of the layout; the
 * captured goldens check the server's framing.
 */

/** A framed `/v1/viewport` body: `u8 kind, u32 LE length, payload`, repeated. */
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
/** A nullable `uint64` column, as `target` travels. */
const u64Nullable = (values: (bigint | null)[]) => vectorFromArray(values, new Uint64());
const RINGS = new List(new Field('item', new List(new Field('item', new Uint32(), false)), true));
const PARTS = new List(new Field('item', RINGS, true));
const TEXTS = new List(new Field('item', new Utf8(), true));
/** `parent_ids: list<uint64>`: the served parents in this response, ascending. */
const PARENTS = new List(new Field('item', new Uint64(), false));
const LAYER = new Dictionary(new Utf8(), new Uint16());

type Row = {layer: string; id: bigint; rung: number; matched: boolean | null; highlighted?: boolean | null; parentIds?: bigint[]; shape?: number[][][] | null; target?: bigint | null};

const TILES = tableToIPC(new Table({tile: u64([0n]), visible: u64([1n]), matched: u64([1n]), served: u64([0n]), highlighted: u64([1n])}), 'stream');
const TRAILER = new TextEncoder().encode(JSON.stringify({arrow_serialise_ns: 0, flushes: 0, points: 0, stream_us: 0}));

/** The fixed columns, `layer` through `target`, in contract order. */
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
    parent_ids: vectorFromArray(rows.map((r) => r.parentIds ?? []), PARENTS),
    rung: vectorFromArray(rows.map((r) => r.rung), new Uint32()),
    matched: vectorFromArray(rows.map((r) => r.matched), new Bool()),
    // The `tessera_id` of the row this one is attached to, or null.
    target: u64Nullable(rows.map((r) => r.target ?? null))
  };
}

/** A full-projection body; the shape columns trail, and only when `shapes` says a layer draws one. */
function fullBody(rows: Row[], opts: {shapes?: boolean; layerType?: unknown; oneAxis?: boolean; renameRung?: string; oldParent?: boolean; oldNames?: boolean; noTarget?: boolean} = {}): Uint8Array {
  const columns: Record<string, unknown> = fixedColumns(rows, opts.layerType);
  if (opts.renameRung) {
    columns[opts.renameRung] = columns['rung'];
    delete columns['rung'];
  }
  if (opts.noTarget) delete columns['target'];
  if (opts.oldParent) {
    // The old scalar `parent_id` in the list's place.
    columns['parent_id'] = vectorFromArray(rows.map((r) => r.parentIds?.[0] ?? null), new Uint64());
    delete columns['parent_ids'];
  }
  if (opts.shapes) {
    const [x, y] = opts.oldNames ? ['hull_x', 'hull_y'] : ['shape_x', 'shape_y'];
    columns[x] = vectorFromArray(rows.map((r) => (r.shape ? r.shape.map((part) => part.map((ring) => ring.map((v) => v))) : null)), PARTS);
    if (!opts.oneAxis) columns[y] = vectorFromArray(rows.map((r) => (r.shape ? r.shape.map((part) => part.map((ring) => ring.map((v) => v + 1))) : null)), PARTS);
  }
  const artifacts = tableToIPC(new Table(columns as never), 'stream');
  return frame([{kind: 1, payload: TILES}, {kind: 5, payload: artifacts}, {kind: 4, payload: TRAILER}]);
}

/** The identity projection's body: exactly `(layer, tessera_id, rung, matched, highlighted)`. */
function identityBody(rows: Row[]): Uint8Array {
  const artifacts = tableToIPC(
    new Table({
      layer: vectorFromArray(rows.map((r) => r.layer), LAYER),
      tessera_id: u64(rows.map((r) => r.id)),
      rung: vectorFromArray(rows.map((r) => r.rung), new Uint32()),
      matched: vectorFromArray(rows.map((r) => r.matched), new Bool()),
      highlighted: vectorFromArray(rows.map((r) => r.highlighted ?? null), new Bool())
    }),
    'stream'
  );
  return frame([{kind: 1, payload: TILES}, {kind: 5, payload: artifacts}, {kind: 4, payload: TRAILER}]);
}

const ROWS: Row[] = [
  {layer: 'clusters/x', id: 1n, rung: 0, matched: true},
  {layer: 'clusters/x', id: 2n, rung: 1, matched: false, parentIds: [1n]},
  {layer: 'labels/x', id: 3n, rung: 0, matched: null}
];

describe('the dictionary-encoded layer column', () => {
  it('travels as a dictionary on the wire, and reads back as the layer name: apache-arrow resolves it on get()', () => {
    const body = fullBody(ROWS);
    // The body carries a dictionary, so the test covers the encoding the server sends.
    const table = tableFromIPC(splitFramedStreams(body).artifacts!);
    const column = table.getChild('layer')!;
    expect(column.type.typeId).toBe(Type.Dictionary);
    expect((column.type as {indices: {bitWidth: number}}).indices.bitWidth).toBe(16);
    // `.get()` returns the value, not the key.
    expect(column.get(0)).toBe('clusters/x');
    expect(column.get(2)).toBe('labels/x');

    const r = decodeViewport(body);
    expect(r.artifacts.map((a) => a.layer)).toEqual(['clusters/x', 'clusters/x', 'labels/x']);
  });

  it('reads a plain utf8 layer the same way: the encoding moves no information', () => {
    const r = decodeViewport(fullBody(ROWS, {layerType: new Utf8()}));
    expect(r.artifacts.map((a) => a.layer)).toEqual(['clusters/x', 'clusters/x', 'labels/x']);
  });
});

describe('the shape columns trail, and are absent when no served layer draws a shape', () => {
  it('reads an absent pair as no artifact carrying a shape, with every other column intact', () => {
    const r = decodeViewport(fullBody(ROWS));
    expect(r.artifacts.length).toBe(3);
    expect(r.artifacts.every((a) => a.shape === null)).toBe(true);
    // The rest of the row is untouched by the absence: the box is still the box.
    expect(r.artifacts[0]!.box).toEqual([0, 0, 9, 9]);
    expect(r.artifacts[0]!.centroid).toEqual([4, 4]);
    expect(r.artifacts[1]!.parentIds).toEqual([1n]);
  });

  it('reads a present pair after the fixed prefix: per-row null still meaning the layer declares none', () => {
    const rows: Row[] = [
      {...ROWS[0]!, shape: [[[0, 3, 3]], [[6, 9, 9]]]},
      {...ROWS[1]!, shape: [[[1, 2]]]},
      {...ROWS[2]!, shape: null}
    ];
    const body = fullBody(rows, {shapes: true});
    const fields = tableFromIPC(splitFramedStreams(body).artifacts!).schema.fields.map((f) => f.name);
    // The fixed columns, then the two shape columns. `highlighted` is left out of these bodies, so
    // every case exercises an absent bit column.
    expect(fields.slice(0, 15)).toEqual(['layer', 'tessera_id', 'key', 'masked_count', 'centroid_x', 'centroid_y', 'box_min_x', 'box_min_y', 'box_max_x', 'box_max_y', 'content', 'parent_ids', 'rung', 'matched', 'target']);
    expect(fields.slice(15)).toEqual(['shape_x', 'shape_y']);

    const r = decodeViewport(body);
    expect(r.artifacts[0]!.shape).toEqual([
      [
        [
          [0, 1],
          [3, 4],
          [3, 4]
        ]
      ],
      [
        [
          [6, 7],
          [9, 10],
          [9, 10]
        ]
      ]
    ]);
    expect(r.artifacts[1]!.shape).toEqual([
      [
        [
          [1, 2],
          [2, 3]
        ]
      ]
    ]);
    expect(r.artifacts[2]!.shape).toBeNull();
  });

  it('refuses one shape column without the other: the pair travels together by contract', () => {
    refused(() => decodeViewport(fullBody([{...ROWS[0]!, shape: [[[0, 1]]]}], {shapes: true, oneAxis: true})));
  });

  it('refuses the columns under their old names: a server older than the shape columns', () => {
    // Read as having no shapes, a `hull_x` body would draw every cluster as its box.
    refused(() => decodeViewport(fullBody([{...ROWS[0]!, shape: [[[0, 1, 2]]]}], {shapes: true, oldNames: true})));
  });
});

describe('the rung column', () => {
  it('is read as the rung every layer kind is drawn at, whatever the parent links say', () => {
    // A treed layer's rung is the server's chain depth and a levelled layer's its declared level;
    // both are read, not counted from `parent_ids`.
    const r = decodeViewport(fullBody([{layer: 'admin', id: 1n, rung: 0, matched: null}, {layer: 'admin', id: 2n, rung: 2, matched: null, parentIds: [1n]}]));
    expect(r.artifacts.map((a) => a.rung)).toEqual([0, 2]);
  });

  it('refuses a body that still carries `level`: a server older than the rename', () => {
    // Read as 0, a missing rung would draw a whole hierarchy at its coarsest.
    refused(() => decodeViewport(fullBody(ROWS, {renameRung: 'level'})));
  });
});

describe('the parent list', () => {
  it('reads every served parent, in the wire’s ascending order: several on a `dag` layer, none for a root', () => {
    const r = decodeViewport(
      fullBody([
        {layer: 'mesh', id: 1n, rung: 0, matched: null},
        {layer: 'mesh', id: 3n, rung: 0, matched: null},
        {layer: 'mesh', id: 7n, rung: 1, matched: null, parentIds: [1n, 3n]}
      ])
    );
    expect(r.artifacts.map((a) => a.parentIds)).toEqual([[], [], [1n, 3n]]);
  });

  it('refuses a body that still carries the scalar `parent_id`: a server older than the list', () => {
    // Read as having no links, a `parent_id` body would draw a hierarchy flat.
    refused(() => decodeViewport(fullBody(ROWS, {oldParent: true})));
  });

  it('reads the filter bit beside it: true, false, and null where there was no question', () => {
    const r = decodeViewport(fullBody(ROWS));
    expect(r.artifacts.map((a) => a.matched)).toEqual([true, false, null]);
  });
});

describe('the target column', () => {
  it('reads the attachment as an identifier in the same frame, and null for an artifact attached to nothing', () => {
    const r = decodeViewport(fullBody([ROWS[0]!, ROWS[1]!, {...ROWS[2]!, target: 2n}]));
    expect(r.artifacts.map((a) => a.target)).toEqual([null, null, 2n]);
    // The value names a row this response carries.
    expect(r.artifacts.map((a) => a.tesseraId)).toContain(r.artifacts[2]!.target);
    expect(r.artifacts[2]!.target).toBeTypeOf('bigint');
  });

  it('refuses a body carrying no `target` column: a server older than the attachment', () => {
    // Read as nothing attached, such a body would drop every topic label from the map.
    refused(() => decodeViewport(fullBody(ROWS, {noTarget: true})));
  });
});

describe('the identity projection', () => {
  it('decodes the five-column frame to identity rows and no full artifacts', () => {
    const r = decodeViewport(identityBody(ROWS));
    expect(r.artifacts).toEqual([]);
    // `highlighted` is null here, as `matched` is on the third row: the request carried no
    // highlight.
    expect(r.artifactsIdentity).toEqual([
      {layer: 'clusters/x', tesseraId: 1n, rung: 0, matched: true, highlighted: null},
      {layer: 'clusters/x', tesseraId: 2n, rung: 1, matched: false, highlighted: null},
      {layer: 'labels/x', tesseraId: 3n, rung: 0, matched: null, highlighted: null}
    ]);
    // The identifier stays a u64.
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
