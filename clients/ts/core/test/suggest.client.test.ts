import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';

/**
 * `TesseraClient.suggest`: `GET /v1/categories/{column}/suggest` (`value-suggestion.md` §5.1,
 * contracts §3.2). The server does not exist yet, so every case here is a fake `fetch` — the
 * shape of the request the client composes, and what it makes of what comes back.
 */

function jsonResponse(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {status, headers: {'content-type': 'application/json', ...headers}});
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('TesseraClient.suggest', () => {
  it('composes q, limit, counts and view onto the suggest route', async () => {
    let seenUrl = '';
    let seenAuth = '';
    vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
      seenUrl = url;
      seenAuth = (init?.headers as Record<string, string>).authorization;
      return jsonResponse(200, {column: 'primary_category', q: 'mach', values: [], more: false});
    });
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
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
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
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
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
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

  it('surfaces a 429 as a typed superseded result, never a thrown error', async () => {
    vi.stubGlobal(
      'fetch',
      async () => jsonResponse(429, {error: 'backpressure', detail: 'one suggest in flight', retry_after_s: 2}, {'Retry-After': '2'})
    );
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const result = await client.suggest('tok', 'primary_category', 'ma');
    expect(result).toEqual({status: 'superseded', retryAfterS: 2});
  });

  it('falls back to the Retry-After header when a 429 body will not parse', async () => {
    vi.stubGlobal('fetch', async () => new Response('not json', {status: 429, headers: {'Retry-After': '3'}}));
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    const result = await client.suggest('tok', 'primary_category', 'ma');
    expect(result).toEqual({status: 'superseded', retryAfterS: 3});
  });

  it('throws TesseraError for a real refusal, e.g. 500 fail-closed on a derived column', async () => {
    vi.stubGlobal('fetch', async () => jsonResponse(500, {error: 'fail-closed', detail: 'primary_category postings unreadable'}));
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: ''});
    await expect(client.suggest('tok', 'primary_category', 'ma')).rejects.toThrow(TesseraError);
  });
});
