import {describe, expect, it} from 'vitest';
import {Bool, Dictionary, Field, Float64, List, Table, Uint16, Uint32, Uint64, Utf8, makeData, makeVector, tableToIPC, vectorFromArray} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';
import {refused} from './support.js';

/**
 * The shape on the wire. `shape_x` and `shape_y` are `list<list<list<uint32>>>` (parts, rings,
 * vertices) because a hole and a second part draw differently: two separated clouds are two parts,
 * and an enclave is a hole of its part. The decoder checks that the two axes agree at every level.
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
const RINGS = new List(new Field('item', new List(new Field('item', new Uint32(), false)), true));
const PARTS = new List(new Field('item', RINGS, true));
const FLAT = new List(new Field('item', new Uint32(), false));
const TEXTS = new List(new Field('item', new Utf8(), true));
/** `parent_ids: list<uint64>`: the served parents in this response, ascending. */
const PARENTS = new List(new Field('item', new Uint64(), false));

type Parts = number[][][] | null;

/**
 * An artifacts-frame body with one row per pair of shape axes, as the annotation channel's `k = 0`
 * request gets: no points frame, the fixed columns, then the two shape columns. The axes are given
 * separately so a test can make them disagree.
 */
function body(rows: {x: Parts; y: Parts}[], type: {x: unknown; y: unknown} = {x: PARTS, y: PARTS}): Uint8Array {
  const tiles = tableToIPC(new Table({tile: u64([0n]), visible: u64([1n]), matched: u64([1n]), served: u64([0n]), highlighted: u64([1n])}), 'stream');
  const artifacts = tableToIPC(
    new Table({
      layer: vectorFromArray(rows.map(() => 'clusters/x'), new Dictionary(new Utf8(), new Uint16())),
      tessera_id: u64(rows.map((_, i) => BigInt(i + 1))),
      key: vectorFromArray(rows.map((_, i) => `c-${i}`), new Utf8()),
      masked_count: u64(rows.map(() => 7n)),
      centroid_x: vectorFromArray(rows.map(() => 4), new Float64()),
      centroid_y: vectorFromArray(rows.map(() => 4), new Float64()),
      box_min_x: vectorFromArray(rows.map(() => 0), new Uint32()),
      box_min_y: vectorFromArray(rows.map(() => 0), new Uint32()),
      box_max_x: vectorFromArray(rows.map(() => 9), new Uint32()),
      box_max_y: vectorFromArray(rows.map(() => 9), new Uint32()),
      content: vectorFromArray(rows.map(() => [] as string[]), TEXTS),
      parent_ids: vectorFromArray(rows.map(() => [] as bigint[]), PARENTS),
      // Required: the decoder refuses a body without it.
      rung: vectorFromArray(rows.map(() => 0), new Uint32()),
      matched: vectorFromArray(rows.map(() => null), new Bool()),
      // Required, and all null: these rows are a clustering, attached to nothing.
      target: vectorFromArray(rows.map(() => null as bigint | null), new Uint64()),
      shape_x: vectorFromArray(rows.map((r) => r.x), type.x as never),
      shape_y: vectorFromArray(rows.map((r) => r.y), type.y as never)
    }),
    'stream'
  );
  const trailer = new TextEncoder().encode(JSON.stringify({arrow_serialise_ns: 0, flushes: 0, points: 0, stream_us: 0}));
  return frame([
    {kind: 1, payload: tiles},
    {kind: 5, payload: artifacts},
    {kind: 4, payload: trailer}
  ]);
}

describe('a shape is parts of rings', () => {
  it('decodes three levels: parts, then rings, the axes zipped inside each', () => {
    const r = decodeViewport(
      body([
        {x: [[[0, 3, 3, 0]], [[6, 9, 9, 6]]], y: [[[0, 0, 3, 3]], [[6, 6, 9, 9]]]},
        {x: [[[1, 2, 1]]], y: [[[1, 1, 2]]]}
      ])
    );
    // Two separated clouds are two parts; a flat decode would read one ring whose fifth edge crosses
    // the gap.
    expect(r.artifacts[0]!.shape).toEqual([
      [
        [
          [0, 0],
          [3, 0],
          [3, 3],
          [0, 3]
        ]
      ],
      [
        [
          [6, 6],
          [9, 6],
          [9, 9],
          [6, 9]
        ]
      ]
    ]);
    expect(r.artifacts[1]!.shape).toEqual([
      [
        [
          [1, 1],
          [2, 1],
          [1, 2]
        ]
      ]
    ]);
  });

  it('keeps a hole with its part: the second ring of a part is not a second shape', () => {
    const r = decodeViewport(body([{x: [[[0, 9, 9, 0], [3, 6, 6, 3]]], y: [[[0, 0, 9, 9], [3, 3, 6, 6]]]}]));
    expect(r.artifacts[0]!.shape!.length).toBe(1);
    expect(r.artifacts[0]!.shape![0]!.map((ring) => ring.length)).toEqual([4, 4]);
    expect(r.artifacts[0]!.shape![0]![1]).toEqual([
      [3, 3],
      [6, 3],
      [6, 6],
      [3, 6]
    ]);
  });

  it('keeps a degenerate group as the ring the wire sent: one vertex, or two', () => {
    // A one-member group is a ring of one vertex and a two-member group a ring of two. The decoder
    // carries what it was sent and does not invent an area.
    const r = decodeViewport(body([{x: [[[4]], [[1, 2]], [[0, 3, 3]]], y: [[[4]], [[1, 1]], [[0, 0, 3]]]}]));
    expect(r.artifacts[0]!.shape!.map((part) => part[0]!.length)).toEqual([1, 2, 3]);
    expect(r.artifacts[0]!.shape![0]).toEqual([[[4, 4]]]);
  });

  it('reads a null shape as the layer drawing none, not as an empty shape', () => {
    const r = decodeViewport(body([{x: null, y: null}]));
    expect(r.artifacts[0]!.shape).toBeNull();
    // The box is still there: a null geometry column is about the layer, and this layer declares a
    // box.
    expect(r.artifacts[0]!.box).toEqual([0, 0, 9, 9]);
  });

  it('reads an artifact with no parts at all as an empty list, and does not confuse it with a null', () => {
    const r = decodeViewport(body([{x: [], y: []}]));
    expect(r.artifacts[0]!.shape).toEqual([]);
  });

  it('refuses a two-level shape column: the rings of vertices a server older than the shape columns sends', () => {
    // A column one level too shallow is refused at the schema rather than read as parts.
    refused(() => decodeViewport(body([{x: [[0, 3, 3]] as never, y: [[0, 0, 3]] as never}], {x: RINGS, y: RINGS})));
    refused(() => decodeViewport(body([{x: [0, 3, 3] as never, y: [0, 0, 3] as never}], {x: FLAT, y: FLAT})));
  });

  it('refuses axes that disagree on the part count', () => {
    refused(() => decodeViewport(body([{x: [[[0, 3, 3]], [[5, 8, 8]]], y: [[[0, 0, 3]]]}])));
  });

  it('refuses axes that disagree on a part’s ring count', () => {
    refused(() => decodeViewport(body([{x: [[[0, 9, 9], [3, 6, 6]]], y: [[[0, 0, 9]]]}])));
  });

  it('refuses axes that disagree on a ring’s length', () => {
    refused(() => decodeViewport(body([{x: [[[0, 3, 3]], [[5, 8, 8, 5]]], y: [[[0, 0, 3]], [[5, 5, 8]]]}])));
  });

  it('refuses one axis null against the other present', () => {
    refused(() => decodeViewport(body([{x: [[[0, 3, 3]]], y: null}])));
  });
});
