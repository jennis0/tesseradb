import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {decodeViewport} from '../src/decode.js';
import type {BrowsePage} from '../src/types.js';
import {liftArtifactTarget} from './old-shape-columns.js';

/**
 * **The highlight's three columns and the browse verb, against real bytes.**
 *
 * Every fixture here was recorded from a `tessera serve` over `data/ladder/arxiv/` on 2026-09-02,
 * the day the server track landed (`highlight-and-hierarchy.md` §2 and §4; contracts §3.2 r74 and
 * r75). They are not assembled, and they are not lifted: `liftTilesHighlighted` exists for the
 * older goldens, which predate the column, and none of it is used here.
 *
 * The request behind the three viewport bodies is one bbox at zoom 3 over the `knn` view, `k = 40`,
 * `computed: ["centroid"]` so no hull travels, under a principal holding the forty broadest
 * archives — with `filters` on `archive in [cs, math]` and `highlight` on `archive in [cs]`, so the
 * three counts are **distinct and all non-zero**, which is the only arrangement in which a decoder
 * reading the wrong column would be caught.
 */

const fixture = (name: string) => liftArtifactTarget(new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name))));
const page = (name: string) => JSON.parse(readFileSync(join(import.meta.dirname, 'fixtures', name), 'utf8')) as Record<string, unknown>;
const sum = (tiles: readonly {visible: bigint; matched: bigint; highlighted: bigint; served: bigint}[], f: 'visible' | 'matched' | 'highlighted' | 'served') =>
  tiles.reduce((n, t) => n + Number(t[f]), 0);

describe('the three highlighted columns, off a served body', () => {
  it('counts a tile’s highlighted inside its matched inside its visible', () => {
    const r = decodeViewport(fixture('viewport-highlight.bin'));
    expect(r.tiles.length).toBe(16);
    // The design's `highlighted ≤ matched ≤ visible`, and all three different: 341,971 of the
    // 581,300 the filter admits, of the 1,504,043 this principal may see here.
    expect(sum(r.tiles, 'visible')).toBe(1_504_043);
    expect(sum(r.tiles, 'matched')).toBe(581_300);
    expect(sum(r.tiles, 'highlighted')).toBe(341_971);
    for (const t of r.tiles) {
      expect(t.highlighted).toBeLessThanOrEqual(t.matched);
      expect(t.matched).toBeLessThanOrEqual(t.visible);
    }
  });

  it('carries a bit per served point, and the bit is not the whole set', () => {
    const r = decodeViewport(fixture('viewport-highlight.bin'));
    expect(r.highlighted).not.toBeNull();
    expect(r.highlighted!.length).toBe(r.ids.length);
    // 359 of 640: neither all nor none, so a decoder returning a constant would fail here.
    expect(r.highlighted!.reduce((a, b) => a + b, 0)).toBe(359);
    expect(r.ids.length).toBe(640);
  });

  it('carries a bit per served artifact, beside the filter’s and different from it', () => {
    const r = decodeViewport(fixture('viewport-highlight.bin'));
    expect(r.artifacts.length).toBe(16);
    expect(r.artifacts.filter((a) => a.matched === true)).toHaveLength(10);
    expect(r.artifacts.filter((a) => a.highlighted === true)).toHaveLength(7);
    // Never null on a body whose request carried a highlight — the question was put of every row.
    expect(r.artifacts.every((a) => a.highlighted !== null)).toBe(true);
  });

  it('answers a request with no highlight by the identity, and sends no bits at all', () => {
    const r = decodeViewport(fixture('viewport-no-highlight.bin'));
    // **`highlighted` is always present and equal to `matched`** where none was asked — an absent
    // highlight is the identity for that quantity, so a client reads a column and never an option.
    expect(sum(r.tiles, 'highlighted')).toBe(sum(r.tiles, 'matched'));
    for (const t of r.tiles) expect(t.highlighted).toBe(t.matched);
    // The points column is absent rather than all-false, and the artifacts' bit is null: a `false`
    // would answer a question nobody asked.
    expect(r.highlighted).toBeNull();
    expect(r.artifacts.every((a) => a.highlighted === null)).toBe(true);
    expect(r.artifacts.some((a) => a.matched !== null)).toBe(true);
  });
});

describe('point_rows = "highlight", against the full answer to the same request', () => {
  /**
   * §2's claim that a highlight change re-sends bits and not points, checked where it can only be
   * checked — against the two bodies a server produced for one request under the two projections.
   */
  it('is the same rows, in the same order, carrying the same bits', () => {
    const full = decodeViewport(fixture('viewport-highlight.bin'));
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    expect(bits.pointsProjection).toBe('highlight');
    expect(full.pointsProjection).toBe('full');
    expect([...bits.ids]).toEqual([...full.ids]);
    expect([...bits.highlighted!]).toEqual([...full.highlighted!]);
  });

  it('is the same per-tile answer — the served split and every count', () => {
    const full = decodeViewport(fixture('viewport-highlight.bin'));
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    expect(bits.tiles).toEqual(full.tiles);
  });

  it('carries no position and no scalar — which is what a client joining by identifier wants', () => {
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    // The projection is read off the frame's schema and never off the request, so a decoder that
    // demanded `code` refused a body the client can already ask for. It does not.
    expect(bits.codes.length).toBe(0);
    expect(bits.positions.length).toBe(0);
    expect(bits.world.length).toBe(0);
    expect(Object.keys(bits.scalars)).toEqual([]);
    expect(bits.ids.length).toBe(640);
  });
});

describe('POST /v1/artifacts/browse, off served pages', () => {
  it('serves the roots by masked count descending, paged, `parents` empty on this form', () => {
    const p = page('browse-roots.json') as unknown as {artifacts: {masked_count: number}[]; parents: unknown[]; next?: string};
    expect(p.artifacts).toHaveLength(4);
    // `parents` is the children form's and is `[]` on the other two.
    expect(p.parents).toEqual([]);
    // Seventeen roots at `limit: 4`, so this page has a cursor: the order is total — count
    // descending, then `tessera_id` ascending — so a walk over it neither duplicates nor drops.
    expect(typeof p.next).toBe('string');
    const counts = p.artifacts.map((a) => a.masked_count);
    expect([...counts].sort((a, b) => b - a)).toEqual(counts);
  });

  it('serves a child under its parent, with a matched count the masked one does not move with', () => {
    const p = page('browse-children-filtered.json') as unknown as {
      artifacts: {tessera_id: string; rung: number; parent_ids: string[]; masked_count: number; matched_count: number}[];
    };
    const row = p.artifacts[0]!;
    expect(row.parent_ids).toEqual(['7718166018496935461']);
    expect(row.rung).toBe(1);
    // **Existence and `masked_count` never move with the filter**, and a row the filter admits
    // nothing of is still served — which is what a tree of counts has to be able to say.
    expect(row.matched_count).toBe(0);
    expect(row.masked_count).toBe(17_417);
    // Every identifier a decimal string: a `tessera_id` is u64 and JSON has no 64-bit integer.
    expect(typeof row.tessera_id).toBe('string');
    expect(BigInt(row.tessera_id)).toBeGreaterThan(2n ** 53n);
  });

  it('serves a search page with a cursor, and no matched count where no filter was sent', () => {
    const p = page('browse-search.json') as unknown as {artifacts: {matched_count?: number}[]; next?: string};
    expect(p.artifacts).toHaveLength(2);
    expect(typeof p.next).toBe('string');
    // Absent rather than null or zero — *there was no question*.
    expect(p.artifacts.every((a) => a.matched_count === undefined)).toBe(true);
  });

  it('reads through the client’s own row mapper, identifiers and counts intact', async () => {
    const raw = page('browse-roots.json');
    const {TesseraClient} = await import('../src/client.js');
    const fetchMock = async () => ({ok: true, status: 200, json: async () => raw, headers: new Headers()});
    const held = globalThis.fetch;
    globalThis.fetch = fetchMock as unknown as typeof globalThis.fetch;
    try {
      const client = new TesseraClient({viewerUrl: 'http://v', sessionUrl: 'http://s'});
      const decoded: BrowsePage = await client.browse('tok', {view: 'knn', layer: 'clusters/hdbscan'});
      expect(decoded.artifacts).toHaveLength(4);
      expect(decoded.artifacts[0]!.tesseraId).toBe(13_650_843_325_015_492_830n);
      expect(decoded.artifacts[0]!.maskedCount).toBe(239_300n);
      expect(decoded.artifacts[0]!.matchedCount).toBeNull();
      expect(decoded.artifacts[0]!.parentIds).toEqual([]);
      expect(decoded.next).toBe('33830:2719244864711908352');
    } finally {
      globalThis.fetch = held;
    }
  });
});
