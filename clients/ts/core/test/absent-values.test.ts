import {describe, expect, it} from 'vitest';
import {
  Bool,
  Float32,
  Int32,
  makeData,
  makeVector,
  tableToIPC,
  Table,
  TimestampMicrosecond,
  Uint64,
  Utf8,
  vectorFromArray
} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';
import {bandsOfResult} from '../src/bands.js';
import {hasValue, numericValues, widenDomain} from '../src/encoding.js';
import type {ScalarColumn} from '../src/types.js';

/**
 * A rendered value the server sends as null is no value: the decoder marks it in `present` and
 * nothing downstream reads the zero beside it. A genuine zero stays a value.
 */

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

/** A body whose points arrive in the frames given, served by tiles 0, 1, ... in the counts given. */
function body(points: Table[], served?: bigint[]): Uint8Array {
  const total = BigInt(points.reduce((n, t) => n + t.numRows, 0));
  const counts = served ?? [total];
  const tiles = tableToIPC(
    new Table({
      tile: u64(counts.map((_, i) => BigInt(i))),
      visible: u64(counts),
      matched: u64(counts),
      served: u64(counts),
      highlighted: u64(counts)
    }),
    'stream'
  );
  const trailer = new TextEncoder().encode(
    JSON.stringify({arrow_serialise_ns: 0, flushes: points.length, points: Number(total), stream_us: 0})
  );
  return frame([
    {kind: 1, payload: tiles},
    ...points.map((t) => ({kind: 3, payload: tableToIPC(t, 'stream')})),
    {kind: 4, payload: trailer}
  ]);
}

/** Points `ids`, with every column's values as given; `null` is a null on the wire. */
function points(ids: bigint[], columns: {
  heat: (number | null)[];
  score: (number | null)[];
  count: (bigint | null)[];
  seen: (number | null)[];
  flag: (boolean | null)[];
  note: (string | null)[];
}): Table {
  return new Table({
    mosaica_id: u64(ids),
    code: u64(ids.map(() => 0n)),
    heat: vectorFromArray(columns.heat, new Float32()),
    score: vectorFromArray(columns.score, new Int32()),
    count: vectorFromArray(columns.count, new Uint64()),
    seen: vectorFromArray(columns.seen, new TimestampMicrosecond()),
    flag: vectorFromArray(columns.flag, new Bool()),
    note: vectorFromArray(columns.note, new Utf8())
  });
}

/** Each point's value, `null` where it has none. */
function read(column: ScalarColumn): unknown[] {
  return Array.from(column.values as ArrayLike<unknown>, (v, i) => (hasValue(column, i) ? v : null));
}

describe('an absent rendered value', () => {
  const nulls = points([1n, 2n, 3n], {
    heat: [null, 0, 2.5],
    score: [0, null, -4],
    count: [null, 0n, 9n],
    seen: [0, 5, null],
    flag: [null, false, true],
    note: ['', null, 'x']
  });
  const whole = points([4n], {heat: [1], score: [1], count: [1n], seen: [1], flag: [true], note: ['y']});

  it('decodes to no value, and a genuine zero to a value', () => {
    const r = decodeViewport(body([nulls]));
    expect(read(r.scalars.heat!)).toEqual([null, 0, 2.5]);
    expect(read(r.scalars.score!)).toEqual([0, null, -4]);
    expect(read(r.scalars.count!)).toEqual([null, 0n, 9n]);
    // apache-arrow's builder takes a timestamp in milliseconds.
    expect(read(r.scalars.seen!)).toEqual([0n, 5000n, null]);
    expect(read(r.scalars.flag!)).toEqual([null, false, true]);
    expect(read(r.scalars.note!)).toEqual(['', null, 'x']);
  });

  it('carries no present where every value arrived', () => {
    const r = decodeViewport(body([whole]));
    for (const column of Object.values(r.scalars)) expect(column.present ?? null).toBeNull();
  });

  it('keeps its place across frames, whichever frame holds the nulls', () => {
    for (const order of [[nulls, whole], [whole, nulls]]) {
      const r = decodeViewport(body(order));
      const ids = [...r.ids];
      const heat = read(r.scalars.heat!);
      const byId = Object.fromEntries(ids.map((id, i) => [String(id), heat[i]]));
      expect(byId).toEqual({'1': null, '2': 0, '3': 2.5, '4': 1});
    }
  });

  it('stays no value in the band of the tile that served it', () => {
    // The first tile serves the point with every value, the second the three with nulls.
    const r = decodeViewport(body([whole, nulls], [1n, 3n]));
    const bands = bandsOfResult(r, 1, {identityKey: 'i', contentKey: 'c', capUsed: 10, now: 0});
    expect(bands.map((b) => read(b.scalars.heat!))).toEqual([[1], [null, 0, 2.5]]);
    expect(bands.map((b) => read(b.scalars.flag!))).toEqual([[true], [null, false, true]]);
  });

  it('takes no part in a numeric domain and reads as NaN, never as zero', () => {
    const r = decodeViewport(body([nulls]));
    const heat = numericValues(r.scalars.heat!)!;
    expect(Number.isNaN(heat[0]!)).toBe(true);
    expect(heat[1]).toBe(0);
    const score = r.scalars.score!;
    expect(widenDomain(null, score)).toEqual({min: -4, max: 0});
    const count = numericValues(r.scalars.count!)!;
    expect(Number.isNaN(count[0]!)).toBe(true);
    // A column with a null in every place has no domain to give.
    const none: ScalarColumn = {arrowType: 'f32', values: Float32Array.from([0, 0]), present: Uint8Array.from([0, 0])};
    expect(widenDomain(null, none)).toBeNull();
  });
});
