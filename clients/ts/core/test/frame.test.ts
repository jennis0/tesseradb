import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {
  FRAME_ARTIFACTS,
  FRAME_POINTS,
  FRAME_TILES,
  FRAME_TRAILER,
  splitFramedStreams
} from '../src/frame.js';
import {refused} from './support.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

describe('splitFramedStreams', () => {
  it('finds tiles, points and trailer, and no sub-cells, in a payload with no underlay', () => {
    const parts = splitFramedStreams(fixture('viewport-plain.bin'));
    expect(parts.tiles.byteLength).toBeGreaterThan(0);
    expect(parts.points.length).toBeGreaterThan(0);
    for (const frame of parts.points) expect(frame.byteLength).toBeGreaterThan(0);
    expect(parts.subCells).toBeNull();
    expect(parts.trailer.byteLength).toBeGreaterThan(0);
  });

  it('finds a sub-cells frame when the underlay was requested', () => {
    const parts = splitFramedStreams(fixture('viewport-underlay.bin'));
    expect(parts.subCells).not.toBeNull();
    expect(parts.subCells!.byteLength).toBeGreaterThan(0);
  });

  it('has no artifacts frame when the response served none', () => {
    // Absent, not empty: the server omits the frame, so a deployment with no annotation layers pays
    // nothing. Captured with `layers: []` against a server that has layers.
    expect(splitFramedStreams(fixture('viewport-plain.bin')).artifacts).toBeNull();
    expect(splitFramedStreams(fixture('viewport-underlay.bin')).artifacts).toBeNull();
  });

  it('takes the frame where the response served some, between the tiles and the points', () => {
    // Captured from a server with a published layer (`scripts/capture-golden.mjs`).
    const parts = splitFramedStreams(fixture('viewport-artifacts.bin'));
    expect(parts.artifacts).not.toBeNull();
    expect(parts.artifacts!.byteLength).toBeGreaterThan(0);
  });

  it('takes an artifacts frame, and refuses a second or a misplaced one', () => {
    const frame = (kind: number, payload: Uint8Array) => {
      const out = new Uint8Array(5 + payload.byteLength);
      out[0] = kind;
      new DataView(out.buffer).setUint32(1, payload.byteLength, true);
      out.set(payload, 5);
      return out;
    };
    const cat = (...parts: Uint8Array[]) => {
      const out = new Uint8Array(parts.reduce((n, p) => n + p.byteLength, 0));
      let at = 0;
      for (const p of parts) {
        out.set(p, at);
        at += p.byteLength;
      }
      return out;
    };
    const body = new Uint8Array([1, 2, 3]);

    const ok = splitFramedStreams(
      cat(
        frame(FRAME_TILES, body),
        frame(FRAME_POINTS, body),
        frame(FRAME_ARTIFACTS, body),
        frame(FRAME_TRAILER, body)
      )
    );
    expect(ok.artifacts).not.toBeNull();

    // A second artifacts frame is refused, as a second tile frame is; it would show a cluster twice.
    refused(() =>
      splitFramedStreams(
        cat(
          frame(FRAME_TILES, body),
          frame(FRAME_ARTIFACTS, body),
          frame(FRAME_ARTIFACTS, body),
          frame(FRAME_TRAILER, body)
        )
      )
    );

    // The points come first, so no point waits on the artifacts; a points frame after them is
    // out of order.
    refused(() =>
      splitFramedStreams(
        cat(
          frame(FRAME_TILES, body),
          frame(FRAME_ARTIFACTS, body),
          frame(FRAME_POINTS, body),
          frame(FRAME_TRAILER, body)
        )
      )
    );
  });

  it('consumes the whole payload exactly: every frame is length-prefixed', () => {
    const raw = fixture('viewport-underlay.bin');
    const parts = splitFramedStreams(raw);
    const header = 5; // u8 kind + u32 LE length, per frame
    const frames =
      2 + parts.points.length + (parts.subCells ? 1 : 0); // tiles + trailer + the rest
    const consumed =
      frames * header +
      parts.tiles.byteLength +
      parts.points.reduce((n, f) => n + f.byteLength, 0) +
      (parts.subCells?.byteLength ?? 0) +
      parts.trailer.byteLength;
    expect(consumed).toBe(raw.byteLength);
  });

  it('refuses a truncated payload rather than returning a short stream', () => {
    // A short points stream would decode to fewer points than `served` promised: a sample presented
    // as the set.
    const raw = fixture('viewport-plain.bin');
    refused(() => splitFramedStreams(raw.subarray(0, raw.byteLength - 16)));
  });

  it('refuses a body whose trailer is missing: incomplete by contract', () => {
    // Without the trailing kind-4 frame the body is well-framed but incomplete, as a mid-stream abort
    // leaves it.
    const raw = fixture('viewport-plain.bin');
    const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
    let at = 0;
    let trailerStart = -1;
    while (at < raw.byteLength) {
      if (view.getUint8(at) === FRAME_TRAILER) trailerStart = at;
      at += 5 + view.getUint32(at + 1, true);
    }
    expect(trailerStart).toBeGreaterThan(0);
    refused(() => splitFramedStreams(raw.subarray(0, trailerStart)));
  });

  it('refuses an unknown frame kind rather than skipping it', () => {
    const raw = fixture('viewport-plain.bin');
    const extended = new Uint8Array(raw.byteLength + 5);
    extended.set(raw);
    extended[raw.byteLength] = 9; // no such kind
    refused(() => splitFramedStreams(extended));
  });
});
