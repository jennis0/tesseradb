import {Field, List, Table, Uint64, tableFromIPC, tableToIPC, vectorFromArray, type Vector} from 'apache-arrow';
import {FRAME_ARTIFACTS, FRAME_TILES, splitFramedStreams} from '../src/frame.js';

/** `parent_ids: list<uint64>` as contracts §3.2 r71 types it. */
const PARENTS = new List(new Field('item', new Uint64(), false));

/**
 * An r44 golden with its `hull_x`/`hull_y` columns removed — the r45 body the same capture would
 * produce for a request that did not ask for the shape — and its scalar `parent_id` lifted into
 * the `parent_ids` list r71 serves (a null becomes the empty list, a value a list of one — the
 * tree these goldens were captured from serves at most one parent). So the claims about the rest
 * of the row can still be made against real bytes until the goldens are recaptured
 * (`artifacts.client.test.ts` says why the decoder refuses the old names outright). This is a
 * rewrite of a stale recording, in test code only: the decoder keeps no such shim (decision 0048).
 */
export function stripOldShapeColumns(body: Uint8Array): Uint8Array {
  const streams = splitFramedStreams(body);
  const table = tableFromIPC(streams.artifacts!);
  const columns: Record<string, Vector> = {};
  for (const field of table.schema.fields) {
    if (field.name === 'hull_x' || field.name === 'hull_y') continue;
    const column = table.getChild(field.name)!;
    if (field.name === 'parent_id') {
      columns['parent_ids'] = vectorFromArray(
        Array.from(column, (v) => (v === null ? [] : [BigInt(v as bigint)])),
        PARENTS
      );
    } else columns[field.name] = column;
  }
  return reframe(body, FRAME_ARTIFACTS, tableToIPC(new Table(columns), 'stream'));
}

/**
 * The same golden's **tiles** frame given its `highlighted` column, fifth after `served`
 * (`highlight-and-hierarchy.md` §2).
 *
 * The value is each tile's `matched`, which is what the server serves for a request carrying no
 * `highlight` — these captures carried none — so this recreates the body the same capture would
 * produce against a server that serves the column, and every claim the tests make about the
 * counts still stands against real bytes. A rewrite of a stale recording, in test code only: the
 * decoder keeps no shim (decision 0048) and refuses a body without the column.
 */
export function liftTilesHighlighted(body: Uint8Array): Uint8Array {
  const streams = splitFramedStreams(body);
  const table = tableFromIPC(streams.tiles);
  if (table.schema.fields.some((f) => f.name === 'highlighted')) return body;
  const columns: Record<string, Vector> = {};
  for (const field of table.schema.fields) columns[field.name] = table.getChild(field.name)!;
  columns['highlighted'] = table.getChild('matched')!;
  return reframe(body, FRAME_TILES, tableToIPC(new Table(columns), 'stream'));
}

/** Every lift a recorded golden needs to read as the wire reads today. */
export function liftGolden(body: Uint8Array): Uint8Array {
  return liftTilesHighlighted(stripOldShapeColumns(body));
}

/** `body` with one frame's payload replaced: `u8 kind, u32 LE length, payload`, frame by frame. */
function reframe(body: Uint8Array, replace: number, payload: Uint8Array): Uint8Array {
  const frames: {kind: number; payload: Uint8Array}[] = [];
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  let at = 0;
  while (at < body.length) {
    const kind = body[at]!;
    const length = view.getUint32(at + 1, true);
    frames.push({kind, payload: kind === replace ? payload : body.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  const out = new Uint8Array(frames.reduce((n, f) => n + 5 + f.payload.length, 0));
  const outView = new DataView(out.buffer);
  at = 0;
  for (const frame of frames) {
    out[at] = frame.kind;
    outView.setUint32(at + 1, frame.payload.length, true);
    out.set(frame.payload, at + 5);
    at += 5 + frame.payload.length;
  }
  return out;
}
