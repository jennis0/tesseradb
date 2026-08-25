// A worked decode of a `POST /v1/viewport` body, in JavaScript with `apache-arrow` and nothing
// of Tessera's. The framing is contracts §5; this file is the whole of what a stranger needs to
// read one, and `docs/openapi/README.md` walks it.
//
//   node src/decode-viewport.mjs <body.bin>
//
// prints every frame — kind, payload length, and for an Arrow payload its row count, column
// names and first row — then the first rows of each batch. `tessera_id` is a `u64`: it comes off
// Arrow as a `BigInt` and is printed as a decimal string, never narrowed to a JS number.

import {readFileSync} from 'node:fs';
import {tableFromIPC} from 'apache-arrow';

/** Frame kinds, contracts §5. Anything else is a decoder error, never skipped. */
export const KIND = Object.freeze({
  TILES: 1,
  SUB_CELLS: 2,
  POINTS: 3,
  TRAILER: 4,
  ARTIFACTS: 5
});

/** @type {Readonly<Record<number, string>>} */
const KIND_NAMES = Object.freeze({1: 'tiles', 2: 'sub-cells', 3: 'points', 4: 'trailer', 5: 'artifacts'});

/**
 * Split a body into its frames: `u8 kind`, `u32 little-endian payload length`, payload,
 * repeated. Nothing here parses Arrow; a reader dispatches on `kind` alone.
 *
 * Strict on purpose. A body that ends mid-frame, or ends without a trailer, is incomplete
 * whatever the transport said — every prefix of a stream is sound to *draw* (the counts are
 * exact from the first frame), but it must not be mistaken for the whole answer.
 *
 * @param {Uint8Array} body
 * @returns {{kind: number, payload: Uint8Array}[]}
 */
export function splitFrames(body) {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  const frames = [];
  let offset = 0;
  while (offset < body.byteLength) {
    if (offset + 5 > body.byteLength) throw new Error(`truncated frame header at byte ${offset}`);
    const kind = view.getUint8(offset);
    const length = view.getUint32(offset + 1, true);
    if (!(kind in KIND_NAMES)) throw new Error(`unknown frame kind ${kind} at byte ${offset}`);
    if (offset + 5 + length > body.byteLength) {
      throw new Error(`frame of kind ${kind} at byte ${offset} claims ${length} bytes past the end`);
    }
    frames.push({kind, payload: body.subarray(offset + 5, offset + 5 + length)});
    offset += 5 + length;
  }
  const first = frames[0];
  const last = frames[frames.length - 1];
  if (last === undefined || last.kind !== KIND.TRAILER) {
    throw new Error('no trailing kind-4 frame: the response is incomplete');
  }
  if (first === undefined || first.kind !== KIND.TILES) {
    throw new Error('the first frame must be the tiles frame');
  }
  return frames;
}

/**
 * Decode one body into its batches. Each Arrow payload is a complete IPC stream, so
 * `tableFromIPC` consumes it whole; the trailer is JSON.
 *
 * @param {Uint8Array} body
 */
export function decodeViewport(body) {
  const frames = splitFrames(body);
  /** @type {import('apache-arrow').Table | null} */
  let tiles = null;
  /** @type {import('apache-arrow').Table | null} */
  let subCells = null;
  /** @type {import('apache-arrow').Table | null} */
  let artifacts = null;
  /** @type {import('apache-arrow').Table[]} */
  const points = [];
  /** @type {Record<string, unknown> | null} */
  let trailer = null;
  for (const {kind, payload} of frames) {
    switch (kind) {
      case KIND.TILES:
        tiles = tableFromIPC(payload);
        break;
      case KIND.SUB_CELLS:
        subCells = tableFromIPC(payload);
        break;
      case KIND.ARTIFACTS:
        artifacts = tableFromIPC(payload);
        break;
      case KIND.POINTS:
        // Whole tiles per frame, boundaries not contract: the frames concatenate.
        points.push(tableFromIPC(payload));
        break;
      case KIND.TRAILER:
        trailer = JSON.parse(new TextDecoder().decode(payload));
        break;
    }
  }
  return {frames: frames.map((f) => f.kind), tiles, subCells, artifacts, points, trailer};
}

/**
 * One row of a table as `{column: value}`, `BigInt`s rendered as decimal strings so a `u64`
 * survives printing.
 *
 * @param {import('apache-arrow').Table} table
 * @param {number} i
 */
export function rowOf(table, i) {
  /** @type {Record<string, unknown>} */
  const out = {};
  for (const field of table.schema.fields) {
    const value = table.getChild(field.name)?.get(i);
    out[field.name] = typeof value === 'bigint' ? value.toString() : value;
  }
  return out;
}

/**
 * The first `n` rows across a list of tables that concatenate — the points frames.
 *
 * @param {import('apache-arrow').Table[]} tables
 * @param {number} n
 */
export function firstRows(tables, n) {
  const rows = [];
  for (const table of tables) {
    for (let i = 0; i < table.numRows && rows.length < n; i++) rows.push(rowOf(table, i));
    if (rows.length >= n) break;
  }
  return rows;
}

/** @param {string} path */
function main(path) {
  const body = new Uint8Array(readFileSync(path));
  const frames = splitFrames(body);
  console.log(`${path}: ${body.byteLength} bytes, ${frames.length} frames`);
  for (const {kind, payload} of frames) {
    if (kind === KIND.TRAILER) {
      console.log(`  kind ${kind} ${KIND_NAMES[kind]}: ${payload.byteLength} B  ${new TextDecoder().decode(payload)}`);
      continue;
    }
    const table = tableFromIPC(payload);
    console.log(
      `  kind ${kind} ${KIND_NAMES[kind]}: ${payload.byteLength} B, ${table.numRows} rows, columns [${table.schema.fields.map((f) => f.name).join(', ')}]`
    );
  }
  const {tiles, subCells, artifacts, points} = decodeViewport(body);
  console.log('first tiles rows:', firstRows(tiles ? [tiles] : [], 3));
  if (subCells) console.log('first sub-cells rows:', firstRows([subCells], 3));
  if (artifacts) console.log('first artifacts rows:', firstRows([artifacts], 3));
  console.log('first points rows:', firstRows(points, 3));
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  const path = process.argv[2];
  if (!path) {
    console.error('usage: node src/decode-viewport.mjs <body.bin>');
    process.exit(2);
  }
  main(path);
}
