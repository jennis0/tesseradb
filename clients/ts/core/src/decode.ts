import {tableFromIPC, Type, type DataType, type Table, type Vector} from 'apache-arrow';
import {CELLS_PER_WORLD_UNIT} from './coords.js';
import {splitFramedStreams} from './frame.js';
import type {Artifact, ScalarColumn, SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

/**
 * One declared-scalar column, as the type the manifest declared plus the buffer Arrow already
 * holds.
 *
 * **`toArray()`, not a spread.** Arrow's numeric children are typed arrays already; `[...child]`
 * boxes every element, which at 5 × 10⁴ marks across the wide fixture's eighteen columns is close
 * to a million throwaway heap objects per response. `bool` and `utf8` have no typed form — a
 * bitmap and an offset table respectively — so those two, and only those two, materialise.
 *
 * **A type the manifest cannot declare is a decoder bug, not a column to skip.** Silently dropping
 * it would shift nothing (the map is keyed by name) but would make the column vanish from the
 * legend with no error, so it throws instead. The thirteen arms mirror
 * `tessera_wire::payload::ScalarColumn`; the two must be changed together.
 */
function scalarColumn(name: string, vector: Vector<DataType>): ScalarColumn {
  const type = vector.type;
  switch (type.typeId) {
    case Type.Bool:
      return {arrowType: 'bool', values: [...vector] as boolean[]};
    case Type.Utf8:
      return {arrowType: 'utf8', values: [...vector] as string[]};
    case Type.Timestamp:
      return {arrowType: 'timestamp_us', values: vector.toArray() as BigInt64Array};
    case Type.Int: {
      // `Int` covers all eight widths; the bit width and signedness are on the type, not the id.
      const {bitWidth, isSigned} = type as unknown as {bitWidth: number; isSigned: boolean};
      const key = `${isSigned ? 'i' : 'u'}${bitWidth}`;
      switch (key) {
        case 'u8':
          return {arrowType: 'u8', values: vector.toArray() as Uint8Array};
        case 'u16':
          return {arrowType: 'u16', values: vector.toArray() as Uint16Array};
        case 'u32':
          return {arrowType: 'u32', values: vector.toArray() as Uint32Array};
        case 'u64':
          return {arrowType: 'u64', values: vector.toArray() as BigUint64Array};
        case 'i8':
          return {arrowType: 'i8', values: vector.toArray() as Int8Array};
        case 'i16':
          return {arrowType: 'i16', values: vector.toArray() as Int16Array};
        case 'i32':
          return {arrowType: 'i32', values: vector.toArray() as Int32Array};
        case 'i64':
          return {arrowType: 'i64', values: vector.toArray() as BigInt64Array};
      }
      break;
    }
    case Type.Float: {
      const {precision} = type as unknown as {precision: number};
      // Arrow `Precision`: 0 = HALF, 1 = SINGLE, 2 = DOUBLE. Only the latter two are declarable.
      if (precision === 1) return {arrowType: 'f32', values: vector.toArray() as Float32Array};
      if (precision === 2) return {arrowType: 'f64', values: vector.toArray() as Float64Array};
      break;
    }
  }
  throw new Error(`viewport column "${name}" has a type this decoder cannot read: ${type}`);
}

/**
 * Concatenate one declared column's per-frame pieces into a single [`ScalarColumn`].
 *
 * All pieces carry the same `arrowType` by construction — every frame serialises the same
 * manifest schema — asserted rather than assumed, because a mismatch would mean the fill below
 * silently mixed two columns' values. Typed-array families allocate once and `set`; `bool` and
 * `utf8` are plain JS arrays and concat.
 */
function concatScalarColumns(pieces: ScalarColumn[], total: number): ScalarColumn {
  const first = pieces[0]!;
  if (pieces.length === 1) return first;
  for (const piece of pieces) {
    if (piece.arrowType !== first.arrowType) {
      throw new Error(
        `frames disagree on a column's type: ${piece.arrowType} vs ${first.arrowType}`
      );
    }
  }
  if (first.arrowType === 'bool' || first.arrowType === 'utf8') {
    return {
      arrowType: first.arrowType,
      values: pieces.flatMap((p) => p.values as (boolean | string)[])
    } as ScalarColumn;
  }
  // The typed families share the `set`-into-a-preallocated-buffer shape; the switch is what
  // names each concrete constructor for the type checker. Mirrors `scalarColumn`'s arms — the
  // two must be changed together.
  const fill = <A extends {set(a: A, o: number): void; length: number}>(out: A): A => {
    let offset = 0;
    for (const piece of pieces) {
      // Same `arrowType` (asserted above) means same concrete typed-array class; the checker
      // cannot see through the union, hence the `unknown` step.
      const values = piece.values as unknown as A;
      out.set(values, offset);
      offset += values.length;
    }
    return out;
  };
  switch (first.arrowType) {
    case 'u8':
      return {arrowType: 'u8', values: fill(new Uint8Array(total))};
    case 'u16':
      return {arrowType: 'u16', values: fill(new Uint16Array(total))};
    case 'u32':
      return {arrowType: 'u32', values: fill(new Uint32Array(total))};
    case 'u64':
      return {arrowType: 'u64', values: fill(new BigUint64Array(total))};
    case 'i8':
      return {arrowType: 'i8', values: fill(new Int8Array(total))};
    case 'i16':
      return {arrowType: 'i16', values: fill(new Int16Array(total))};
    case 'i32':
      return {arrowType: 'i32', values: fill(new Int32Array(total))};
    case 'i64':
      return {arrowType: 'i64', values: fill(new BigInt64Array(total))};
    case 'timestamp_us':
      return {arrowType: 'timestamp_us', values: fill(new BigInt64Array(total))};
    case 'f32':
      return {arrowType: 'f32', values: fill(new Float32Array(total))};
    case 'f64':
      return {arrowType: 'f64', values: fill(new Float64Array(total))};
  }
}

/**
 * Gather the even bits of a `u32` into the low 16 bits — the inverse of the Morton spread, and the
 * mirror of `tessera_build::input::compact`.
 */
function compact(v: number): number {
  let x = v & 0x55555555;
  x = (x | (x >>> 1)) & 0x33333333;
  x = (x | (x >>> 2)) & 0x0f0f0f0f;
  x = (x | (x >>> 4)) & 0x00ff00ff;
  x = (x | (x >>> 8)) & 0x0000ffff;
  return x >>> 0;
}

/**
 * Decode a framed `/v1/viewport` body.
 *
 * Two rules this function exists to hold:
 *
 * 1. `ids` stays a `BigUint64Array`. A `tessera_id` is a u64 and does not survive a double.
 * 2. Positions are deinterleaved here, once, into the layout deck.gl's `getPosition` wants. The
 *    server ships one `code: uint64` per point — the Morton interleave of two 32-bit fixed-point
 *    axes — so this is where the axes come apart.
 *
 * The output is **cell space**, `[0, 65536)` per axis with a fraction below the cell, which is the
 * grid's own units and needs no quantisation extent to interpret. That is why `coords.ts` no
 * longer takes one: the extent is what maps *data* coordinates onto the grid, and nothing on this
 * path is in data coordinates any more. The raw `codes` are returned alongside, because a client
 * that wants the containing tile at any depth gets it by shifting rather than by re-quantising.
 */
export function decodeViewport(body: Uint8Array): ViewportResult {
  const parts = splitFramedStreams(body);

  // The trailer's key set is closed (contracts §3.2 r26) and validated at every decode: the one
  // server-authored JSON region of the body must not quietly acquire a field no reader checks.
  const trailer = JSON.parse(new TextDecoder().decode(parts.trailer)) as Record<string, unknown>;
  const trailerKeys = Object.keys(trailer)
    .filter((k) => k !== 'stage_ns')
    .sort();
  const expected = ['arrow_serialise_ns', 'flushes', 'points', 'stream_us'];
  if (trailerKeys.length !== expected.length || trailerKeys.some((k, i) => k !== expected[i])) {
    throw new Error(`trailer keys outside the closed set: ${trailerKeys.join(',')}`);
  }
  if (trailer['flushes'] !== parts.points.length) {
    throw new Error(
      `trailer claims ${trailer['flushes']} point frames, body carries ${parts.points.length}`
    );
  }

  const tileTable = tableFromIPC(parts.tiles);
  const tile = u64Column(tileTable, 'tile');
  const visible = u64Column(tileTable, 'visible');
  const matched = u64Column(tileTable, 'matched');
  const served = u64Column(tileTable, 'served');
  const tiles: TileCounts[] = [];
  for (let i = 0; i < tile.length; i++) {
    tiles.push({
      tile: tile[i]!,
      visible: visible[i]!,
      matched: matched[i]!,
      served: served[i]!
    });
  }

  // Each kind-3 frame is a complete Arrow stream; Arrow JS reads the batches of one stream into
  // one Table, and concatenating the frames' streams byte-wise would decode only the first — so
  // frames decode separately and their tables concatenate as row groups. Zero frames (an empty
  // response carries no points schema at all — contracts §3.2) decodes to zero points.
  const pointTables = parts.points.map((frame) => tableFromIPC(frame));
  const totalPoints = pointTables.reduce((n, t) => n + t.numRows, 0);
  if (trailer['points'] !== totalPoints) {
    throw new Error(`trailer claims ${trailer['points']} points, body carries ${totalPoints}`);
  }
  const ids = new BigUint64Array(totalPoints);
  const codes = new BigUint64Array(totalPoints);
  {
    let offset = 0;
    for (const t of pointTables) {
      ids.set(u64Column(t, 'tessera_id'), offset);
      codes.set(u64Column(t, 'code'), offset);
      offset += t.numRows;
    }
  }
  // **`f64`, and not because it is convenient.** A cell coordinate is 32 bits per axis, so the
  // de-interleaved value needs a 32-bit mantissa to round-trip; `f32` has 24 and loses the
  // sub-cell part. `decode.test.ts` re-interleaves these back into the server's `code` and would
  // catch it. The narrowing to the renderer's `f32` world space happens later, per band, where the
  // precision is no longer needed.
  const positions = new Float64Array(ids.length * 2);
  const world = new Float32Array(ids.length * 2);
  // **The halves are read as `u32`s over the same bytes, never as `BigInt`s.** Arrow's `u64` column
  // is little-endian, so each code is already two 32-bit words in the order this loop wants them,
  // and a `Uint32Array` view costs nothing. Taking them off the `BigUint64Array` instead — one read
  // plus a shift plus a mask — is three `BigInt` allocations per point, which at 2.5 × 10^6 points
  // measured 2.5 s of decode on the main thread and was the largest single cost in the client.
  const halves = new Uint32Array(codes.buffer, codes.byteOffset, codes.length * 2);
  for (let i = 0; i < ids.length; i++) {
    // JS bitwise operators are int32, so the spread/compact arithmetic happens 32 bits at a time.
    // The halves recombine by multiplication rather than by shifting, which would overflow int32
    // at the top of the axis.
    const lo = halves[i * 2]!;
    const hi = halves[i * 2 + 1]!;
    const qx = compact(lo) + compact(hi) * 65536;
    const qy = compact(lo >>> 1) + compact(hi >>> 1) * 65536;
    const x = qx / 65536;
    const y = qy / 65536;
    positions[i * 2] = x;
    positions[i * 2 + 1] = y;
    world[i * 2] = x / CELLS_PER_WORLD_UNIT;
    world[i * 2 + 1] = y / CELLS_PER_WORLD_UNIT;
  }

  // Scalars concatenate per column across the frames' tables. Every frame carries the full
  // declared schema (each is a complete stream over the same manifest), so the first table's
  // field list is the response's.
  const scalars: Record<string, ScalarColumn> = {};
  if (pointTables.length > 0) {
    for (const field of pointTables[0]!.schema.fields) {
      if (field.name === 'tessera_id' || field.name === 'code') continue;
      const perFrame = pointTables.map((t) => scalarColumn(field.name, t.getChild(field.name)!));
      scalars[field.name] = concatScalarColumns(perFrame, totalPoints);
    }
  }

  let subCells: SubCell[] | null = null;
  if (parts.subCells) {
    const t = tableFromIPC(parts.subCells);
    const cell = u64Column(t, 'cell');
    const count = u64Column(t, 'count');
    subCells = [];
    for (let i = 0; i < cell.length; i++) {
      subCells.push({cell: cell[i]!, count: count[i]!});
    }
  }

  // Empty when the response carried no artifacts frame, which is the ordinary state of a
  // deployment with no layers — and of a principal who reaches none, and of a view holding none.
  // Those are one answer on purpose; see `Artifact`.
  const artifacts: Artifact[] = [];
  if (parts.artifacts) {
    const t = tableFromIPC(parts.artifacts);
    const layer = t.getChild('layer')!;
    const stableKey = t.getChild('stable_key')!;
    const tesseraId = u64Column(t, 'tessera_id');
    const maskedCount = u64Column(t, 'masked_count');
    for (let i = 0; i < tesseraId.length; i++) {
      artifacts.push({
        layer: String(layer.get(i)),
        tesseraId: tesseraId[i]!,
        // The one nullable column: a publisher need not supply a key.
        stableKey: stableKey.get(i) === null ? null : String(stableKey.get(i)),
        maskedCount: maskedCount[i]!
      });
    }
  }

  return {tiles, ids, codes, positions, world, scalars, subCells, artifacts};
}
