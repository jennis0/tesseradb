import {afterEach, describe, expect, it, vi} from 'vitest';
import {addressed, Control, MAX_ATTEMPTS, UNANSWERED, type Answer} from '../src/control.js';

/**
 * `Control` against a recording `fetch`: the request each method sends, the `429` retry, and that
 * every other status comes back as an answer. The live file checks the same routes against a server.
 */

type Sent = {method: string; url: string; headers: Record<string, string>; body: unknown};
type Reply = {status: number; body?: unknown; headers?: Record<string, string>};

/** Stubs `fetch` to record each request and answer the replies in turn, the last one for ever. */
function recording(...replies: Reply[]): Sent[] {
  const sent: Sent[] = [];
  vi.stubGlobal('fetch', async (url: string, init: RequestInit = {}) => {
    sent.push({method: init.method ?? 'GET', url, headers: {...(init.headers as Record<string, string>)}, body: init.body});
    const reply = replies[Math.min(sent.length - 1, replies.length - 1)]!;
    return {
      status: reply.status,
      headers: new Headers(reply.headers ?? {}),
      text: async () => (reply.body === undefined ? '' : JSON.stringify(reply.body))
    } as unknown as Response;
  });
  return sent;
}

const control = new Control({controlUrl: 'http://control/', operatorCredential: 'op-cred'});
const rows = new Uint8Array([1, 2, 3, 4]);

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('each route', () => {
  const JSON_ROUTE = {'content-type': 'application/json'};
  const cases: {name: string; call: (c: Control) => Promise<Answer>; method: string; path: string; headers?: Record<string, string>; json?: unknown}[] = [
    {name: 'status', call: (c) => c.status(), method: 'GET', path: '/control/status'},
    {name: 'declareLayer', call: (c) => c.declareLayer({name: 'a/b'}), method: 'PUT', path: '/control/layers', headers: JSON_ROUTE, json: {name: 'a/b'}},
    {name: 'declareViewGroup', call: (c) => c.declareViewGroup('g', {extent: {x: [0, 1], y: [0, 1]}}), method: 'PUT', path: '/control/view_groups/g', headers: JSON_ROUTE, json: {extent: {x: [0, 1], y: [0, 1]}}},
    {name: 'declareAttribute', call: (c) => c.declareAttribute({name: 'note', type: 'keyword'}), method: 'PUT', path: '/control/attributes', headers: JSON_ROUTE, json: {name: 'note', type: 'keyword'}},
    {name: 'declareVocabulary', call: (c) => c.declareVocabulary('v', {width: 'u8'}), method: 'PUT', path: '/control/vocabularies/v', headers: JSON_ROUTE, json: {width: 'u8'}},
    {name: 'vocabularyValues', call: (c) => c.vocabularyValues('v', {values: [{key: 'k'}]}), method: 'PATCH', path: '/control/vocabularies/v/values', headers: JSON_ROUTE, json: {values: [{key: 'k'}]}},
    {name: 'declareView', call: (c) => c.declareView('plain', {extent: {x: [0, 1], y: [0, 1]}}), method: 'PUT', path: '/control/views/plain', headers: JSON_ROUTE, json: {extent: {x: [0, 1], y: [0, 1]}}},
    {name: 'createView', call: (c) => c.createView('g', 'k 1', {metadata: {}}), method: 'PUT', path: '/control/views/g/k%201', headers: JSON_ROUTE, json: {metadata: {}}},
    {name: 'publish', call: (c) => c.publish('clusters/kmeans', {level: 0}), method: 'PUT', path: '/control/layers/clusters%2Fkmeans/artifacts', headers: JSON_ROUTE, json: {level: 0}},
    {name: 'grow (JSON)', call: (c) => c.grow('clusters/kmeans', {level: 0}), method: 'PATCH', path: '/control/layers/clusters%2Fkmeans/artifacts', headers: JSON_ROUTE, json: {level: 0}},
    {name: 'changes', call: (c) => c.changes([{external_id: 'AQ==', op: 'suppress'}]), method: 'POST', path: '/control/changes', headers: JSON_ROUTE, json: [{external_id: 'AQ==', op: 'suppress'}]},
    {name: 'dropLayer', call: (c) => c.dropLayer('a/b'), method: 'DELETE', path: '/control/layers/a%2Fb'},
    {name: 'dropLayer waiting', call: (c) => c.dropLayer('a', {wait: true}), method: 'DELETE', path: '/control/layers/a?wait=visible'},
    {name: 'dropView', call: (c) => c.dropView('g', 'k'), method: 'DELETE', path: '/control/views/g/k'},
    {name: 'dropView with both', call: (c) => c.dropView('g', 'k', {deleteDangling: true, wait: true}), method: 'DELETE', path: '/control/views/g/k?delete_dangling=true&wait=visible'},
    {name: 'compact', call: (c) => c.compact(), method: 'POST', path: '/control/compact'},
    {name: 'flush', call: (c) => c.flush(), method: 'POST', path: '/control/flush'},
    {name: 'flush waiting', call: (c) => c.flush({wait: true}), method: 'POST', path: '/control/flush?wait=visible'}
  ];

  for (const k of cases) {
    it(`${k.name} sends ${k.method} ${k.path} with the operator credential`, async () => {
      const sent = recording({status: 200, body: {}});
      const answer = await k.call(control);
      expect(answer.ok).toBe(true);
      expect(sent).toHaveLength(1);
      const [request] = sent;
      expect(request!.method).toBe(k.method);
      expect(request!.url).toBe(`http://control${k.path}`);
      expect(request!.headers).toMatchObject({authorization: 'Bearer op-cred', ...k.headers});
      if (k.json !== undefined) expect(JSON.parse(request!.body as string)).toEqual(k.json);
    });
  }

  it('grow sends bytes as an Arrow stream', async () => {
    const sent = recording({status: 200, body: {}});
    await control.grow('l', rows);
    expect(sent[0]!.headers['content-type']).toBe('application/vnd.apache.arrow.stream');
    expect(sent[0]!.body).toBe(rows);
  });

  for (const route of ['ingest', 'values'] as const) {
    it(`${route} sends the bytes as given, under the caller's batch id and view`, async () => {
      const sent = recording({status: 200, body: {accepted: 4}});
      const answer = await control[route](rows, {batch: 'page-0', view: 's0'});
      expect(sent[0]).toMatchObject({method: 'POST', url: `http://control/control/${route}`, body: rows});
      expect(sent[0]!.headers).toEqual({
        authorization: 'Bearer op-cred',
        'content-type': 'application/vnd.apache.arrow.stream',
        'x-tessera-batch-id': 'page-0',
        'x-tessera-view': 's0'
      });
      expect(answer.batch).toBe('page-0');
      expect(answer.body).toEqual({accepted: 4});
    });

    it(`${route} names no view unless given one, and makes a fresh batch id for each call`, async () => {
      const sent = recording({status: 200, body: {}});
      const first = await control[route](rows);
      const second = await control[route](rows);
      expect(sent[0]!.headers).not.toHaveProperty('x-tessera-view');
      expect(first.batch).not.toBe(second.batch);
      expect(sent.map((s) => s.headers['x-tessera-batch-id'])).toEqual([first.batch, second.batch]);
    });
  }
});

describe('an answer', () => {
  it('after a 429, waits its Retry-After and resends the same batch id and bytes', async () => {
    vi.useFakeTimers();
    const sent = recording({status: 429, headers: {'retry-after': '2'}}, {status: 429, headers: {'retry-after': '2'}}, {status: 200, body: {accepted: 4}});
    const pending = control.ingest(rows, {view: 's0'});
    await vi.advanceTimersByTimeAsync(1_999);
    expect(sent).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(sent).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(2_000);
    const answer = await pending;
    expect(answer).toMatchObject({status: 200, ok: true, attempts: 3, body: {accepted: 4}});
    expect(sent.map((s) => s.body)).toEqual([rows, rows, rows]);
    expect(new Set(sent.map((s) => s.headers['x-tessera-batch-id']))).toEqual(new Set([answer.batch]));
  });

  it('takes the body’s retry_after_s where a 429 has no header', async () => {
    vi.useFakeTimers();
    const sent = recording({status: 429, body: {error: 'backpressure', retry_after_s: 3}}, {status: 200, body: {}});
    const pending = control.flush();
    await vi.advanceTimersByTimeAsync(2_999);
    expect(sent).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect((await pending).attempts).toBe(2);
  });

  it('is the last 429 once the attempts are spent', async () => {
    vi.useFakeTimers();
    const sent = recording({status: 429, headers: {'retry-after': '0'}, body: {error: 'backpressure'}});
    const pending = control.ingest(rows);
    await vi.runAllTimersAsync();
    const answer = await pending;
    expect(answer).toMatchObject({status: 429, ok: false, attempts: MAX_ATTEMPTS, body: {error: 'backpressure'}});
    expect(sent).toHaveLength(MAX_ATTEMPTS);
    expect(new Set(sent.map((s) => s.headers['x-tessera-batch-id'])).size).toBe(1);
  });

  it('is any other refusal as the server sent it, sent once', async () => {
    const sent = recording({status: 409, body: {error: 'conflict', detail: 'held'}});
    const answer = await control.publish('l', {level: 0});
    expect(answer).toMatchObject({status: 409, ok: false, attempts: 1, body: {error: 'conflict', detail: 'held'}});
    expect(sent).toHaveLength(1);
  });

  it('carries status 0 where no server answered', async () => {
    vi.stubGlobal('fetch', async () => {
      throw new TypeError('fetch failed');
    });
    const answer = await control.changes([{tessera_id: '5', idset: 1, op: 'delete'}]);
    expect(answer).toMatchObject({status: UNANSWERED, ok: false, attempts: 1});
  });

  it('keeps a non-object JSON body under `value`, and a non-JSON body only as text', async () => {
    recording({status: 200, body: [1, 2]});
    expect((await control.status()).body).toEqual({value: [1, 2]});
    vi.stubGlobal('fetch', async () => ({status: 502, headers: new Headers(), text: async () => 'bad gateway'}) as unknown as Response);
    expect(await control.status()).toMatchObject({status: 502, body: {}, detail: 'bad gateway'});
  });

  it('limits is the status body’s limits block', async () => {
    recording({status: 200, body: {limits: {changes: {max_changes_per_request: 10_000}}}});
    expect(await control.limits()).toEqual({changes: {max_changes_per_request: 10_000}});
  });
});

describe('addressed', () => {
  it('is base64 of a string’s UTF-8, a bigint’s eight little-endian bytes, or the bytes given', () => {
    expect(addressed('é')).toBe('w6k=');
    expect(addressed(1n)).toBe('AQAAAAAAAAA=');
    expect(addressed(-1n)).toBe('//////////8=');
    expect(addressed(new Uint8Array([0xff, 0]))).toBe('/wA=');
  });
});
