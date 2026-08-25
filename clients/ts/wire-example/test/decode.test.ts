// The worked decode against the golden fixtures, read by path from the core package's test
// directory — deliberately not imported from `@tesseradb/client`, whose decoder is the thing
// this example must not share code with. `expected.json` is the same file the Python example's
// test reads, so the two decodes are held to one answer.

import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {decodeViewport, firstRows, rowOf, splitFrames} from '../src/decode-viewport.mjs';

const fixtures = join(import.meta.dirname, '..', '..', 'core', 'test', 'fixtures');
const fixture = (name: string) => new Uint8Array(readFileSync(join(fixtures, name)));
const expected: Record<
  string,
  {
    frames: number[];
    tiles: number;
    sub_cells: number | null;
    artifacts: number | null;
    points: number;
    first_point_tessera_id: string | null;
    first_artifact_tessera_id: string | null;
  }
> = JSON.parse(readFileSync(join(import.meta.dirname, 'expected.json'), 'utf8'));

describe('the worked decode', () => {
  for (const [name, want] of Object.entries(expected)) {
    if (name.startsWith('_')) continue;
    it(`decodes ${name} to the agreed frames, counts and first ids`, () => {
      const r = decodeViewport(fixture(name));
      expect(r.frames).toEqual(want.frames);
      expect(r.tiles!.numRows).toBe(want.tiles);
      expect(r.subCells?.numRows ?? null).toBe(want.sub_cells);
      expect(r.artifacts?.numRows ?? null).toBe(want.artifacts);
      expect(r.points.reduce((n, t) => n + t.numRows, 0)).toBe(want.points);
      expect(r.trailer).not.toBeNull();
      expect(r.trailer!.points).toBe(want.points);

      const firstPoint = firstRows(r.points, 1)[0];
      expect(firstPoint?.tessera_id ?? null).toBe(want.first_point_tessera_id);
      const firstArtifact = r.artifacts ? rowOf(r.artifacts, 0) : null;
      expect(firstArtifact?.tessera_id ?? null).toBe(want.first_artifact_tessera_id);
    });
  }

  it('reads a tessera_id as a BigInt and never as a number', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const id = r.points[0]!.getChild('tessera_id')!.get(0);
    expect(typeof id).toBe('bigint');
    // Past 2^53 a JS number would already have lost bits; the fixture's first id is.
    expect(id > 2n ** 53n).toBe(true);
  });

  it('counts served points per tile from the tiles frame and finds them all in the points frames', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const served = r.tiles!.getChild('served')!;
    let sum = 0n;
    for (let i = 0; i < r.tiles!.numRows; i++) sum += served.get(i) as bigint;
    expect(Number(sum)).toBe(r.points.reduce((n, t) => n + t.numRows, 0));
  });

  it('refuses a truncated body and a body without a trailer', () => {
    const body = fixture('viewport-plain.bin');
    expect(() => splitFrames(body.subarray(0, body.byteLength - 3))).toThrow(/past the end/);
    // Drop the trailer frame whole: the last frame is 5 bytes of header plus its JSON.
    const frames = splitFrames(body);
    const trailerBytes = 5 + frames[frames.length - 1]!.payload.byteLength;
    expect(() => splitFrames(body.subarray(0, body.byteLength - trailerBytes))).toThrow(/incomplete/);
  });

  it('refuses an unknown frame kind rather than skipping it', () => {
    const body = new Uint8Array(fixture('viewport-plain.bin'));
    body[0] = 9;
    expect(() => splitFrames(body)).toThrow(/unknown frame kind 9/);
  });
});
