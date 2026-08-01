import {tableFromIPC, type Table} from 'apache-arrow';
import {splitFramedStreams} from './frame.js';
import type {SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

function f32Column(table: Table, name: string): Float32Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as Float32Array;
}

/**
 * Decode a framed `/v1/viewport` body.
 *
 * Two rules this function exists to hold:
 *
 * 1. `ids` stays a `BigUint64Array`. A `tessera_id` is a u64 and does not survive a double.
 * 2. Positions are interleaved here, once, into the layout deck.gl's `getPosition` wants — the
 *    server ships separate `x`/`y` columns (client-interaction §8.2 records the cost). Coordinates
 *    are left in DATA space; `coords.ts` converts to world space, because that conversion needs
 *    the bundle's quantisation extent and this function does not have it.
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
  const xs = f32Column(pointTable, 'x');
  const ys = f32Column(pointTable, 'y');
  const positions = new Float32Array(ids.length * 2);
  for (let i = 0; i < ids.length; i++) {
    positions[i * 2] = xs[i]!;
    positions[i * 2 + 1] = ys[i]!;
  }

  const scalars: Record<string, unknown[]> = {};
  for (const field of pointTable.schema.fields) {
    if (field.name === 'tessera_id' || field.name === 'x' || field.name === 'y') continue;
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

  return {tiles, ids, positions, scalars, subCells};
}
