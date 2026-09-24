import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {decodeViewport} from '../src/decode.js';
import {setWorkerFactory, workerDecoder} from '../src/decoder.js';

const body = () => new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', 'viewport-plain.bin')));

/**
 * A worker that answers like the real one: `ready` when told to load, then each request decoded
 * in-process. `fail()` fires the worker's error event.
 */
class FakeWorker {
  onmessage: ((event: {data: unknown}) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  received = 0;
  terminated = false;
  load(): void {
    this.onmessage?.({data: {ready: true}});
  }
  fail(): void {
    this.onerror?.({message: undefined});
  }
  postMessage(message: {id: number; bytes: ArrayBuffer}): void {
    this.received++;
    const result = decodeViewport(new Uint8Array(message.bytes));
    queueMicrotask(() => this.onmessage?.({data: {id: message.id, result, ms: 1}}));
  }
  terminate(): void {
    this.terminated = true;
  }
}

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
