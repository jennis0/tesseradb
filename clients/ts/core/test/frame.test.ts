import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {
  FRAME_TRAILER,
  splitFramedStreams
} from '../src/frame.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

describe('splitFramedStreams', () => {
  it('finds tiles, points and trailer — and no sub-cells — in a payload with no underlay', () => {
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

  it('consumes the whole payload exactly — every frame is length-prefixed', () => {
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
    // A silently short points stream would decode to fewer points than `served` promised, which
    // is a P2 failure wearing a plausible face: a sample presented as the set.
    const raw = fixture('viewport-plain.bin');
    expect(() => splitFramedStreams(raw.subarray(0, raw.byteLength - 16))).toThrow();
  });

  it('refuses a body whose trailer is missing — incomplete by contract', () => {
    // Strip the trailing kind-4 frame whole: what remains is well-framed but incomplete, which
    // is exactly the state a mid-stream abort leaves a client holding.
    const raw = fixture('viewport-plain.bin');
    const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
    let at = 0;
    let trailerStart = -1;
    while (at < raw.byteLength) {
      if (view.getUint8(at) === FRAME_TRAILER) trailerStart = at;
      at += 5 + view.getUint32(at + 1, true);
    }
    expect(trailerStart).toBeGreaterThan(0);
    expect(() => splitFramedStreams(raw.subarray(0, trailerStart))).toThrow(/trailer/);
  });

  it('refuses an unknown frame kind rather than skipping it', () => {
    const raw = fixture('viewport-plain.bin');
    const extended = new Uint8Array(raw.byteLength + 5);
    extended.set(raw);
    extended[raw.byteLength] = 9; // no such kind
    expect(() => splitFramedStreams(extended)).toThrow(/unknown frame kind/);
  });
});
