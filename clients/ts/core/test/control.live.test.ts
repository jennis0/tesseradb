import {Binary, Field, Float64, List, Null, RecordBatch, Schema, Table, tableToIPC, Utf8, vectorFromArray} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, vi, type TestContext} from 'vitest';
import {clusterLayerDeclaration, labelLayerDeclaration} from '../../scripts/operator.js';
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
/** The artifact the publish test creates, over rows 0 to 3 once grown. */
let pair: bigint;

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

/**
 * The columns declared when the tests insert: the notebook's six and the `note` the first test
 * declares. A row that creates an item carries every declared column.
 */
const DECLARED = ['archive', 'primary_category', 'submitted_at', 'title', 'abstract', 'arxiv_id', 'note'];

/** Each declared column, null on every one of `rows` rows. */
function nulls(rows: number) {
  return Object.fromEntries(DECLARED.map((name) => [name, vectorFromArray(Array.from({length: rows}, () => null), new Null())]));
}

/** The four rows, near the middle of view `s0`, each labelled `TERM`, with no value in any declared column. */
function points(): Uint8Array {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const x = (q.xMin + q.xMax) / 2;
  const y = (q.yMin + q.yMax) / 2;
  const table = new Table({
    external_id: vectorFromArray(IDS.map(bytes), new Binary()),
    x: vectorFromArray(IDS.map((_, i) => x + i), new Float64()),
    y: vectorFromArray(IDS.map(() => y), new Float64()),
    access: vectorFromArray(IDS.map(() => [TERM]), new List(new Field('item', new Utf8(), true))),
    ...nulls(IDS.length)
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
      access: vectorFromArray([['someone-else']], new List(new Field('item', new Utf8(), true))),
      ...nulls(1)
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

  it('publishes an artifact over two rows and grows it by a third as JSON and a fourth as Arrow, each count read through the viewer', async (ctx) => {
    live(ctx);
    // The declaration `publish-clusters.mjs` sends.
    const declaration = clusterLayerDeclaration({name: LAYER, title: 'picked rows', view: 's0', visibility: null, minVisible: 1, computed: ['centroid', 'box', 'hull']});
    expect((await control.declareLayer(declaration)).status).toBe(201);
    const published = await control.publish(LAYER, {
      level: 0,
      addressing: 'external',
      artifacts: [{key: 'pair', members: [addressed('row-2'), addressed('row-3')]}]
    });
    expect(published.status).toBe(201);
    pair = BigInt((published.body.artifacts as {key: string; tessera_id: string}[])[0]!.tessera_id);
    await flushed();
    expect(await client.artifact(token, pair, {view: 's0'})).toMatchObject({layer: LAYER, key: 'pair', maskedCount: 2n});

    const grown = await control.grow(LAYER, {level: 0, addressing: 'external', artifacts: [{key: 'pair', members: [addressed('row-1')]}]});
    expect(grown.status).toBe(200);
    await flushed();
    expect((await client.artifact(token, pair, {view: 's0'})).maskedCount).toBe(3n);

    const rows = new Table({key: vectorFromArray(['pair'], new Utf8()), members: vectorFromArray([[addressed('row-0')]], new List(new Field('item', new Utf8(), true)))});
    const schema = new Schema(rows.schema.fields, new Map([['addressing', 'external'], ['level', '0']]));
    const arrow = tableToIPC(new Table(schema, rows.batches.map((b) => new RecordBatch(schema, b.data))), 'stream');
    expect((await control.grow(LAYER, arrow, {wait: true})).body).toMatchObject({visible: true});
    expect((await client.artifact(token, pair, {view: 's0'})).maskedCount).toBe(4n);
  });

  it('takes the label layer the operator scripts declare, and serves a label to a viewer who sees what it was written from', async (ctx) => {
    live(ctx);
    const labels = `${LAYER}-labels`;
    expect((await control.declareLayer(labelLayerDeclaration({name: labels, title: 'labels', view: 's0', clusters: LAYER}))).status).toBe(201);
    const published = await control.publish(labels, {
      level: 0,
      addressing: 'external',
      artifacts: [
        {
          key: 'l-pair',
          members: [addressed('row-2'), addressed('row-3')],
          content: [{values: ['two rows'], generated_from: [addressed('row-2'), addressed('row-3')]}],
          attached_to: {layer: LAYER, level: 0, key: 'pair'}
        }
      ]
    }, {wait: true});
    expect(published.status).toBe(201);
    const label = BigInt((published.body.artifacts as {tessera_id: string}[])[0]!.tessera_id);
    expect(await client.artifact(token, label, {view: 's0'})).toMatchObject({layer: labels, key: 'l-pair', maskedCount: 2n});

    // The pair `write-cycle-demo.mjs` declares: no existence floor, the centroid alone.
    const clusters = 'ts-live/write-cycle';
    expect((await control.declareLayer(clusterLayerDeclaration({name: clusters, title: 'c', view: 's0', visibility: null, minVisible: null, computed: ['centroid']}))).status).toBe(201);
    expect((await control.declareLayer(labelLayerDeclaration({name: `${clusters}-labels`, title: 'l', view: 's0', clusters}))).status).toBe(201);
    const names = (await client.meta(token)).layers.map((l) => l.name);
    expect(names).toEqual(expect.arrayContaining([labels, clusters, `${clusters}-labels`]));
  });

  it('declares a vocabulary, a column over it and more values, which the vocabulary route then lists', async (ctx) => {
    live(ctx);
    const vocabulary = await control.declareVocabulary('ts-kind', {value_set: 'closed', visibility: 'public', width: 'u8', values: [{key: 'a', title: 'A'}]});
    expect(vocabulary.status).toBe(201);
    expect((await control.declareAttribute({name: 'kind', type: 'category', vocabulary: 'ts-kind', index: true})).status).toBe(201);
    expect((await client.categories(token, 'kind')).map((v) => v.key)).toEqual(['a']);
    expect((await control.vocabularyValues('ts-kind', {values: [{key: 'b'}]})).status).toBe(200);
    expect((await client.categories(token, 'kind')).map((v) => v.key).sort()).toEqual(['a', 'b']);
  });

  it('declares a plain view and a view group, creates a view of the group and drops it, each read back from /v1/meta', async (ctx) => {
    live(ctx);
    // A session resolves the views it may reach when it is authorised.
    const metaNow = async () => client.meta((await client.authorise([TERM])).token);
    const extent = {x: [0, 100], y: [0, 100]};
    expect(await control.declareView('ts-plain', {extent}, {wait: true})).toMatchObject({status: 201, body: {visible: true}});
    expect((await metaNow()).views.map((v) => v.id)).toContain('ts-plain');

    expect((await control.declareViewGroup('ts-group', {extent, metadata: [{name: 'label', type: 'text'}]}, {wait: true})).status).toBe(201);
    expect((await metaNow()).groups.find((g) => g.name === 'ts-group')).toMatchObject({views: []});

    expect((await control.createView('ts-group', 'one', {metadata: {label: 'One'}}, {wait: true})).status).toBe(201);
    const created = (await metaNow()).views.find((v) => v.id === 'ts-group:one');
    expect(created?.roster).toEqual({group: 'ts-group', key: 'one', metadata: {label: {type: 'text', value: 'One'}}});

    expect((await control.dropView('ts-group', 'one', {wait: true})).status).toBe(200);
    expect((await metaNow()).views.map((v) => v.id)).not.toContain('ts-group:one');
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
    expect((await client.artifact(token, pair, {view: 's0'})).maskedCount).toBe(3n);
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

  it('asks for a fold, which runs, and the deleted row stays gone after it', async (ctx) => {
    live(ctx);
    const folds = async () => ((await control.status()).body.compaction as {folds: number}).folds;
    const before = await folds();
    expect((await control.compact()).status).toBe(202);
    await vi.waitFor(async () => expect(await folds()).toBeGreaterThan(before), {timeout: 30_000, interval: 50});
    expect((await seen()).visible).toBe(3n);
    expect((await byExternalId()).has(addressed('row-0'))).toBe(false);
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
