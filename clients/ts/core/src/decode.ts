import {tableFromIPC, Type, type DataType, type Table, type Vector} from 'apache-arrow';
import {splitFramedStreams} from './frame.js';
import type {ScalarColumn, SubCell, TileCounts, ViewportResult} from './types.js';

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

  const pointTable = tableFromIPC(parts.points);
  const ids = u64Column(pointTable, 'tessera_id');
  const codes = u64Column(pointTable, 'code');
  const positions = new Float64Array(ids.length * 2);
  for (let i = 0; i < ids.length; i++) {
    const code = codes[i]!;
    // Split into two u32 halves before deinterleaving: JS bitwise operators are int32, so the
    // spread/compact arithmetic has to happen 32 bits at a time. The halves recombine by
    // multiplication rather than by shifting, which would overflow int32 at the top of the axis.
    const hi = Number(code >> 32n) >>> 0;
    const lo = Number(code & 0xffffffffn) >>> 0;
    const qx = compact(lo) + compact(hi) * 65536;
    const qy = compact(lo >>> 1) + compact(hi >>> 1) * 65536;
    positions[i * 2] = qx / 65536;
    positions[i * 2 + 1] = qy / 65536;
  }

  const scalars: Record<string, ScalarColumn> = {};
  for (const field of pointTable.schema.fields) {
    if (field.name === 'tessera_id' || field.name === 'code') continue;
    scalars[field.name] = scalarColumn(field.name, pointTable.getChild(field.name)!);
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

  return {tiles, ids, codes, positions, scalars, subCells};
}
