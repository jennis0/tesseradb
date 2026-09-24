import {Binary, Field, Float64, List, Table, tableToIPC, Utf8, vectorFromArray} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {addressed, Control} from '../src/control.js';
import type {Meta, ViewportRequest} from '../src/types.js';
import {start, type Served} from './served.js';

/**
 * `Control` against a real `tessera serve` over the notebook corpus (`served.ts` builds and starts
 * it), each write read back through the viewer plane. The rows written here carry a label no
 * notebook item carries, and a session holding only that label sees exactly them. A session resolves
 * its terms when it is authorised, so the one that reads the rows is authorised after they land.
 *
 * The tests run in order and share the rows: each one starts from what the one before it left.
 */

const TERM = 'ts-live-control';
const IDS = ['row-0', 'row-1', 'row-2', 'row-3'];
const LAYER = 'ts-live/picks';

let served: Served | string = 'the server has not started';
let client: TesseraClient;
let control: Control;
let meta: Meta;
let token: string;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  client = new TesseraClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.sessionCredential});
  control = new Control({controlUrl: served.controlUrl, operatorCredential: served.operatorCredential});
  token = (await client.authorise([TERM])).token;
  meta = await client.meta(token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

const bytes = (id: string) => new TextEncoder().encode(id);

/** The four rows, near the middle of view `s0`, each labelled `TERM`. */
function points(): Uint8Array {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const x = (q.xMin + q.xMax) / 2;
  const y = (q.yMin + q.yMax) / 2;
  const table = new Table({
    external_id: vectorFromArray(IDS.map(bytes), new Binary()),
    x: vectorFromArray(IDS.map((_, i) => x + i), new Float64()),
    y: vectorFromArray(IDS.map(() => y), new Float64()),
    access: vectorFromArray(IDS.map(() => [TERM]), new List(new Field('item', new Utf8(), true)))
  });
  return tableToIPC(table, 'stream');
}

/** A value for `note` on each row. */
function notes(): Uint8Array {
  const table = new Table({
    external_id: vectorFromArray(IDS.map(bytes), new Binary()),
    note: vectorFromArray(IDS.map((id) => `note of ${id}`), new Utf8())
  });
  return tableToIPC(table, 'stream');
}

/** What the session holding `TERM` is served: the visible count and the ids of the points. */
async function seen(): Promise<{visible: bigint; ids: bigint[]}> {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const request: ViewportRequest = {view: 's0', zoom: 2, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 100};
  const {result} = await client.viewport(token, request);
  return {visible: result.tiles.reduce((sum, t) => sum + t.visible, 0n), ids: [...result.ids]};
}

/** The served id of each row still visible, by external id. */
async function byExternalId(): Promise<Map<string, bigint>> {
  const out = new Map<string, bigint>();
  for (const id of (await seen()).ids) {
    const item = await client.item(token, id);
    out.set(item.externalId!, id);
  }
  return out;
}

async function flushed(): Promise<void> {
  const answer = await control.flush({wait: true});
  expect(answer).toMatchObject({status: 202, body: {visible: true}});
}

describe('Control against a live server', () => {
  it('declares a column, inserts rows, and after a flush a viewer holding their label sees exactly them', async (ctx) => {
    live(ctx);
    const declared = await control.declareAttribute({name: 'note', type: 'keyword', index: true});
    expect(declared.status).toBe(201);
    expect(await seen()).toEqual({visible: 0n, ids: []});

    const inserted = await control.ingest(points(), {view: 's0'});
    expect(inserted).toMatchObject({status: 200, body: {accepted: 4}});
    await flushed();
    token = (await client.authorise([TERM])).token;
    const after = await seen();
    expect(after.visible).toBe(4n);
    expect(after.ids).toHaveLength(4);
    expect([...(await byExternalId()).keys()].sort()).toEqual(IDS.map(addressed).sort());
  });

  it('replays a page resent under its batch id, and lands it again under a fresh one', async (ctx) => {
    live(ctx);
    // No external id, so the second landing is not refused as a position already held.
    const body = new Table({
      x: vectorFromArray([meta.views.find((v) => v.id === 's0')!.quantisation.xMin], new Float64()),
      y: vectorFromArray([meta.views.find((v) => v.id === 's0')!.quantisation.yMin], new Float64()),
      access: vectorFromArray([['someone-else']], new List(new Field('item', new Utf8(), true)))
    });
    const page = tableToIPC(body, 'stream');
    const first = await control.ingest(page, {view: 's0'});
    expect(first.body).toMatchObject({accepted: 1});
    const again = await control.ingest(page, {view: 's0', batch: first.batch});
    expect(again.body).toMatchObject({accepted: 0, replayed: true});
    // Another batch id is another request, carrying the same bytes.
    const other = await control.ingest(page, {view: 's0'});
    expect(other.batch).not.toBe(first.batch);
    expect(other.body).toMatchObject({accepted: 1});
  });

  it('fills a declared column on rows it holds, which the item card then carries', async (ctx) => {
    live(ctx);
    const filled = await control.values(notes(), {view: 's0'});
    expect(filled).toMatchObject({status: 200, body: {rows: 4, filled: 4}});
    await flushed();
    const rows = await byExternalId();
    expect(rows.size).toBe(4);
    for (const [external, id] of rows) {
      const expected = `note of ${new TextDecoder().decode(Uint8Array.from(atob(external), (c) => c.charCodeAt(0)))}`;
      expect((await client.item(token, id)).fields.note).toBe(expected);
    }
  });

  it('returns a refusal as an answer', async (ctx) => {
    live(ctx);
    const refused = await control.declareAttribute({name: 'note', type: 'text'});
    expect(refused).toMatchObject({status: 409, ok: false, attempts: 1});
  });

  it('publishes an artifact over two rows and grows it by a third, each count read through the viewer', async (ctx) => {
    live(ctx);
    const declared = await control.declareLayer({
      name: LAYER,
      title: 'picked rows',
      views: ['s0'],
      membership: 'enumerated',
      visibility: null,
      artifact_visibility: {field: null, default: 'inherited'},
      require_member_visibility: {count: 1},
      hierarchy: {kind: 'flat', prune_children: false},
      content: {computed: ['centroid'], supplied: []},
      depends_on: [],
      levels: []
    });
    expect(declared.status).toBe(201);
    const published = await control.publish(LAYER, {
      level: 0,
      addressing: 'external',
      artifacts: [{key: 'pair', members: [addressed('row-2'), addressed('row-3')]}]
    });
    expect(published.status).toBe(201);
    const artifact = BigInt((published.body.artifacts as {key: string; tessera_id: string}[])[0]!.tessera_id);
    await flushed();
    expect(await client.artifact(token, artifact, {view: 's0'})).toMatchObject({layer: LAYER, key: 'pair', maskedCount: 2n});

    const grown = await control.grow(LAYER, {level: 0, addressing: 'external', artifacts: [{key: 'pair', members: [addressed('row-1')]}]});
    expect(grown.status).toBe(200);
    await flushed();
    expect((await client.artifact(token, artifact, {view: 's0'})).maskedCount).toBe(3n);
  });

  it('deletes a row, which the viewer no longer counts or opens', async (ctx) => {
    live(ctx);
    const gone = (await byExternalId()).get(addressed('row-0'))!;
    const deleted = await control.changes([{external_id: addressed('row-0'), op: 'delete'}]);
    expect(deleted.status).toBe(200);
    const after = await seen();
    expect(after.visible).toBe(3n);
    expect(after.ids).not.toContain(gone);
    await expect(client.item(token, gone)).rejects.toMatchObject({status: 404});
  });

  it('suppresses a row from the moment it is accepted, and serves it again once lifted', async (ctx) => {
    live(ctx);
    const hidden = (await byExternalId()).get(addressed('row-1'))!;
    expect((await control.changes([{external_id: addressed('row-1'), op: 'suppress'}])).status).toBe(200);
    expect((await seen()).visible).toBe(2n);
    await expect(client.item(token, hidden)).rejects.toMatchObject({status: 404});

    expect((await control.changes([{external_id: addressed('row-1'), op: 'unsuppress'}])).status).toBe(200);
    const after = await seen();
    expect(after.visible).toBe(3n);
    expect(after.ids).toContain(hidden);
  });

  it('drops the layer, which the viewer then no longer lists', async (ctx) => {
    live(ctx);
    expect((await client.meta(token)).layers.map((l) => l.name)).toContain(LAYER);
    expect((await control.dropLayer(LAYER, {wait: true})).status).toBe(200);
    expect((await client.meta(token)).layers.map((l) => l.name)).not.toContain(LAYER);
  });

  it('reports status with the limits every route publishes', async (ctx) => {
    live(ctx);
    const status = await control.status();
    expect(status.status).toBe(200);
    expect(await control.limits()).toEqual(status.body.limits);
  });

  it('revokes a session, after which its token is refused', async (ctx) => {
    live(ctx);
    const session = await client.authorise([TERM]);
    await client.meta(session.token);
    await client.revoke(session.tokenId);
    await expect(client.meta(session.token)).rejects.toMatchObject({status: 401});
  });
});
