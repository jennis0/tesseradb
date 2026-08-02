import {tableFromIPC, type Table} from 'apache-arrow';
import {splitFramedStreams} from './frame.js';
import type {SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
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

  const scalars: Record<string, unknown[]> = {};
  for (const field of pointTable.schema.fields) {
    if (field.name === 'tessera_id' || field.name === 'code') continue;
    scalars[field.name] = [...pointTable.getChild(field.name)!];
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
