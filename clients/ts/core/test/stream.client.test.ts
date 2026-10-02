import {afterEach, describe, expect, it, vi} from 'vitest';
import {tableToIPC, Table} from 'apache-arrow';
import {TesseraClient} from '../src/client.js';
import {decodeViewport} from '../src/decode.js';
import {inlineDecoder} from '../src/decoder.js';
import type {ViewportCounts, ViewportPart} from '../src/types.js';
import {chunked, framed, manual, rejectsAsRefused, settle, u64} from './support.js';

/**
 * The streamed viewport: a response read frame by frame as it arrives. The server flushes whole
 * tiles at a size threshold and the trailer ends the response, so the counts and the first tiles
 * are usable before the last points frame arrives. These check that the streamed reading equals
 * the whole-body reading, that it is early, and that an abort and a body without its trailer both
 * fail.
 */

/**
 * Seven tiles over three points frames, two points a tile, and a tile that serves nothing, which a
 * walk over the counts can lose.
 */
const SERVED = [2n, 2n, 0n, 2n, 2n, 2n, 2n];
const PER_FRAME = [2, 2, 3]; // tiles per points frame; frames flush at whole tiles

function bodyBytes(stageNs?: string, subCells = false): Uint8Array {
  const tiles = tableToIPC(
    new Table({
      tile: u64(SERVED.map((_, i) => BigInt(i))),
      visible: u64(SERVED.map((s) => s + 5n)),
      matched: u64(SERVED.map((s) => s + 5n)),
      served: u64(SERVED),
      highlighted: u64(SERVED.map((s) => s + 5n))
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
    JSON.stringify({
      arrow_serialise_ns: 0,
      flushes: points.length,
      points: total,
      stream_us: 0,
      ...(stageNs === undefined ? {} : {stage_ns: stageNs})
    })
  );
  const underlay = tableToIPC(new Table({cell: u64([3n, 9n]), count: u64([4n, 1n])}), 'stream');
  return framed([
    {kind: 1, payload: tiles},
    ...(subCells ? [{kind: 2, payload: underlay}] : []),
    ...points.map((payload) => ({kind: 3, payload})),
    {kind: 4, payload: trailer}
  ]);
}

const HEADERS = {etag: '"c1"', 'x-tessera-identity-key': 'i1'};

const client = () =>
  new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: 'http://session', decoder: inlineDecoder()});

const ask = (c: TesseraClient, onPart: (p: ViewportPart) => void, signal?: AbortSignal) =>
  c.viewport('tok', {view: 's0', zoom: 4, k: 100}, {signal, onPart});

afterEach(() => vi.unstubAllGlobals());

describe('a streamed viewport response', () => {
  it('decodes to the same answer however the chunks fall across the frames', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);

    // 1 splits every header across three chunks and every payload across hundreds; 7 lands mid
    // header and mid payload at every frame; a size past the body is the single-chunk case.
    for (const size of [1, 7, 64, 1_000, body.byteLength * 2]) {
      vi.stubGlobal('fetch', async () => chunked(body, size, {headers: HEADERS}));
      const parts: ViewportPart[] = [];
      const response = await ask(client(), (p) => parts.push(p));

      // The parts partition the tiles batch, in the response's own order.
      expect(parts.flatMap((p) => p.result.tiles.map((t) => t.tile))).toEqual(
        whole.tiles.map((t) => t.tile)
      );
      // Their points concatenate to the whole body's, in the same order.
      const ids = parts.flatMap((p) => [...p.result.ids]);
      expect(ids).toEqual([...whole.ids]);
      expect(parts.flatMap((p) => [...p.result.world])).toEqual([...whole.world]);
      // Each part carries the keys it was served under, so a store can partition on them before the
      // response resolves.
      expect(parts.map((p) => `${p.identityKey}/${p.contentKey}`)).toEqual(parts.map(() => 'i1/c1'));

      // Every point went to the sink, so the response carries none.
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

    const feed = manual(undefined, HEADERS);
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p));

    feed.push(body.subarray(0, lastPoints));
    await settle();
    // Two of the three points frames are in, with their counts, while the third has not been sent.
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

  it('hands over the counts as soon as they land, before any point, with or without a part sink', async () => {
    for (const underlay of [false, true]) {
      for (const onPart of [(): void => {}, undefined]) {
        const body = bodyBytes(undefined, underlay);
        const whole = decodeViewport(body);
        const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
        const afterTiles = 5 + view.getUint32(1, true);
        const afterCounts = underlay ? afterTiles + 5 + view.getUint32(afterTiles + 1, true) : afterTiles;

        const feed = manual(undefined, HEADERS);
        vi.stubGlobal('fetch', async () => feed.response);
        const counts: ViewportCounts[] = [];
        const req = {view: 's0', zoom: 4, k: 100, ...(underlay ? {underlayOffset: 2} : {})};
        const asking = client().viewport('tok', req, {onPart, onCounts: (c) => counts.push(c)});

        feed.push(body.subarray(0, afterTiles));
        await settle();
        // With an underlay asked for, the counts wait for the sub-cells frame that follows.
        expect(counts.length).toBe(underlay ? 0 : 1);
        feed.push(body.subarray(afterTiles, afterCounts));
        await settle();
        expect(counts.length).toBe(1);
        expect(counts[0]!.tiles).toEqual(whole.tiles);
        expect(counts[0]!.subCells).toEqual(whole.subCells);
        expect(`${counts[0]!.identityKey}/${counts[0]!.contentKey}`).toBe('i1/c1');

        feed.push(body.subarray(afterCounts));
        feed.close();
        const response = await asking;
        expect(counts.length).toBe(1);
        expect(response.result.tiles).toEqual(whole.tiles);
      }
    }
  });

  it('hands over the counts once however the chunks split the tiles frame, without a part sink', async () => {
    const body = bodyBytes(undefined, true);
    const whole = decodeViewport(body);
    // 1 and 3 split the tiles frame's five-byte header across reads; 7 splits it mid-payload too.
    for (const size of [1, 3, 7]) {
      vi.stubGlobal('fetch', async () => chunked(body, size, {headers: HEADERS}));
      const counts: ViewportCounts[] = [];
      const response = await client().viewport('tok', {view: 's0', zoom: 4, k: 100, underlayOffset: 2}, {onCounts: (c) => counts.push(c)});
      expect(counts.length).toBe(1);
      expect(counts[0]!.tiles).toEqual(whole.tiles);
      expect(counts[0]!.subCells).toEqual(whole.subCells);
      expect([...response.result.ids]).toEqual([...whole.ids]);
    }
  });

  it('hands over the counts where the transport cannot stream, with or without a part sink', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);
    for (const onPart of [(): void => {}, undefined]) {
      vi.stubGlobal('fetch', async () => ({
        ok: true,
        body: null,
        headers: new Headers(HEADERS),
        arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength)
      }));
      const counts: ViewportCounts[] = [];
      const response = await client().viewport('tok', {view: 's0', zoom: 4, k: 100}, {onPart, onCounts: (c) => counts.push(c)});
      expect(counts.map((c) => c.tiles)).toEqual([whole.tiles]);
      expect(response.result.tiles).toEqual(whole.tiles);
    }
  });

  it('keeps the counts it handed over when the request is aborted before the points', async () => {
    const body = bodyBytes();
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    const afterTiles = 5 + view.getUint32(1, true);
    for (const withParts of [true, false]) {
      const controller = new AbortController();
      const feed = manual(controller.signal, HEADERS);
      vi.stubGlobal('fetch', async () => feed.response);
      const counts: ViewportCounts[] = [];
      const parts: ViewportPart[] = [];
      const asking = client().viewport('tok', {view: 's0', zoom: 4, k: 100}, {
        signal: controller.signal,
        onPart: withParts ? (p) => void parts.push(p) : undefined,
        onCounts: (c) => counts.push(c)
      });
      feed.push(body.subarray(0, afterTiles));
      await settle();
      expect(counts.length).toBe(1);

      controller.abort();
      feed.push(body.subarray(afterTiles));
      await expect(asking).rejects.toThrow();
      await settle();
      // No point arrived and nothing more is handed over.
      expect(counts.length).toBe(1);
      expect(parts.length).toBe(0);
    }
  });

  it('lands nothing more once the request is aborted mid-stream', async () => {
    const body = bodyBytes();
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    const starts: number[] = [];
    for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) starts.push(at);
    const secondPoints = starts[2]!;

    const controller = new AbortController();
    const feed = manual(controller.signal, HEADERS);
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p), controller.signal);

    feed.push(body.subarray(0, secondPoints));
    await settle();
    expect(parts.length).toBe(1);

    controller.abort();
    // The rest of the body would decode; nothing reads it.
    feed.push(body.subarray(secondPoints));
    await expect(asking).rejects.toThrow();
    await settle();
    expect(parts.length).toBe(1);
    // What landed stays: whole tiles from one snapshot. The caller gets no completed response, so
    // the region is not marked covered.
    expect(parts[0]!.result.ids.length).toBe(4);
  });

  it('refuses a body that ends without its trailer, after handing over what did arrive', async () => {
    const body = bodyBytes();
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    let trailerAt = 0;
    for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) {
      if (view.getUint8(at) === 4) trailerAt = at;
    }
    const feed = manual(undefined, HEADERS);
    vi.stubGlobal('fetch', async () => feed.response);
    const parts: ViewportPart[] = [];
    const asking = ask(client(), (p) => parts.push(p));

    feed.push(body.subarray(0, trailerAt));
    feed.close();
    await rejectsAsRefused(asking);
    // Every points frame arrived and is drawable, and the response is still refused: the trailer is
    // what says it is complete.
    expect(parts.length).toBe(3);
  });

  it('refuses a body cut inside a frame rather than serving the short answer', async () => {
    const body = bodyBytes();
    vi.stubGlobal('fetch', async () => chunked(body.subarray(0, body.byteLength - 12), 64, {headers: HEADERS}));
    await expect(ask(client(), () => {})).rejects.toThrow();
  });

  it('takes the whole body where the transport cannot stream, and still fills the sink', async () => {
    const body = bodyBytes();
    const whole = decodeViewport(body);
    // A `fetch` with no `body` stream hands the sink everything at once.
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
    vi.stubGlobal('fetch', async () => chunked(body, 100, {headers: HEADERS}));
    const response = await client().viewport('tok', {view: 's0', zoom: 4, k: 100});
    expect([...response.result.ids]).toEqual([...whole.ids]);
    expect(response.bytes).toBe(body.byteLength);
  });

  it('reports the trailer\'s stage timings on the streamed and the whole-body paths, and none where it has none', async () => {
    for (const onPart of [(): void => {}, undefined]) {
      vi.stubGlobal('fetch', async () => chunked(bodyBytes('10,20,30'), 64, {headers: HEADERS}));
      const staged = await client().viewport('tok', {view: 's0', zoom: 4, k: 100}, {onPart});
      expect(staged.timings.stageNs).toEqual([10, 20, 30]);
      vi.stubGlobal('fetch', async () => chunked(bodyBytes(), 64, {headers: HEADERS}));
      const plain = await client().viewport('tok', {view: 's0', zoom: 4, k: 100}, {onPart});
      expect(plain.timings.stageNs).toBeNull();
    }
  });
});
