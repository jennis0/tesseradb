import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';

/**
 * `POST /v1/artifacts/browse` (`highlight-and-hierarchy.md` §4) as this client speaks it: the
 * three forms, the decimal-string identifiers, and the two counts.
 */

function client(): TesseraClient {
  return new TesseraClient({viewerUrl: 'http://v', sessionUrl: 'http://s'});
}

const row = (id: string, masked: number, extra: Record<string, unknown> = {}) => ({
  tessera_id: id,
  key: `k-${id}`,
  name: `n-${id}`,
  masked_count: masked,
  rung: 0,
  parent_ids: [],
  ...extra
});

function answering(body: unknown, ok = true) {
  const fetchMock = vi.fn(async () => ({
    ok,
    status: ok ? 200 : 422,
    json: async () => body,
    headers: new Headers()
  }));
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

afterEach(() => vi.unstubAllGlobals());

describe('browse', () => {
  it('sends the roots form as the layer alone, and reads identifiers past 2^53', async () => {
    const fetchMock = answering({artifacts: [row('18064038920082622571', 393_741)], parents: [], next: null});
    const page = await client().browse('tok', {layer: 'mesh/descriptors'});
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, {body: string}];
    expect(url).toBe('http://v/v1/artifacts/browse');
    expect(JSON.parse(init.body)).toEqual({layer: 'mesh/descriptors'});
    // A `u64` that a JSON number would have rounded: the wire spells it, and this keeps it.
    expect(page.artifacts[0]!.tesseraId).toBe(18_064_038_920_082_622_571n);
    expect(page.artifacts[0]!.maskedCount).toBe(393_741n);
    // No filter was sent, so there is no matched count — *there was no question*, not zero.
    expect(page.artifacts[0]!.matchedCount).toBeNull();
    expect(page.parents).toEqual([]);
    expect(page.next).toBeNull();
  });

  it('sends the children form with the parent as a decimal string, and reads its parents back', async () => {
    const fetchMock = answering({artifacts: [row('7', 4)], parents: [row('3', 90)], next: 'c2'});
    const page = await client().browse('tok', {layer: 'l', parent: 3n, limit: 50, cursor: 'c1'});
    expect(JSON.parse((fetchMock.mock.calls[0] as unknown as [string, {body: string}])[1].body)).toEqual({
      layer: 'l',
      parent: '3',
      limit: 50,
      cursor: 'c1'
    });
    expect(page.parents.map((p) => p.tesseraId)).toEqual([3n]);
    expect(page.next).toBe('c2');
  });

  it('sends the search form and the viewport’s own filter object, and reads matched_count', async () => {
    const fetchMock = answering({artifacts: [row('9', 100, {matched_count: 0})], parents: [], next: null});
    const page = await client().browse('tok', {layer: 'l', q: 'lymph', filters: {archive: {in: ['cs']}}});
    expect(JSON.parse((fetchMock.mock.calls[0] as unknown as [string, {body: string}])[1].body)).toEqual({
      layer: 'l',
      q: 'lymph',
      filters: {archive: {in: ['cs']}}
    });
    // Zero is a value: existence and the masked count never move with the filter, so a row the
    // filter admits nothing of is still served and still says what it holds.
    expect(page.artifacts[0]!.matchedCount).toBe(0n);
    expect(page.artifacts[0]!.maskedCount).toBe(100n);
  });

  it('throws a TesseraError on a refusal, like every other verb', async () => {
    answering({error: 'unknown_layer', detail: 'no such layer'}, false);
    await expect(client().browse('tok', {layer: 'nope'})).rejects.toThrow(TesseraError);
  });
});
