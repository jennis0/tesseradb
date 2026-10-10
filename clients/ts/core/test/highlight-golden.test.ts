import {describe, expect, it} from 'vitest';
import {decodeArtifactsFrame, decodeViewport} from '../src/decode.js';
import {FRAME_ARTIFACTS, FrameReader} from '../src/frame.js';
import type {BrowsePage} from '../src/types.js';
import {fixture} from './support.js';

/**
 * The highlight's three columns and the browse verb, against captured bytes.
 *
 * Recorded by `scripts/capture-golden.mjs` from `mosaica serve` over the notebook corpus. The three
 * viewport bodies are one request in three shapes: zoom 3 over the whole of view `s0`, `k = 20`,
 * the taxonomy layer tagging the points, and `filters` on `archive in [cs, math]`, under
 * a principal seeing about half the corpus. Two carry `highlight` on `archive in [cs]`, one of them
 * under `point_rows = "highlight"`, and the third carries none. The capture checks that the three
 * counts are distinct and above zero, so a decoder reading the wrong column fails.
 * `artifacts-highlight.bin` is the taxonomy's artifacts under the same filter and highlight, at
 * zoom 1, from `POST /v1/artifacts/viewport`.
 */

const page = (name: string) => JSON.parse(new TextDecoder().decode(fixture(name))) as Record<string, unknown>;
const sum = (tiles: readonly {visible: bigint; matched: bigint; highlighted: bigint; served: bigint}[], f: 'visible' | 'matched' | 'highlighted' | 'served') =>
  tiles.reduce((n, t) => n + Number(t[f]), 0);

describe('the three highlighted columns, off a served body', () => {
  it('counts a tile’s highlighted inside its matched inside its visible', () => {
    const r = decodeViewport(fixture('viewport-highlight.bin'));
    expect(r.tiles.length).toBeGreaterThan(0);
    // `highlighted <= matched <= visible`, and over the whole body all three differ.
    expect(sum(r.tiles, 'highlighted')).toBeGreaterThan(0);
    expect(sum(r.tiles, 'highlighted')).toBeLessThan(sum(r.tiles, 'matched'));
    expect(sum(r.tiles, 'matched')).toBeLessThan(sum(r.tiles, 'visible'));
    for (const t of r.tiles) {
      expect(t.highlighted).toBeLessThanOrEqual(t.matched);
      expect(t.matched).toBeLessThanOrEqual(t.visible);
    }
  });

  it('carries a bit per served point, and the bit is not the whole set', () => {
    const r = decodeViewport(fixture('viewport-highlight.bin'));
    expect(r.highlighted).not.toBeNull();
    expect(r.highlighted!.length).toBe(r.ids.length);
    // Neither all nor none, so a decoder returning a constant would fail here.
    const lit = r.highlighted!.reduce((a, b) => a + b, 0);
    expect(lit).toBeGreaterThan(0);
    expect(lit).toBeLessThan(r.ids.length);
  });

  it('carries a bit per served artifact, beside the filter’s and different from it', () => {
    const reader = new FrameReader('artifacts');
    const artifacts = reader
      .push(fixture('artifacts-highlight.bin'))
      .filter((f) => f.kind === FRAME_ARTIFACTS)
      .flatMap((f) => decodeArtifactsFrame(f.payload).artifacts);
    const matched = artifacts.filter((a) => a.matched === true).length;
    const lit = artifacts.filter((a) => a.highlighted === true).length;
    // The filter admits some and not all; the highlight lights some of those and not all of them.
    expect(matched).toBeGreaterThan(0);
    expect(matched).toBeLessThan(artifacts.length);
    expect(lit).toBeGreaterThan(0);
    expect(lit).toBeLessThan(matched);
    // The bit is the conjunction with the filter, so nothing the filter refused is lit.
    expect(artifacts.every((a) => !a.highlighted || a.matched)).toBe(true);
    // Not null where the request carried a highlight: every row was asked.
    expect(artifacts.every((a) => a.highlighted !== null)).toBe(true);
  });

  it('answers a request with no highlight by the identity, and sends no bits at all', () => {
    const r = decodeViewport(fixture('viewport-no-highlight.bin'));
    // `highlighted` is present and equal to `matched` where no highlight was asked.
    expect(sum(r.tiles, 'highlighted')).toBe(sum(r.tiles, 'matched'));
    for (const t of r.tiles) expect(t.highlighted).toBe(t.matched);
    // The points column is absent rather than all false: no question was asked.
    expect(r.highlighted).toBeNull();
  });
});

describe('point_rows = "highlight", against the full answer to the same request', () => {
  /**
   * A highlight change re-sends bits and not points, checked against the two bodies a server
   * produced for one request under the two projections.
   */
  it('is the same rows, in the same order, carrying the same bits', () => {
    const full = decodeViewport(fixture('viewport-highlight.bin'));
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    expect(bits.pointsProjection).toBe('highlight');
    expect(full.pointsProjection).toBe('full');
    expect([...bits.ids]).toEqual([...full.ids]);
    expect([...bits.highlighted!]).toEqual([...full.highlighted!]);
  });

  it('is the same per-tile answer: the served split and every count', () => {
    const full = decodeViewport(fixture('viewport-highlight.bin'));
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    expect(bits.tiles).toEqual(full.tiles);
  });

  it('carries no position and no scalar: which is what a client joining by identifier wants', () => {
    const bits = decodeViewport(fixture('viewport-point-rows-highlight.bin'));
    // The projection is read from the frame's schema: the highlight projection has no `code`.
    expect(bits.codes.length).toBe(0);
    expect(bits.positions.length).toBe(0);
    expect(bits.world.length).toBe(0);
    expect(Object.keys(bits.scalars)).toEqual([]);
    expect(bits.ids.length).toBeGreaterThan(0);
  });
});

describe('POST /v1/artifacts/browse, off served pages', () => {
  type Row = {mosaica_id: string; key: string; rung: number; parent_ids: string[]; masked_count: number; matched_count?: number};
  type Page = {artifacts: Row[]; parents: Row[]; next?: string};
  const pages = ['browse-roots.json', 'browse-children-filtered.json', 'browse-search.json'].map((name) => page(name) as unknown as Page);
  const [roots, children, search] = pages as [Page, Page, Page];

  it('serves the roots by masked count descending, paged, `parents` empty on this form', () => {
    // The k-means layer at `limit: 4`, with more roots than that.
    expect(roots.artifacts).toHaveLength(4);
    // `parents` is the children form's and is `[]` on the other two.
    expect(roots.parents).toEqual([]);
    // A cursor over a total order (count descending, then `mosaica_id` ascending), so paging neither
    // repeats nor drops.
    expect(typeof roots.next).toBe('string');
    const counts = roots.artifacts.map((a) => a.masked_count);
    expect([...counts].sort((a, b) => b - a)).toEqual(counts);
  });

  it('serves a root’s children, with a matched count the masked one does not move with', () => {
    // Taken under the HDBSCAN tree's root, filtered to an archive most of its children hold none of.
    expect(children.artifacts.length).toBeGreaterThan(0);
    const parent = children.artifacts[0]!.parent_ids[0]!;
    for (const row of children.artifacts) {
      expect(row.parent_ids).toContain(parent);
      expect(row.rung).toBe(1);
      expect(typeof row.matched_count).toBe('number');
    }
    // Existence and `masked_count` do not move with the filter, and a row the filter admits nothing
    // of is still served.
    expect(children.artifacts.some((row) => row.matched_count === 0 && row.masked_count > 0)).toBe(true);
  });

  it('writes every identifier as a decimal string, which keeps a u64 whole', () => {
    const ids = pages.flatMap((p) => [...p.artifacts, ...p.parents].flatMap((row) => [row.mosaica_id, ...row.parent_ids]));
    for (const id of ids) {
      expect(typeof id).toBe('string');
      expect(BigInt(id).toString()).toBe(id);
    }
    // At least one is past 2^53, where a JSON number would already have lost bits.
    expect(ids.some((id) => BigInt(id) > 2n ** 53n)).toBe(true);
  });

  it('serves a search page with a cursor, and no matched count where no filter was sent', () => {
    expect(search.artifacts).toHaveLength(2);
    expect(typeof search.next).toBe('string');
    // Absent rather than null or zero: there was no question.
    expect(search.artifacts.every((a) => a.matched_count === undefined)).toBe(true);
  });

  it('reads through the client’s own row mapper, identifiers and counts intact', async () => {
    const raw = page('browse-roots.json');
    const {MosaicaClient} = await import('../src/client.js');
    const fetchMock = async () => ({ok: true, status: 200, json: async () => raw, headers: new Headers()});
    const held = globalThis.fetch;
    globalThis.fetch = fetchMock as unknown as typeof globalThis.fetch;
    try {
      const client = new MosaicaClient({viewerUrl: 'http://v', sessionUrl: 'http://s'});
      const decoded: BrowsePage = await client.browse('tok', {view: 's0', layer: 'clusters/kmeans'});
      expect(decoded.artifacts).toHaveLength(roots.artifacts.length);
      const first = roots.artifacts[0]!;
      expect(decoded.artifacts[0]!.mosaicaId).toBe(BigInt(first.mosaica_id));
      expect(decoded.artifacts[0]!.maskedCount).toBe(BigInt(first.masked_count));
      expect(decoded.artifacts[0]!.matchedCount).toBeNull();
      expect(decoded.artifacts[0]!.parentIds).toEqual([]);
      expect(decoded.next).toBe(roots.next);
    } finally {
      globalThis.fetch = held;
    }
  });
});
