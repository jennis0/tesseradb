import {afterEach, describe, expect, it, vi} from 'vitest';
import {makeData, makeVector, tableToIPC, Table, Uint64} from 'apache-arrow';
import {TesseraClient} from '../src/client.js';
import {decodeViewport} from '../src/decode.js';
import {inlineDecoder} from '../src/decoder.js';
import type {ViewportPart} from '../src/types.js';

/**
 * The streamed viewport: a response landed frame by frame instead of body by body.
 *
 * **Slow data should cause pop-in, not lag.** The wire streams — the server flushes whole tiles at
 * a size threshold and the trailer ends the response (`streamed-serving.md` §2, §6) — and the
 * client reads it as it arrives, so the counts and the first tiles are usable long before the last
 * of a hundred point frames has been received. What these pin is that the streamed reading is
 * *equal* to the whole-body reading, that it is genuinely early, and that the two loud failures —
 * an abort and a body without its trailer — stay loud.
 */

/** A framed body: `u8 kind, u32 LE length, payload`, repeated (frame.ts). */
function frame(parts: {kind: number; payload: Uint8Array}[]): Uint8Array {
  const total = parts.reduce((n, p) => n + 5 + p.payload.length, 0);
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let at = 0;
  for (const {kind, payload} of parts) {
    out[at] = kind;
    view.setUint32(at + 1, payload.length, true);
    out.set(payload, at + 5);
    at += 5 + payload.length;
  }
  return out;
}

function u64(values: bigint[]) {
  return makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
}

/**
 * Seven tiles over three points frames, two points a tile — and a tile the definition serves
 * nothing for, because a zero-served tile is the one thing a walk over the counts can lose.
 */
const SERVED = [2n, 2n, 0n, 2n, 2n, 2n, 2n];
const PER_FRAME = [2, 2, 3]; // tiles per points frame; frames flush at whole tiles

function bodyBytes(): Uint8Array {
  const tiles = tableToIPC(
    new Table({
      tile: u64(SERVED.map((_, i) => BigInt(i))),
      visible: u64(SERVED.map((s) => s + 5n)),
      matched: u64(SERVED.map((s) => s + 5n)),
      served: u64(SERVED)
    }),
    'stream'
  );
  let id = 1n;
  let tileAt = 0;
  const points: Uint8Array[] = [];
  for (const tilesHere of PER_FRAME) {
    const ids: bigint[] = [];
    for (let t = 0; t < tilesHere; t++, tileAt++) {
      for (let p = 0; p < Number(SERVED[tileAt]!); p++) ids.push(id++);
    }
    points.push(
      tableToIPC(new Table({tessera_id: u64(ids), code: u64(ids.map((v) => v * 7n))}), 'stream')
    );
  }
  const total = Number(SERVED.reduce((a, b) => a + b, 0n));
  const trailer = new TextEncoder().encode(
    JSON.stringify({arrow_serialise_ns: 0, flushes: points.length, points: total, stream_us: 0})
  );
  return frame([
    {kind: 1, payload: tiles},
    ...points.map((payload) => ({kind: 3, payload})),
    {kind: 4, payload: trailer}
  ]);
}

const HEADERS = {etag: '"c1"', 'x-tessera-identity-key': 'i1'};

/** A response whose body arrives in fixed-size chunks — the sizes are what split the frames. */
function chunked(body: Uint8Array, size: number): Response {
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (let at = 0; at < body.byteLength; at += size) {
        controller.enqueue(body.subarray(at, Math.min(body.byteLength, at + size)));
      }
      controller.close();
    }
  });
  return new Response(stream, {status: 200, headers: HEADERS});
}

/** A response whose body the test feeds by hand, so "not yet arrived" is a state it can hold. */
function manual(signal?: AbortSignal): {
  response: Response;
  push: (bytes: Uint8Array) => void;
  close: () => void;
} {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    }
  });
  // What a real `fetch` does to a body when its request is aborted: the reader's next read
  // rejects, rather than the stream quietly ending.
  signal?.addEventListener('abort', () => {
    try {
      controller.error(new DOMException('The operation was aborted.', 'AbortError'));
    } catch {
      // Already closed — the abort raced the trailer, and the response stands.
    }
  });
  return {
    response: new Response(stream, {status: 200, headers: HEADERS}),
    push: (bytes) => {
      try {
        controller.enqueue(bytes);
      } catch {
        // Errored by an abort: the transport is refusing the rest of the body, which is the
        // state the abort test is asserting about rather than a failure of the test.
      }
    },
    close: () => controller.close()
  };
}

const client = () =>
  new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: 'http://session', decoder: inlineDecoder()});

const ask = (c: TesseraClient, onPart: (p: ViewportPart) => void, signal?: AbortSignal) =>
  c.viewport('tok', {view: 's0', zoom: 4, k: 100}, signal, false, onPart);

/** Let the read loop, the decode chain and the sink run to a standstill. */
async function settle(times = 8): Promise<void> {
  for (let i = 0; i < times; i++) await new Promise((r) => setTimeout(r, 0));
}

afterEach(() => vi.unstubAllGlobals());

describe('a streamed viewport response', () => {
  it('decodes to the same answer however the chunks fall across the frames', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);

    // 1 splits every header across three chunks and every payload across hundreds; 7 lands mid
    // header and mid payload at every frame; a size past the body is the single-chunk case.
    for (const size of [1, 7, 64, 1_000, body.byteLength * 2]) {
      vi.stubGlobal('fetch', async () => chunked(body, size));
      const parts: ViewportPart[] = [];
      const response = await ask(client(), (p) => parts.push(p));

      // The parts partition the tiles batch, in the response's own order.
      expect(parts.flatMap((p) => p.result.tiles.map((t) => t.tile))).toEqual(
        whole.tiles.map((t) => t.tile)
      );
      // ...and their points concatenate to exactly the whole body's, in the same order.
      const ids = parts.flatMap((p) => [...p.result.ids]);
      expect(ids).toEqual([...whole.ids]);
      expect(parts.flatMap((p) => [...p.result.world])).toEqual([...whole.world]);
      // Each part carries the coordinates it was served under, so a store can partition on them
      // before the response has resolved.
      expect(parts.map((p) => `${p.identityKey}/${p.contentKey}`)).toEqual(parts.map(() => 'i1/c1'));

      // Every point went to the sink, so the response carries none — handing them over twice
      // would double both the memory and the absorbing.
      expect(response.result.ids.length).toBe(0);
      expect(response.result.tiles.map((t) => t.tile)).toEqual(whole.tiles.map((t) => t.tile));
      expect(response.bytes).toBe(body.byteLength);
      expect(response.contentKey).toBe('c1');
      expect(response.identityKey).toBe('i1');
    }
  });

  it('hands over the counts and the first tiles before the last points frame has arrived', async () => {
    const body = bodyBytes();
    // Everything up to the last points frame, found by walking the frames the way a reader does.
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    const starts: number[] = [];
    for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) starts.push(at);
    const lastPoints = starts[starts.length - 2]!;

    const feed = manual();
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p));

    feed.push(body.subarray(0, lastPoints));
    await settle();
    // Two of the three point frames are in, with the counts they satisfy — while the third has
    // not been sent at all. Whole-body reading could not have drawn any of this.
    expect(parts.length).toBe(2);
    expect(parts[0]!.result.tiles.length).toBe(2);
    expect(parts[0]!.result.ids.length).toBe(4);
    // Frame 2 covers the zero-served tile and the one after it, so six points are in.
    expect(parts.flatMap((p) => [...p.result.ids])).toEqual([1n, 2n, 3n, 4n, 5n, 6n]);

    feed.push(body.subarray(lastPoints));
    feed.close();
    await asking;
    expect(parts.length).toBe(3);
  });

  it('lands nothing more once the request is aborted mid-stream', async () => {
    const body = bodyBytes();
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    const starts: number[] = [];
    for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) starts.push(at);
    const secondPoints = starts[2]!;

    const controller = new AbortController();
    const feed = manual(controller.signal);
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p), controller.signal);

    feed.push(body.subarray(0, secondPoints));
    await settle();
    expect(parts.length).toBe(1);

    controller.abort();
    // The rest of the body would decode perfectly well; nothing must read it.
    feed.push(body.subarray(secondPoints));
    await expect(asking).rejects.toThrow();
    await settle();
    expect(parts.length).toBe(1);
    // What did land stays sound — it is a whole tile's worth of points from the one snapshot, and
    // a caller keeps it. What it never gets is a completed response to mark the region covered on.
    expect(parts[0]!.result.ids.length).toBe(4);
  });

  it('refuses a body that ends without its trailer, after handing over what did arrive', async () => {
    const body = bodyBytes();
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    let trailerAt = 0;
    for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) {
      if (view.getUint8(at) === 4) trailerAt = at;
    }
    const feed = manual();
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p));

    feed.push(body.subarray(0, trailerAt));
    feed.close();
    await expect(asking).rejects.toThrow(/trailer/);
    // Well-framed and incomplete: every points frame was delivered and is drawable, and the
    // response is still refused, because the trailer's presence is the completeness signal.
    expect(parts.length).toBe(3);
  });

  it('refuses a body cut inside a frame rather than serving the short answer', async () => {
    const body = bodyBytes();
    vi.stubGlobal('fetch', async () => chunked(body.subarray(0, body.byteLength - 12), 64));
    await expect(ask(client(), () => {})).rejects.toThrow();
  });

  it('takes the whole body where the transport cannot stream, and still fills the sink', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);
    // A polyfilled or mocked `fetch` with no `body`: the sink is handed everything in one piece.
    // The difference is when, never what.
    vi.stubGlobal('fetch', async () => ({
      ok: true,
      body: null,
      headers: new Headers(HEADERS),
      arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength)
    }));
    const parts: ViewportPart[] = [];
    const response = await ask(client(), (p) => parts.push(p));
    expect(parts.length).toBe(1);
    expect([...parts[0]!.result.ids]).toEqual([...whole.ids]);
    expect(response.result.ids.length).toBe(0);
  });

  it('reads the whole body when no sink is given, as every other caller does', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);
    vi.stubGlobal('fetch', async () => chunked(body, 100));
    const response = await client().viewport('tok', {view: 's0', zoom: 4, k: 100});
    expect([...response.result.ids]).toEqual([...whole.ids]);
    expect(response.bytes).toBe(body.byteLength);
  });
});
