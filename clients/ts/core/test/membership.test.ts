import {describe, expect, it} from 'vitest';
import {
  makeData,
  makeVector,
  tableToIPC,
  Table,
  Uint64,
  vectorFromArray
} from 'apache-arrow';
import {decodeViewport} from '../src/decode.js';

/**
 * The per-point membership column (D12, §5.10) is a nullable `u64` named `membership:<layer>`,
 * carried after the render scalars. It is not a declared scalar — step 3 consumes it into a
 * response-local index in the decode worker — so the decoder must skip it by name. Decoding it as
 * a scalar would put a `tessera_id` on the palette, which is exactly the guess exact-only refuses.
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

function u64(values: bigint[]) {
  return makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
}

describe('the membership column', () => {
  it('is skipped by name and never becomes a scalar', () => {
    const tiles = tableToIPC(
      new Table({
        tile: u64([0n]),
        visible: u64([9n]),
        matched: u64([9n]),
        served: u64([2n])
      }),
      'stream'
    );
    // Two points with a render scalar `w` and the membership column after it.
    const points = tableToIPC(
      new Table({
        tessera_id: u64([1n, 2n]),
        code: u64([0n, 0n]),
        w: vectorFromArray(Uint32Array.from([10, 11])),
        'membership:clusters/x': u64([5n, 5n])
      }),
      'stream'
    );
    const trailer = new TextEncoder().encode(
      JSON.stringify({arrow_serialise_ns: 0, flushes: 1, points: 2, stream_us: 0})
    );
    const body = frame([
      {kind: 1, payload: tiles},
      {kind: 3, payload: points},
      {kind: 4, payload: trailer}
    ]);

    const r = decodeViewport(body);
    expect(r.ids.length).toBe(2);
    // The render scalar decoded; the membership column did not become one.
    expect(Object.keys(r.scalars)).toEqual(['w']);
    expect('membership:clusters/x' in r.scalars).toBe(false);
  });
});
