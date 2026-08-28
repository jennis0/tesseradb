import {describe, expect, it} from 'vitest';
import {Field, Float64, List, Table, Uint32, Uint64, Utf8, makeData, makeVector, tableToIPC, vectorFromArray} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';

/**
 * The hull's rings on the wire (contracts §3.2 item 4, `artifact-shapes.md` §9).
 *
 * `hull_x` and `hull_y` are `list<list<uint32>>` — one entry per ring, because a membership that
 * is two separated clouds is two shapes and never one polygon over the gap between them. The
 * decoder descends two levels and **checks** that the two axes agree on the ring count and on
 * each ring's length: they agree by construction, and a decoder that assumes it misdraws silently
 * on the day something else does not.
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
const FLAT = new List(new Field('item', new Uint32(), false));
const TEXTS = new List(new Field('item', new Utf8(), true));

type Rings = number[][] | null;

/**
 * An artifacts-frame body over one row per pair of hull axes — the annotation channel's own shape,
 * `k = 0` and no points frame. The axes are given separately so a test can make them disagree,
 * which the server never does and which is exactly why the decoder must not assume it.
 */
function body(rows: {x: Rings; y: Rings}[], type: {x: unknown; y: unknown} = {x: RINGS, y: RINGS}): Uint8Array {
  const tiles = tableToIPC(new Table({tile: u64([0n]), visible: u64([1n]), matched: u64([1n]), served: u64([0n])}), 'stream');
  const artifacts = tableToIPC(
    new Table({
      layer: vectorFromArray(rows.map(() => 'clusters/x'), new Utf8()),
      tessera_id: u64(rows.map((_, i) => BigInt(i + 1))),
      key: vectorFromArray(rows.map((_, i) => `c-${i}`), new Utf8()),
      masked_count: u64(rows.map(() => 7n)),
      centroid_x: vectorFromArray(rows.map(() => 4), new Float64()),
      centroid_y: vectorFromArray(rows.map(() => 4), new Float64()),
      box_min_x: vectorFromArray(rows.map(() => 0), new Uint32()),
      box_min_y: vectorFromArray(rows.map(() => 0), new Uint32()),
      box_max_x: vectorFromArray(rows.map(() => 9), new Uint32()),
      box_max_y: vectorFromArray(rows.map(() => 9), new Uint32()),
      hull_x: vectorFromArray(rows.map((r) => r.x), type.x as never),
      hull_y: vectorFromArray(rows.map((r) => r.y), type.y as never),
      content: vectorFromArray(rows.map(() => [] as string[]), TEXTS)
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

describe('a hull is a list of rings', () => {
  it('decodes two levels — one entry per ring, the axes zipped inside each', () => {
    const r = decodeViewport(
      body([
        {x: [[0, 3, 3, 0], [6, 9, 9, 6]], y: [[0, 0, 3, 3], [6, 6, 9, 9]]},
        {x: [[1, 2, 1]], y: [[1, 1, 2]]}
      ])
    );
    // Two separated clouds are two rings, and nothing joins them: a flat decode would have read
    // one eight-vertex ring whose fifth edge crosses the ground between the clouds.
    expect(r.artifacts[0]!.hull).toEqual([
      [
        [0, 0],
        [3, 0],
        [3, 3],
        [0, 3]
      ],
      [
        [6, 6],
        [9, 6],
        [9, 9],
        [6, 9]
      ]
    ]);
    expect(r.artifacts[1]!.hull).toEqual([
      [
        [1, 1],
        [2, 1],
        [1, 2]
      ]
    ]);
  });

  it('keeps a degenerate group as the ring the wire sent — one vertex, or two', () => {
    // A one-member group is a ring of one vertex and a two-member group a ring of two
    // (`artifact-shapes.md` §1). Rounding either up to a triangle here would invent an area no
    // member occupies, so the decoder carries what it was sent and the drawing decides.
    const r = decodeViewport(body([{x: [[4], [1, 2], [0, 3, 3]], y: [[4], [1, 1], [0, 0, 3]]}]));
    expect(r.artifacts[0]!.hull!.map((ring) => ring.length)).toEqual([1, 2, 3]);
    expect(r.artifacts[0]!.hull![0]).toEqual([[4, 4]]);
  });

  it('reads a null hull as the layer declaring none, not as an empty shape', () => {
    const r = decodeViewport(body([{x: null, y: null}]));
    expect(r.artifacts[0]!.hull).toBeNull();
    // The box is still there: a null geometry column is a fact about the layer, never about the
    // viewer, and this layer declares a box.
    expect(r.artifacts[0]!.box).toEqual([0, 0, 9, 9]);
  });

  it('reads an artifact with no rings at all as an empty list, and does not confuse it with a null', () => {
    const r = decodeViewport(body([{x: [], y: []}]));
    expect(r.artifacts[0]!.hull).toEqual([]);
  });

  it('refuses a flat hull column — the shape a server older than the rings change sends', () => {
    // The nesting is what makes a single-ring reader fail its downcast rather than concatenate the
    // rings and draw a chord between them. The same downcast in reverse is checked here, at the
    // schema, so the skew is a named refusal and not a shape read one level too shallow.
    expect(() => decodeViewport(body([{x: [0, 3, 3] as never, y: [0, 0, 3] as never}], {x: FLAT, y: FLAT}))).toThrow(
      /hull_x.*List<Uint32>.*list of rings/s
    );
  });

  it('refuses axes that disagree on the ring count', () => {
    expect(() => decodeViewport(body([{x: [[0, 3, 3], [5, 8, 8]], y: [[0, 0, 3]]}]))).toThrow(/disagree on ring count \(2 and 1\)/);
  });

  it('refuses axes that disagree on a ring’s length', () => {
    expect(() => decodeViewport(body([{x: [[0, 3, 3], [5, 8, 8, 5]], y: [[0, 0, 3], [5, 5, 8]]}]))).toThrow(
      /disagree on the length of ring 1 \(4 and 3\)/
    );
  });

  it('refuses one axis null against the other present', () => {
    expect(() => decodeViewport(body([{x: [[0, 3, 3]], y: null}]))).toThrow(/one hull axis is null and the other is not/);
  });
});
