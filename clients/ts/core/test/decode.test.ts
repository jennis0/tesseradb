import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {decodeViewport} from '../src/decode.js';
import {CELL_GRID} from '../src/coords.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));
const meta = JSON.parse(readFileSync(join(import.meta.dirname, 'fixtures', 'meta.json'), 'utf8'));

describe('decodeViewport', () => {
  it('returns one counts row per non-empty tile, with served and matched inside visible', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    expect(r.tiles.length).toBeGreaterThan(0);
    for (const t of r.tiles) {
      expect(t.served).toBeLessThanOrEqual(t.visible);
      expect(t.matched).toBeLessThanOrEqual(t.visible);
    }
  });

  it('returns exactly sum(served) points, interleaved as x,y pairs', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const served = r.tiles.reduce((acc, t) => acc + Number(t.served), 0);
    expect(served).toBeGreaterThan(0);
    expect(r.ids.length).toBe(served);
    expect(r.positions.length).toBe(served * 2);
  });

  it('places every point inside the cell grid', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    for (let i = 0; i < r.ids.length; i++) {
      expect(r.positions[i * 2]).toBeGreaterThanOrEqual(0);
      expect(r.positions[i * 2]).toBeLessThan(CELL_GRID);
      expect(r.positions[i * 2 + 1]).toBeGreaterThanOrEqual(0);
      expect(r.positions[i * 2 + 1]).toBeLessThan(CELL_GRID);
    }
  });

  it('deinterleaves losslessly — the positions re-interleave to the code they came from', () => {
    // The decoder's only geometric job. Re-spreading the two axes must reproduce the server's
    // `code` bit for bit, at the full 32 bits per axis: a `Float32Array` here would fail this,
    // which is why the positions are `f64`.
    const r = decodeViewport(fixture('viewport-plain.bin'));
    expect(r.codes.length).toBe(r.ids.length);
    const spread = (v: number): bigint => {
      let out = 0n;
      for (let bit = 0; bit < 32; bit++) out |= ((BigInt(v) >> BigInt(bit)) & 1n) << BigInt(2 * bit);
      return out;
    };
    for (let i = 0; i < r.ids.length; i++) {
      const qx = Math.round(r.positions[i * 2]! * 65536);
      const qy = Math.round(r.positions[i * 2 + 1]! * 65536);
      expect(spread(qx) | (spread(qy) << 1n)).toBe(r.codes[i]!);
    }
  });

  it('agrees with the bundle’s cell grid on where the points are', () => {
    // The extent is still what makes a cell interpretable — it just no longer sits between the
    // wire and the position. Scaling cell space back through it must land inside the extent
    // `/v1/meta` publishes, or the client and the server disagree about the grid.
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const q = meta.quantisation;
    for (let i = 0; i < r.ids.length; i++) {
      const x = q.x_min + (r.positions[i * 2]! / CELL_GRID) * (q.x_max - q.x_min);
      const y = q.y_min + (r.positions[i * 2 + 1]! / CELL_GRID) * (q.y_max - q.y_min);
      expect(x).toBeGreaterThanOrEqual(q.x_min);
      expect(x).toBeLessThanOrEqual(q.x_max);
      expect(y).toBeGreaterThanOrEqual(q.y_min);
      expect(y).toBeLessThanOrEqual(q.y_max);
    }
  });

  it('returns null sub-cells without the underlay and positive counts with it', () => {
    expect(decodeViewport(fixture('viewport-plain.bin')).subCells).toBeNull();
    const withUnderlay = decodeViewport(fixture('viewport-underlay.bin'));
    expect(withUnderlay.subCells!.length).toBeGreaterThan(0);
    for (const c of withUnderlay.subCells!) expect(c.count).toBeGreaterThan(0n);
  });

  it('never narrows a tessera_id to a double', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    expect(r.ids).toBeInstanceOf(BigUint64Array);
  });

  it('agrees with the underlay payload on the point set', () => {
    // Same request, same k, same viewport — the underlay adds cells, never changes selection.
    const plain = decodeViewport(fixture('viewport-plain.bin'));
    const underlay = decodeViewport(fixture('viewport-underlay.bin'));
    expect([...underlay.ids]).toEqual([...plain.ids]);
  });

  it('sums the underlay’s sub-cell counts to the visible total', () => {
    // The underlay is an exact masked breakdown of the same visible set the tile batch counts, so
    // the two must agree. If they ever do not, one of them is a sample and the panel that renders
    // it is lying.
    const r = decodeViewport(fixture('viewport-underlay.bin'));
    const cellTotal = r.subCells!.reduce((a, c) => a + c.count, 0n);
    const tileTotal = r.tiles.reduce((a, t) => a + t.visible, 0n);
    expect(cellTotal).toBe(tileTotal);
  });
});
