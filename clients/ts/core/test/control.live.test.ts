import {Field, Float64, List, Null, RecordBatch, Schema, Struct, Table, tableToIPC, Utf8, vectorFromArray} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, vi, type TestContext} from 'vitest';
import {clusterLayerDeclaration, labelLayerDeclaration} from '../../scripts/operator.js';
import {MosaicaClient} from '../src/client.js';
import {Control} from '../src/control.js';
import type {Meta, ViewportRequest} from '../src/types.js';
import {start, type Served} from './served.js';

/**
 * `Control` against a real `mosaica serve` over the notebook corpus (`served.ts` builds and starts
 * it), each write read back through the viewer plane. The rows written here carry a label no
 * notebook item carries, and a session holding only that label sees exactly them. A session resolves
 * its terms when it is authorised, so the one that reads the rows is authorised after they land.
 *
 * The rows are named by `row_key`, a unique column the first test declares. The tests run in
 * order and share the rows: each one starts from what the one before it left.
 */

const TERM = 'ts-live-control';
const IDS = ['row-0', 'row-1', 'row-2', 'row-3'];
const LAYER = 'ts-live/picks';

let served: Served | string = 'the server has not started';
let client: MosaicaClient;
let control: Control;
let meta: Meta;
let token: string;
/** The artifact the publish test creates, over rows 0 to 3 once grown. */
let pair: bigint;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  client = new MosaicaClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.operatorCredential});
  control = new Control({controlUrl: served.controlUrl, credential: served.operatorCredential});
  token = (await client.authorise({terms: [TERM]})).token;
  meta = await client.meta(token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

/** The columns declared when the tests insert: the notebook's six and the `note` the first test declares. */
const DECLARED = ['archive', 'primary_category', 'submitted_at', 'title', 'abstract', 'arxiv_id', 'note'];

/** The unique column the first test declares, which names each of the four rows. */
const KEY = 'row_key';

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
    [KEY]: vectorFromArray(IDS, new Utf8()),
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
    [KEY]: vectorFromArray(IDS, new Utf8()),
    note: vectorFromArray(IDS.map((id) => `note of ${id}`), new Utf8())
  });
  return tableToIPC(table, 'stream');
}

/** What a session, by default the one holding `TERM`, is served: the visible count and the ids of the points. */
async function seen(as: string = token): Promise<{visible: bigint; ids: bigint[]}> {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const request: ViewportRequest = {view: 's0', zoom: 2, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 100};
  const {result} = await client.viewport(as, request);
  return {visible: result.tiles.reduce((sum, t) => sum + t.visible, 0n), ids: [...result.ids]};
}

/** The served id of each row still visible, by its `row_key`. */
async function byKey(): Promise<Map<string, bigint>> {
  const out = new Map<string, bigint>();
  for (const id of (await seen()).ids) {
    const item = await client.item(token, id);
    out.set(item.fields[KEY] as string, id);
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
    expect((await control.declareAttribute({name: KEY, type: 'keyword', unique: true})).status).toBe(201);
    expect(await seen()).toEqual({visible: 0n, ids: []});

    const inserted = await control.ingest(points(), {view: 's0'});
    expect(inserted).toMatchObject({status: 200, body: {rows: 4, created: 4, refused: []}});
    // The same rows again name the items they created, by their keys, and change nothing.
    const again = await control.ingest(points(), {view: 's0'});
    expect(again).toMatchObject({status: 200, body: {rows: 4, created: 0, unchanged: 4}});
    expect(again.body.tessera_ids).toEqual(inserted.body.tessera_ids);
    await flushed();
    token = (await client.authorise({terms: [TERM]})).token;
    const after = await seen();
    expect(after.visible).toBe(4n);
    expect(after.ids).toHaveLength(4);
    expect([...(await byKey()).keys()].sort()).toEqual([...IDS].sort());
  });

  it('replays a page resent under its batch id, and lands it again under a fresh one', async (ctx) => {
    live(ctx);
    // Nothing in the row names an item, so under a fresh batch id it creates a second one.
    const body = new Table({
      x: vectorFromArray([meta.views.find((v) => v.id === 's0')!.quantisation.xMin], new Float64()),
      y: vectorFromArray([meta.views.find((v) => v.id === 's0')!.quantisation.yMin], new Float64()),
      access: vectorFromArray([['someone-else']], new List(new Field('item', new Utf8(), true))),
      ...nulls(1)
    });
    const page = tableToIPC(body, 'stream');
    const first = await control.ingest(page, {view: 's0'});
    expect(first.body).toMatchObject({created: 1});
    const again = await control.ingest(page, {view: 's0', batch: first.batch});
    expect(again.body).toMatchObject({created: 0, replayed: true});
    // Another batch id is another request, carrying the same bytes.
    const other = await control.ingest(page, {view: 's0'});
    expect(other.batch).not.toBe(first.batch);
    expect(other.body).toMatchObject({created: 1});
  });

  it('refuses a row naming two items and stores the rest, or with strict refuses the page', async (ctx) => {
    live(ctx);
    const row0 = (await byKey()).get('row-0')!.toString();
    // Row 0 names row-0 and changes nothing; row 1 names row-1 by its key and row-0 by its tessera_id.
    const page = tableToIPC(
      new Table({
        [KEY]: vectorFromArray(['row-0', 'row-1'], new Utf8()),
        tessera_id: vectorFromArray([null, row0], new Utf8())
      }),
      'stream'
    );
    const strict = await control.ingest(page, {view: 's0', strict: true});
    expect(strict).toMatchObject({status: 409, ok: false});
    const answer = await control.ingest(page, {view: 's0'});
    expect(answer).toMatchObject({
      status: 200,
      body: {rows: 2, unchanged: 1, edited: 0, created: 0, tessera_ids: [row0, null], refused: [{row: 1, reason: 'names_two_items'}]}
    });
  });

  it('sets a declared column on rows it holds, which the item card then carries', async (ctx) => {
    live(ctx);
    const edited = await control.ingest(notes(), {view: 's0'});
    expect(edited).toMatchObject({status: 200, body: {rows: 4, edited: 4}});
    await flushed();
    const rows = await byKey();
    expect(rows.size).toBe(4);
    for (const [key, id] of rows) {
      expect((await client.item(token, id)).fields.note).toBe(`note of ${key}`);
    }
  });

  it('returns a refusal as an answer', async (ctx) => {
    live(ctx);
    const refused = await control.declareAttribute({name: 'note', type: 'text'});
    expect(refused).toMatchObject({status: 409, ok: false, attempts: 1});
  });

  it('publishes an artifact over two rows named by a unique column, leaving out one naming nothing, and grows it by a third as JSON and a fourth as Arrow, each count read through the viewer', async (ctx) => {
    live(ctx);
    // The declaration `publish-clusters.mjs` sends.
    const declaration = clusterLayerDeclaration({name: LAYER, title: 'picked rows', view: 's0', visibility: null, minVisible: 1, computed: ['centroid', 'box', 'hull']});
    expect((await control.declareLayer(declaration)).status).toBe(201);
    const published = await control.publish(LAYER, {level: 0, artifacts: [{key: 'pair', members: {[KEY]: ['row-2', 'row-3', 'row-none']}}]});
    expect(published).toMatchObject({status: 201, body: {refused: [{artifact: 0, list: 'members', row: 2, reason: 'names_no_item'}]}});
    pair = BigInt((published.body.artifacts as {key: string; tessera_id: string}[])[0]!.tessera_id);
    await flushed();
    expect(await client.artifact(token, pair, {view: 's0'})).toMatchObject({layer: LAYER, key: 'pair', maskedCount: 2n});

    const row1 = (await byKey()).get('row-1')!;
    const grown = await control.grow(LAYER, {level: 0, artifacts: [{key: 'pair', members: {tessera_id: [row1.toString()]}}]});
    expect(grown).toMatchObject({status: 200, body: {refused: []}});
    await flushed();
    expect((await client.artifact(token, pair, {view: 's0'})).maskedCount).toBe(3n);

    // The members column is a list of structs whose fields are the member table's columns.
    const member = new Struct([new Field(KEY, new Utf8(), true)]);
    const rows = new Table({
      key: vectorFromArray(['pair'], new Utf8()),
      members: vectorFromArray([[{[KEY]: 'row-0'}]], new List(new Field('item', member, true)))
    });
    const schema = new Schema(rows.schema.fields, new Map([['level', '0']]));
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
      artifacts: [
        {
          key: 'l-pair',
          members: {[KEY]: ['row-2', 'row-3']},
          content: [{values: ['two rows'], generated_from: {[KEY]: ['row-2', 'row-3']}}],
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
    const metaNow = async () => client.meta((await client.authorise({terms: [TERM]})).token);
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
    const gone = (await byKey()).get('row-0')!;
    const deleted = await control.changes([{op: 'delete', match: {[KEY]: 'row-0'}}]);
    expect(deleted).toMatchObject({status: 200, body: {accepted: 1, refused: []}});
    const after = await seen();
    expect(after.visible).toBe(3n);
    expect(after.ids).not.toContain(gone);
    await expect(client.item(token, gone)).rejects.toMatchObject({status: 404});
    expect((await client.artifact(token, pair, {view: 's0'})).maskedCount).toBe(3n);
  });

  it('suppresses a row from the moment it is accepted, lists a change naming nothing, refuses a strict request holding one, and serves the row again once lifted', async (ctx) => {
    live(ctx);
    const hidden = (await byKey()).get('row-1')!;
    const nobody = {[KEY]: 'row-none'};
    const suppressed = await control.changes([
      {op: 'suppress', match: {tessera_id: hidden.toString()}},
      {op: 'suppress', match: nobody}
    ]);
    expect(suppressed).toMatchObject({status: 200, body: {accepted: 1, refused: [{row: 1, reason: 'names_no_item'}]}});
    expect((await seen()).visible).toBe(2n);
    await expect(client.item(token, hidden)).rejects.toMatchObject({status: 404});

    const lift = [{op: 'unsuppress' as const, match: {[KEY]: 'row-1'}}];
    expect(await control.changes([...lift, {op: 'unsuppress', match: nobody}], {strict: true})).toMatchObject({status: 404, ok: false});
    expect((await seen()).visible).toBe(2n);
    expect(await control.changes(lift)).toMatchObject({status: 200, body: {accepted: 1, refused: []}});
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
    expect((await byKey()).has('row-0')).toBe(false);
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
    const session = await client.authorise({terms: [TERM]});
    await client.meta(session.token);
    await client.revoke(session.tokenId);
    await expect(client.meta(session.token)).rejects.toMatchObject({status: 403, code: 'expired-token'});
  });

  it("mints the operator's own session, which reads every item, and refuses it to a key", async (ctx) => {
    live(ctx);
    const own = await client.authorise({readAll: true});
    const narrow = await client.authorise({terms: [TERM]});
    expect((await seen(own.token)).visible).toBeGreaterThanOrEqual((await seen(narrow.token)).visible);
    await client.revoke(own.tokenId);
    await client.revoke(narrow.tokenId);
  });

  it('manages a principal, its credentials and its sessions through the catalogue', async (ctx) => {
    live(ctx);
    const name = 'ts-live-ann';
    const password = 'a password long enough for the minimum';
    expect((await control.createPrincipal(name, 'person')).status).toBe(200);
    expect((await control.createPrincipal(name, 'person')).status).toBe(409);
    expect((await control.grant({principal: name, permission: 'read'})).ok).toBe(true);
    expect((await control.grant({principal: name, terms: [TERM, 'ts-live-other']})).ok).toBe(true);
    expect((await control.revokeGrant({principal: name, term: 'ts-live-other'})).ok).toBe(true);
    expect((await control.setPassword(name, password)).ok).toBe(true);
    const shown = await control.showPrincipal(name);
    expect(shown.body).toMatchObject({terms: [TERM], permissions: ['read'], has_password: true});

    // A password login reads what the principal's terms admit: the rows labelled TERM.
    const byPassword = await client.login({principal: name, password});
    expect((await seen(byPassword.token)).visible).toBe((await seen()).visible);
    await expect(client.login({principal: name, password: 'not the password at all'})).rejects.toMatchObject({status: 401});

    // A key login, then a grant, which ends both sessions.
    const issued = await control.createKey(name);
    const byKey = await client.login({apiKey: String(issued.body.key)});
    const listed = await control.listSessions({principal: name});
    expect(listed.body.sessions).toHaveLength(2);
    expect((await control.grant({principal: name, term: 'ts-live-third'})).body.sessions_ended).toBe(2);
    await expect(client.meta(byKey.token)).rejects.toMatchObject({status: 403});

    // An integrator's key, whose principal holds `authorise-as`, mints a session for the principal
    // and revokes it; it may not name terms.
    expect((await control.createPrincipal('ts-live-portal', 'service')).ok).toBe(true);
    expect((await control.grant({principal: 'ts-live-portal', permission: 'authorise-as'})).ok).toBe(true);
    const portalKey = String((await control.createKey('ts-live-portal')).body.key);
    const {viewerUrl, sessionUrl} = served as Served;
    const portal = new MosaicaClient({viewerUrl, sessionUrl, sessionCredential: portalKey});
    const minted = await portal.authorise({principal: name});
    expect((await seen(minted.token)).visible).toBe((await seen()).visible);
    await portal.revoke(minted.tokenId);
    await expect(client.meta(minted.token)).rejects.toMatchObject({status: 403, code: 'expired-token'});
    await expect(portal.authorise({terms: [TERM]})).rejects.toMatchObject({status: 403});
    await expect(portal.authorise({readAll: true})).rejects.toMatchObject({status: 403});
    expect((await control.deletePrincipal('ts-live-portal')).ok).toBe(true);

    // Logout ends the session it is sent with, and ending by principal ends the rest.
    const again = await client.login({apiKey: String(issued.body.key)});
    await client.logout(again.token);
    await expect(client.meta(again.token)).rejects.toMatchObject({status: 403});
    await client.login({principal: name, password});
    expect((await control.endSessions({principal: name})).body.sessions_ended).toBe(1);

    // A group's terms reach its members; a revoked key and a disabled principal log in no more.
    expect((await control.createGroup('ts-live-group')).ok).toBe(true);
    expect((await control.addMember('ts-live-group', name)).ok).toBe(true);
    expect((await control.showGroup('ts-live-group')).body.members).toEqual([name]);
    expect((await control.listKeys(name)).body).toEqual({keys: [expect.objectContaining({prefix: issued.body.prefix})]});
    expect((await control.revokeKey(String(issued.body.prefix))).ok).toBe(true);
    await expect(client.login({apiKey: String(issued.body.key)})).rejects.toMatchObject({status: 401});
    expect((await control.changePrincipal(name, {disabled: true})).ok).toBe(true);
    await expect(client.login({principal: name, password})).rejects.toMatchObject({status: 401});
    expect((await control.deleteGroup('ts-live-group')).ok).toBe(true);
    expect((await control.deletePrincipal(name)).ok).toBe(true);
    expect((await control.showPrincipal(name)).status).toBe(404);
  });
});
