import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {
  FRAME_ARTIFACTS,
  FRAME_POINTS,
  FRAME_TILES,
  FRAME_TRAILER,
  FrameReader,
  splitFramedStreams
} from '../src/frame.js';
import {framed, refused} from './support.js';

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

  it('refuses an artifacts frame in a viewport body, and takes artifacts frames and a trailer in an artifacts body', () => {
    const frame = (kind: number, payload: Uint8Array) => ({kind, payload});
    const body = new Uint8Array([1, 2, 3]);
    // Artifacts come from their own route; a viewport body carrying one is not this client's server.
    refused(() => splitFramedStreams(framed([frame(FRAME_TILES, body), frame(FRAME_ARTIFACTS, body), frame(FRAME_TRAILER, body)])));

    const reader = new FrameReader('artifacts');
    const kinds = reader.push(framed([frame(FRAME_ARTIFACTS, body), frame(FRAME_ARTIFACTS, new Uint8Array(0)), frame(FRAME_TRAILER, body)])).map((f) => f.kind);
    reader.end();
    expect(kinds).toEqual([FRAME_ARTIFACTS, FRAME_ARTIFACTS, FRAME_TRAILER]);

    // A points frame, a frame after the trailer, and a body with no trailer are refused.
    refused(() => new FrameReader('artifacts').push(framed([frame(FRAME_POINTS, body)])));
    refused(() => new FrameReader('artifacts').push(framed([frame(FRAME_TRAILER, body), frame(FRAME_ARTIFACTS, body)])));
    const cut = new FrameReader('artifacts');
    cut.push(framed([frame(FRAME_ARTIFACTS, body)]));
    refused(() => cut.end());
  });

  it('reads the captured artifacts body as artifacts frames and a trailer', () => {
    const reader = new FrameReader('artifacts');
    const kinds = reader.push(fixture('viewport-artifacts.bin')).map((f) => f.kind);
    reader.end();
    expect(kinds.at(-1)).toBe(FRAME_TRAILER);
    expect(kinds.slice(0, -1).every((k) => k === FRAME_ARTIFACTS)).toBe(true);
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
