import {afterEach, describe, expect, it, vi} from 'vitest';
import {MosaicaClient, MosaicaError} from '../src/client.js';
import type {FilterExpr} from '../src/types.js';

/**
 * `MosaicaClient.suggest`: `GET /v1/categories/{column}/suggest`, against a fake `fetch`. Checks the
 * request the client composes and what it makes of the reply.
 */

function jsonResponse(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {status, headers: {'content-type': 'application/json', ...headers}});
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('MosaicaClient.suggest', () => {
  it('composes q, limit, counts and view onto the suggest route', async () => {
    let seenUrl = '';
    let seenAuth = '';
    vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
      seenUrl = url;
      seenAuth = new Headers(init?.headers).get('authorization')!;
      return jsonResponse(200, {column: 'primary_category', q: 'mach', values: [], more: false});
    });
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    await client.suggest('tok', 'primary_category', 'mach', {limit: 5, counts: true, view: 'g:k'});
    expect(seenUrl).toBe('http://viewer/v1/categories/primary_category/suggest?q=mach&limit=5&counts=true&view=g%3Ak');
    expect(seenAuth).toBe('Bearer tok');
  });

  it('encodes the column name, and omits limit/counts/view when not given', async () => {
    let seenUrl = '';
    vi.stubGlobal('fetch', async (url: string) => {
      seenUrl = url;
      return jsonResponse(200, {column: 'a/b', q: '', values: [], more: false});
    });
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    await client.suggest('tok', 'a/b', '');
    expect(seenUrl).toBe('http://viewer/v1/categories/a%2Fb/suggest?q=');
  });

  it('maps a landed page: match span, absent title, and count only when carried', async () => {
    vi.stubGlobal('fetch', async () =>
      jsonResponse(200, {
        column: 'primary_category',
        q: 'mach',
        values: [
          {code: 41207, key: 'cs.LG', title: 'Machine Learning', match: {field: 'title', start: 0, len: 4}, count: 18342},
          {code: 9, key: 'stat.ML', match: {field: 'key', start: 0, len: 4}}
        ],
        more: true
      })
    );
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const result = await client.suggest('tok', 'primary_category', 'mach');
    expect(result).toEqual({
      status: 'ok',
      column: 'primary_category',
      q: 'mach',
      values: [
        {code: 41207, key: 'cs.LG', title: 'Machine Learning', match: {field: 'title', start: 0, len: 4}, count: 18342},
        {code: 9, key: 'stat.ML', title: null, match: {field: 'key', start: 0, len: 4}}
      ],
      more: true
    });
  });

  it('sends filters as a POST body with the other fields, and reads the region verdict', async () => {
    let seenUrl = '';
    let seenInit: RequestInit | undefined;
    vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
      seenUrl = url;
      seenInit = init;
      return jsonResponse(
        200,
        {column: 'archive', q: 'a', values: [{code: 11, key: 'astro', match: {field: 'key', start: 0, len: 1}, count: 0}], more: false, total: 40},
        {'x-mosaica-region': 'cover; depth=12'}
      );
    });
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const filters: FilterExpr = {all_of: [{department: {in: ['d01']}}, {region: {bbox: [0, 0, 10, 10]}}]};
    const result = await client.suggest('tok', 'archive', 'a', {counts: true, view: 's0', filters});
    expect(seenUrl).toBe('http://viewer/v1/categories/archive/suggest');
    expect(seenInit?.method).toBe('POST');
    expect(new Headers(seenInit?.headers).get('authorization')).toBe('Bearer tok');
    expect(JSON.parse(seenInit?.body as string)).toEqual({q: 'a', filters, counts: true, view: 's0'});
    expect(result).toEqual({
      status: 'ok',
      column: 'archive',
      q: 'a',
      values: [{code: 11, key: 'astro', title: null, match: {field: 'key', start: 0, len: 1}, count: 0}],
      more: false,
      total: 40,
      region: {exact: false, depth: 12}
    });
  });

  it('surfaces a 429 as a typed shed result, never a thrown error', async () => {
    vi.stubGlobal(
      'fetch',
      async () => jsonResponse(429, {error: 'backpressure', detail: 'one suggest in flight', retry_after_s: 2}, {'Retry-After': '2'})
    );
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const result = await client.suggest('tok', 'primary_category', 'ma');
    expect(result).toEqual({status: 'shed', retryAfterS: 2, detail: 'one suggest in flight'});
  });

  it('falls back to the Retry-After header when a 429 body will not parse', async () => {
    vi.stubGlobal('fetch', async () => new Response('not json', {status: 429, headers: {'Retry-After': '3'}}));
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const result = await client.suggest('tok', 'primary_category', 'ma');
    expect(result).toEqual({status: 'shed', retryAfterS: 3, detail: null});
  });

  it('throws MosaicaError for a real refusal, e.g. 500 fail-closed on a derived column', async () => {
    vi.stubGlobal('fetch', async () => jsonResponse(500, {error: 'fail-closed', detail: 'primary_category postings unreadable'}));
    const client = new MosaicaClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    await expect(client.suggest('tok', 'primary_category', 'ma')).rejects.toThrow(MosaicaError);
  });
});
