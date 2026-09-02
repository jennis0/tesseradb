import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {decodeViewport} from '../src/decode.js';
import {CELL_GRID} from '../src/coords.js';
import {liftTilesHighlighted} from './old-shape-columns.js';

// The goldens predate the tiles frame's `highlighted` column and are lifted rather than
// recaptured — see `liftTilesHighlighted`, and the fixture note in `clients/ts/README.md`.
const fixture = (name: string) =>
  liftTilesHighlighted(new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name))));
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
    const q = meta.views[0].quantisation;
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

  it('decodes every rendered scalar the manifest names, at its declared type, and no other', () => {
    // Asserted against `meta.json` rather than a hand-written list, so a column added to the
    // schema is covered without touching the test.
    //
    // **`render` is the whole of what decides this**, and both directions matter. A rendered
    // column occupies a slot in every row of the hot column and therefore arrives here; a column
    // with `render: false` lives in entity space or in the record blob, is filterable, is returned
    // at drill-down, and appears in **no** viewport response. A client that offered every declared
    // column to its colour control would be offering columns whose values never arrive.
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const declared = meta.declared_scalars as {name: string; arrow_type: string; render: boolean}[];
    expect(declared.length).toBeGreaterThan(0);
    expect(declared.some((c) => c.render)).toBe(true);
    for (const {name, arrow_type, render} of declared) {
      const column = r.scalars[name];
      if (!render) {
        expect(column, `column ${name} is not rendered and must not arrive`).toBeUndefined();
        continue;
      }
      expect(column, `column ${name} is missing`).toBeDefined();
      expect(column!.arrowType, `column ${name}`).toBe(arrow_type);
      expect(column!.values.length, `column ${name}`).toBe(r.ids.length);
    }
  });

  it('hands back typed arrays rather than boxed values', () => {
    // The reason the decoder exists in this shape: `[...child]` allocates one heap object per
    // value per column, which at a full budget across this fixture's tail is ~10⁶ per response.
    // `bool` and `utf8` are the two Arrow has no typed form for, and are the only ones exempt.
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const boxed = ['bool', 'utf8'];
    let typedColumns = 0;
    for (const [name, column] of Object.entries(r.scalars)) {
      if (boxed.includes(column.arrowType)) {
        expect(Array.isArray(column.values), `column ${name}`).toBe(true);
        continue;
      }
      typedColumns++;
      expect(ArrayBuffer.isView(column.values), `column ${name} must be a typed array`).toBe(true);
    }
    expect(typedColumns).toBeGreaterThan(0);
  });

  it('reads a category code as a plain integer of the declared width', () => {
    // Categories cross the wire as codes and nothing else — the key never appears here. Code 0 is
    // the *absent* sentinel, so a column that is absent for part of the corpus legitimately
    // carries it, and the decoder must not confuse it with a missing value.
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const category = (meta.declared_scalars as {name: string; category: unknown}[]).find(
      (s) => s.category !== null
    );
    expect(category, 'the golden fixture must declare at least one category').toBeDefined();
    const column = r.scalars[category!.name]!;
    expect(['u8', 'u16', 'u32']).toContain(column.arrowType);
    for (const code of column.values as Uint8Array | Uint16Array | Uint32Array) {
      expect(Number.isInteger(code)).toBe(true);
      expect(code).toBeGreaterThanOrEqual(0);
    }
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
