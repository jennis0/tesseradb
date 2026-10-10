// A worked decode of a `POST /v1/viewport` or `POST /v1/artifacts/viewport` body in JavaScript,
// using `apache-arrow` and no Mosaica code. `docs/openapi/README.md` walks through it.
//
//   node src/decode-viewport.mjs <body.bin>
//
// prints every frame (kind, payload length, and for an Arrow payload its row count, column names
// and first row), then the first rows of each batch. `mosaica_id` is a `u64`: Arrow gives a
// `BigInt`, printed as a decimal string, since a JS number would lose precision.

import {readFileSync} from 'node:fs';
import {tableFromIPC} from 'apache-arrow';

/** Frame kinds. Any other kind is a decoder error, not skipped. */
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
 * repeated. A reader dispatches on `kind` alone.
 *
 * A body that ends mid-frame or without a trailer throws. Every prefix of a stream can be drawn,
 * since counts are exact from the first frame, but it is not the whole answer.
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
  // A viewport body begins with its tiles frame; an artifacts viewport body is artifacts frames alone.
  if (first === undefined || (first.kind !== KIND.TILES && frames.slice(0, -1).some((f) => f.kind !== KIND.ARTIFACTS))) {
    throw new Error('a body begins with its tiles frame, or holds artifacts frames alone');
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
  /** One table per artifacts frame, in order: each is one tile's, so they are kept apart. */
  /** @type {import('apache-arrow').Table[]} */
  const artifacts = [];
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
        artifacts.push(tableFromIPC(payload));
        break;
      case KIND.POINTS:
        // Each frame holds whole tiles; where frames split is not fixed, and they concatenate.
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
 * The first `n` rows across tables that concatenate, such as the points frames.
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
  if (artifacts.length > 0) console.log('first artifacts rows:', firstRows(artifacts, 3));
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
