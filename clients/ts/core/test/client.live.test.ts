import {describe, expect, it} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {tileToRequestBbox} from '../src/coords.js';

/**
 * The four verbs against a live `tessera serve`.
 *
 * Skipped unless `TESSERA_LIVE=1`, because it needs a server and a built bundle — the golden-file
 * tests are the ones that run everywhere. Run it with:
 *
 *   TESSERA_LIVE=1 TESSERA_SESSION_CRED=… npx vitest run test/client.live.test.ts
 */
const live = process.env.TESSERA_LIVE === '1';
const cred = process.env.TESSERA_SESSION_CRED ?? '';

const client = new TesseraClient({
  viewerUrl: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
  sessionUrl: process.env.TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
  sessionCredential: cred
});

describe.skipIf(!live)('TesseraClient against a live server', () => {
  it('authorises, reads meta, and serves a viewport whose counts hold', async () => {
    const session = await client.authorise(['0']);
    expect(session.token).toBeTruthy();
    expect(session.expiresAt).toBeGreaterThan(Date.now() / 1000);

    const meta = await client.meta(session.token);
    expect(meta.slices.length).toBeGreaterThan(0);
    expect(meta.selection.maxK).toBeGreaterThan(0);

    const bbox = tileToRequestBbox({x: 0, y: 0, z: 0}, meta.quantisation);
    const response = await client.viewport(session.token, {
      slice: meta.slices[0]!.id,
      zoom: 2,
      bbox,
      k: 50
    });

    const served = response.result.tiles.reduce((a, t) => a + Number(t.served), 0);
    expect(response.result.ids.length).toBe(served);
    expect(response.timings.serverUs).toBeGreaterThan(0);
    expect(response.pin).toBeTruthy();
  });

  it('resolves a served identity back to an item', async () => {
    const session = await client.authorise(['0']);
    const meta = await client.meta(session.token);
    const response = await client.viewport(session.token, {
      slice: meta.slices[0]!.id,
      zoom: 2,
      bbox: tileToRequestBbox({x: 0, y: 0, z: 0}, meta.quantisation),
      k: 10
    });
    const id = response.result.ids[0]!;
    const item = await client.item(session.token, id);
    // The 2m4 fixture declares no scalars, so `scalars` is legitimately empty; it does carry
    // external ids, so that field is populated. What is being tested is the round-trip: a served
    // identity resolves back to an item this principal may see.
    expect(item.scalars).toBeInstanceOf(Array);
    expect(item.externalId).toBeTypeOf('string');
  });

  it('reports a refusal as a typed error rather than an empty result', async () => {
    // Fail-closed reaching the client as a distinguishable value is what lets the viewer render a
    // failure differently from an empty region.
    await expect(client.meta('not-a-real-token')).rejects.toBeInstanceOf(TesseraError);
    await expect(client.meta('not-a-real-token')).rejects.toMatchObject({
      status: 401,
      code: 'bad-credential'
    });
  });

  it('refuses a bbox the contract forbids', async () => {
    const session = await client.authorise(['0']);
    await expect(
      client.viewport(session.token, {slice: 's0', zoom: 2, bbox: [10, 10, 0, 0]})
    ).rejects.toMatchObject({status: 422, code: 'contract'});
  });
});
