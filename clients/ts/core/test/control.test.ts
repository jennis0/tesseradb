import {afterEach, describe, expect, it, vi} from 'vitest';
import {addressed, Control, MAX_ATTEMPTS, MAX_BACKOFF, MIN_BACKOFF, UNANSWERED, type Answer, type WriteOptions} from '../src/control.js';
import {headersOf} from './support.js';

/**
 * `Control` against a recording `fetch`: the request each method sends, the `429` retry, and that
 * every other status comes back as an answer. The live file checks the same routes against a server.
 */

type Sent = {method: string; url: string; headers: Record<string, string>; body: unknown; signal: AbortSignal | undefined};
type Reply = {status: number; body?: unknown; headers?: Record<string, string>};

/** Stubs `fetch` to record each request and answer the replies in turn, the last one for ever. */
function recording(...replies: Reply[]): Sent[] {
  const sent: Sent[] = [];
  vi.stubGlobal('fetch', async (url: string, init: RequestInit = {}) => {
    sent.push({method: init.method ?? 'GET', url, headers: headersOf(init), body: init.body, signal: init.signal ?? undefined});
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

  const writes: {name: string; call: (c: Control, o: WriteOptions) => Promise<Answer>; path: string}[] = [
    {name: 'ingest', call: (c, o) => c.ingest(rows, o), path: '/control/ingest'},
    {name: 'values', call: (c, o) => c.values(rows, o), path: '/control/values'},
    {name: 'declareLayer', call: (c, o) => c.declareLayer({}, o), path: '/control/layers'},
    {name: 'declareViewGroup', call: (c, o) => c.declareViewGroup('g', {}, o), path: '/control/view_groups/g'},
    {name: 'declareAttribute', call: (c, o) => c.declareAttribute({}, o), path: '/control/attributes'},
    {name: 'declareVocabulary', call: (c, o) => c.declareVocabulary('v', {}, o), path: '/control/vocabularies/v'},
    {name: 'vocabularyValues', call: (c, o) => c.vocabularyValues('v', {}, o), path: '/control/vocabularies/v/values'},
    {name: 'declareView', call: (c, o) => c.declareView('p', {}, o), path: '/control/views/p'},
    {name: 'createView', call: (c, o) => c.createView('g', 'k', {}, o), path: '/control/views/g/k'},
    {name: 'publish', call: (c, o) => c.publish('l', {}, o), path: '/control/layers/l/artifacts'},
    {name: 'grow', call: (c, o) => c.grow('l', rows, o), path: '/control/layers/l/artifacts'},
    {name: 'changes', call: (c, o) => c.changes([], o), path: '/control/changes'},
    {name: 'dropLayer', call: (c, o) => c.dropLayer('l', o), path: '/control/layers/l'},
    {name: 'dropView', call: (c, o) => c.dropView('g', 'k', o), path: '/control/views/g/k'},
    {name: 'flush', call: (c, o) => c.flush(o), path: '/control/flush'}
  ];

  for (const w of writes) {
    it(`${w.name} asks for wait=visible only when told to, and hands fetch the caller's signal`, async () => {
      const sent = recording({status: 200, body: {}});
      const signal = new AbortController().signal;
      await w.call(control, {});
      await w.call(control, {wait: true, signal});
      expect(sent.map((s) => s.url)).toEqual([`http://control${w.path}`, `http://control${w.path}?wait=visible`]);
      expect(sent.map((s) => s.signal)).toEqual([undefined, signal]);
    });
  }

  it('sends every route through the host’s fetch, with its headers beside the route’s own', async () => {
    vi.stubGlobal('fetch', async () => {
      throw new Error('the global fetch was used');
    });
    const sent: Record<string, string>[] = [];
    const hosted = async (_url: string | URL | Request, init?: RequestInit) => {
      sent.push(headersOf(init));
      return new Response('{}', {status: 200});
    };
    const c = new Control({controlUrl: 'http://control', operatorCredential: 'op-cred', fetch: hosted as typeof fetch, headers: {'X-Host': 'script', Authorization: 'Bearer host'}});
    const calls = [...cases.map((k) => k.call), ...writes.map((w) => (c: Control) => w.call(c, {})), (c: Control) => c.grow('l', rows)];
    for (const call of calls) {
      sent.length = 0;
      expect((await call(c)).ok).toBe(true);
      expect(sent).toHaveLength(1);
      expect(sent[0]).toMatchObject({'x-host': 'script', authorization: 'Bearer op-cred'});
    }
  });

  it('grow sends bytes as an Arrow stream', async () => {
    const sent = recording({status: 200, body: {}});
    await control.grow('l', rows);
    expect(sent[0]!.headers['content-type']).toBe('application/vnd.apache.arrow.stream');
    expect(sent[0]!.body).toBe(rows);
  });

  for (const route of ['ingest', 'values'] as const) {
    it(`${route} sends the bytes as given, under the caller's batch id and view`, async () => {
      const sent = recording({status: 200, body: {created: 4}});
      const answer = await control[route](rows, {batch: 'page-0', view: 's0'});
      expect(sent[0]).toMatchObject({method: 'POST', url: `http://control/control/${route}`, body: rows});
      expect(sent[0]!.headers).toEqual({
        authorization: 'Bearer op-cred',
        'content-type': 'application/vnd.apache.arrow.stream',
        'x-tessera-batch-id': 'page-0',
        'x-tessera-view': 's0'
      });
      expect(answer.batch).toBe('page-0');
      expect(answer.body).toEqual({created: 4});
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
    const sent = recording({status: 429, headers: {'retry-after': '2'}}, {status: 429, headers: {'retry-after': '2'}}, {status: 200, body: {created: 4}});
    const pending = control.ingest(rows, {view: 's0'});
    await vi.advanceTimersByTimeAsync(1_999);
    expect(sent).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(sent).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(2_000);
    const answer = await pending;
    expect(answer).toMatchObject({status: 200, ok: true, attempts: 3, body: {created: 4}});
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
    const sent = recording({status: 429, headers: {'retry-after': '1'}, body: {error: 'backpressure'}});
    const pending = control.ingest(rows);
    await vi.runAllTimersAsync();
    const answer = await pending;
    expect(answer).toMatchObject({status: 429, ok: false, attempts: MAX_ATTEMPTS, body: {error: 'backpressure'}});
    expect(sent).toHaveLength(MAX_ATTEMPTS);
    expect(new Set(sent.map((s) => s.headers['x-tessera-batch-id'])).size).toBe(1);
  });

  for (const [asked, waited] of [
    ['0', MIN_BACKOFF],
    ['100', MAX_BACKOFF]
  ] as const) {
    it(`after a 429 asking for ${asked} s, waits ${waited} s`, async () => {
      vi.useFakeTimers();
      const sent = recording({status: 429, headers: {'retry-after': asked}}, {status: 200, body: {}});
      const pending = control.status();
      await vi.advanceTimersByTimeAsync(waited * 1000 - 1);
      expect(sent).toHaveLength(1);
      await vi.advanceTimersByTimeAsync(1);
      expect((await pending).attempts).toBe(2);
    });
  }

  it('ends a 429 wait when the signal aborts, rejecting with its reason and sending nothing more', async () => {
    vi.useFakeTimers();
    const sent = recording({status: 429, headers: {'retry-after': '5'}});
    const controller = new AbortController();
    const pending = control.flush({signal: controller.signal});
    const outcome = pending.then(
      () => 'resolved',
      (reason: unknown) => reason
    );
    await vi.advanceTimersByTimeAsync(1_000);
    const reason = new Error('stop');
    controller.abort(reason);
    expect(await outcome).toBe(reason);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(sent).toHaveLength(1);
  });

  it('rejects with the signal’s reason when it aborts a request in flight', async () => {
    vi.stubGlobal('fetch', (_url: string, init: RequestInit) =>
      new Promise((_resolve, reject) => init.signal!.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError'))))
    );
    const controller = new AbortController();
    const pending = control.status({signal: controller.signal});
    const reason = new Error('stop');
    controller.abort(reason);
    await expect(pending).rejects.toBe(reason);
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
    const answer = await control.changes([{tessera_id: '5', op: 'delete'}]);
    expect(answer).toMatchObject({status: UNANSWERED, ok: false, attempts: 1});
  });

  it('keeps a non-object JSON body under `value`, and a non-JSON body only as text', async () => {
    recording({status: 200, body: [1, 2]});
    expect((await control.status()).body).toEqual({value: [1, 2]});
    vi.stubGlobal('fetch', async () => ({status: 502, headers: new Headers(), text: async () => 'bad gateway'}) as unknown as Response);
    expect(await control.status()).toMatchObject({status: 502, body: {}, text: 'bad gateway'});
  });

  it('limits is the status body’s limits block', async () => {
    recording({status: 200, body: {limits: {changes: {max_changes_per_request: 10_000}}}});
    expect(await control.limits()).toEqual({changes: {max_changes_per_request: 10_000}});
  });
});

describe('addressed', () => {
  it('is base64 of a string’s UTF-8, an integer’s eight little-endian bytes, or the bytes given', () => {
    expect(addressed('é')).toBe('w6k=');
    expect(addressed(1n)).toBe('AQAAAAAAAAA=');
    expect(addressed(1)).toBe('AQAAAAAAAAA=');
    expect(addressed(-1n)).toBe('//////////8=');
    expect(addressed(new Uint8Array([0xff, 0]))).toBe('/wA=');
  });

  it('takes every integer in [-2^63, 2^64) and refuses those outside it', () => {
    expect(addressed(2n ** 64n - 1n)).toBe('//////////8=');
    expect(addressed(-(2n ** 63n))).toBe('AAAAAAAAAIA=');
    expect(() => addressed(2n ** 64n)).toThrow(RangeError);
    expect(() => addressed(-(2n ** 63n) - 1n)).toThrow(RangeError);
  });

  it('refuses a number that is not a safe integer', () => {
    expect(() => addressed(2 ** 53)).toThrow(RangeError);
    expect(() => addressed(1.5)).toThrow(RangeError);
    expect(() => addressed(Number.NaN)).toThrow(RangeError);
    expect(addressed(Number.MAX_SAFE_INTEGER)).toBe(addressed(BigInt(Number.MAX_SAFE_INTEGER)));
  });

  it('refuses any other type', () => {
    for (const other of [null, undefined, {}, [1], true]) {
      expect(() => addressed(other as unknown as string)).toThrow(TypeError);
    }
  });
});
