import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {splitFramedStreams} from '../src/frame.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

describe('splitFramedStreams', () => {
  it('finds three parts, the last absent, in a payload with no underlay', () => {
    const parts = splitFramedStreams(fixture('viewport-plain.bin'));
    expect(parts.tiles.byteLength).toBeGreaterThan(0);
    expect(parts.points.byteLength).toBeGreaterThan(0);
    expect(parts.subCells).toBeNull();
  });

  it('finds a sub-cell stream when the underlay was requested', () => {
    const parts = splitFramedStreams(fixture('viewport-underlay.bin'));
    expect(parts.subCells).not.toBeNull();
    expect(parts.subCells!.byteLength).toBeGreaterThan(0);
  });

  it('consumes the whole payload exactly', () => {
    const raw = fixture('viewport-underlay.bin');
    const parts = splitFramedStreams(raw);
    const consumed =
      4 + parts.tiles.byteLength + parts.points.byteLength + (parts.subCells?.byteLength ?? 0);
    expect(consumed).toBe(raw.byteLength);
  });

  it('leaves the plain payload byte-identical to a pre-underlay one', () => {
    // The server emits ZERO trailing bytes when no underlay was asked for — not an empty stream.
    const raw = fixture('viewport-plain.bin');
    const parts = splitFramedStreams(raw);
    expect(4 + parts.tiles.byteLength + parts.points.byteLength).toBe(raw.byteLength);
  });

  it('refuses a truncated payload rather than returning a short stream', () => {
    // A silently short points stream would decode to fewer points than `served` promised, which
    // is a P2 failure wearing a plausible face: a sample presented as the set.
    const raw = fixture('viewport-plain.bin');
    expect(() => splitFramedStreams(raw.subarray(0, raw.byteLength - 16))).toThrow();
  });
});
