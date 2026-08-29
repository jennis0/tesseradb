import {tableFromIPC, tableToIPC} from 'apache-arrow';
import {FRAME_ARTIFACTS, splitFramedStreams} from '../src/frame.js';

/**
 * An r44 golden with its `hull_x`/`hull_y` columns removed — the r45 body the same capture would
 * produce for a request that did not ask for the shape — so the claims about the rest of the row
 * can still be made against real bytes until the goldens are recaptured (`artifacts.client.test.ts`
 * says why the decoder refuses the old names outright).
 */
export function stripOldShapeColumns(body: Uint8Array): Uint8Array {
  const streams = splitFramedStreams(body);
  const table = tableFromIPC(streams.artifacts!);
  const kept = table.schema.fields.filter((f) => f.name !== 'hull_x' && f.name !== 'hull_y').map((f) => f.name);
  return reframe(body, tableToIPC(table.select(kept), 'stream'));
}

/** `body` with its kind-5 payload replaced: `u8 kind, u32 LE length, payload`, frame by frame. */
function reframe(body: Uint8Array, artifacts: Uint8Array): Uint8Array {
  const frames: {kind: number; payload: Uint8Array}[] = [];
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  let at = 0;
  while (at < body.length) {
    const kind = body[at]!;
    const length = view.getUint32(at + 1, true);
    frames.push({kind, payload: kind === FRAME_ARTIFACTS ? artifacts : body.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  const out = new Uint8Array(frames.reduce((n, f) => n + 5 + f.payload.length, 0));
  const outView = new DataView(out.buffer);
  at = 0;
  for (const {kind, payload} of frames) {
    out[at] = kind;
    outView.setUint32(at + 1, payload.length, true);
    out.set(payload, at + 5);
    at += 5 + payload.length;
  }
  return out;
}
