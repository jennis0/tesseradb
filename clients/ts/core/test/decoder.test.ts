import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {decodeViewport} from '../src/decode.js';
import {setWorkerFactory, workerDecoder} from '../src/decoder.js';
import {FakeWorker, fixture, settle} from './support.js';

const body = () => fixture('viewport-plain.bin');

let workers: FakeWorker[];

beforeEach(() => {
  workers = [];
  vi.stubGlobal('Worker', FakeWorker);
  setWorkerFactory(() => {
    const w = new FakeWorker();
    workers.push(w);
    return w as unknown as Worker;
  });
});

afterEach(() => {
  setWorkerFactory(null);
  vi.unstubAllGlobals();
});

describe('workerDecoder', () => {
  it('holds a request until its worker is ready, then decodes it there', async () => {
    const decoder = workerDecoder()!;
    const decoded = decoder.decode(body());
    expect(workers.every((w) => w.received === 0)).toBe(true);
    for (const w of workers) w.load();
    const result = await decoded;
    expect(result.ids.length).toBe(decodeViewport(body()).ids.length);
    expect(workers.reduce((n, w) => n + w.received, 0)).toBe(1);
    expect(decoder.lastWorkerMs).toBe(1);
  });

  it('decodes on the main thread what a worker that never loaded was holding, and everything after', async () => {
    const decoder = workerDecoder()!;
    const first = decoder.decode(body());
    const second = decoder.decode(body());
    for (const w of workers) w.fail();
    const expected = decodeViewport(body()).ids;
    expect((await first).ids).toEqual(expected);
    expect((await second).ids).toEqual(expected);
    expect((await decoder.decode(body())).ids).toEqual(expected);
    expect(workers.reduce((n, w) => n + w.received, 0)).toBe(0);
    expect(decoder.lastWorkerMs).toBeNull();
  });

  it('rejects what a worker that stopped after loading was holding, and decodes later requests on the main thread', async () => {
    const decoder = workerDecoder()!;
    for (const w of workers) w.load();
    // Replies are queued as microtasks, so the worker fails with both requests outstanding.
    const lost = [decoder.decode(body()), decoder.decode(body())];
    for (const w of workers) w.fail();
    for (const request of lost) await expect(request).rejects.toThrow(Error);
    const received = workers.reduce((n, w) => n + w.received, 0);
    const later = await decoder.decode(body());
    expect(later.ids).toEqual(decodeViewport(body()).ids);
    expect(workers.reduce((n, w) => n + w.received, 0)).toBe(received);
    expect(workers.every((w) => w.terminated)).toBe(true);
  });
});

describe('a decoder passed to a client', () => {
  const fetch = (async () => new Response(body(), {headers: {etag: '"ck"', 'x-tessera-identity-key': 'ik'}})) as typeof globalThis.fetch;

  it('stays open when a client sharing it is closed, and the decodes in flight on it finish', async () => {
    const decoder = workerDecoder()!;
    const one = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: '', fetch, decoder});
    const two = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: '', fetch, decoder});
    const ask = (client: TesseraClient) => client.viewport('tok', {view: 's0', zoom: 0, k: 100});
    const asked = [ask(one), ask(two)];
    await settle();
    one.close();
    for (const w of workers) w.load();
    const ids = decodeViewport(body()).ids;
    for (const answer of await Promise.all(asked)) expect(answer.result.ids).toEqual(ids);
    expect((await ask(two)).result.ids).toEqual(ids);
  });
});
