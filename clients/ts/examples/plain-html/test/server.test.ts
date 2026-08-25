import {createServer, type Server} from 'node:http';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {createHandler} from '../server.mjs';

/**
 * The app server against a fake session plane and a fake viewer plane: the credential goes
 * upstream and never to the page, the claims are the user's, and the proxy forwards the six
 * headers a replica is keyed by (client-obligations rule 10) along with a streamed body.
 */
const SIX = ['etag', 'x-tessera-identity-key', 'x-tessera-pin', 'x-tessera-stale', 'x-tessera-server-us', 'x-tessera-admission-us'];

let upstream: Server;
let app: Server;
let base = '';
const seen: {path: string; auth: string | undefined; body: string}[] = [];

const listen = (s: Server) => new Promise<number>((r) => s.listen(0, () => r((s.address() as {port: number}).port)));

beforeAll(async () => {
  upstream = createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      seen.push({path: req.url ?? '', auth: req.headers.authorization, body});
      if (req.url === '/session/authorise') {
        res.writeHead(200, {'content-type': 'application/json'});
        return res.end(JSON.stringify({token: 'tok-1', token_id: 1, expires_at: 99}));
      }
      const headers: Record<string, string> = {'content-type': 'application/octet-stream'};
      for (const h of SIX) headers[h] = `v-${h}`;
      res.writeHead(200, headers);
      res.write('part-1;');
      setTimeout(() => res.end('part-2'), 20);
    });
  });
  const port = await listen(upstream);
  const dist = await mkdtemp(join(tmpdir(), 'tessera-plain-'));
  await writeFile(join(dist, 'tessera-components.js'), 'export {};');
  await writeFile(join(dist, 'tessera-components.js.sri'), 'sha384-FAKE\n');
  const handler = createHandler({
    sessionUrl: `http://127.0.0.1:${port}`,
    viewerUrl: `http://127.0.0.1:${port}`,
    credential: 'the-secret',
    users: {reader: {label: 'reader', terms: ['14', '15']}},
    bundleDir: dist
  });
  app = createServer((req, res) => void handler(req, res));
  base = `http://127.0.0.1:${await listen(app)}`;
});
afterAll(() => {
  app.close();
  upstream.close();
});

describe('server.mjs', () => {
  it('mints a token for a known user with the credential upstream and never in the answer', async () => {
    const r = await fetch(`${base}/token?user=reader`, {method: 'POST'});
    expect(r.status).toBe(200);
    const body = await r.json();
    expect(body).toEqual({token: 'tok-1', expiresAt: 99});
    const call = seen.find((s) => s.path === '/session/authorise')!;
    expect(call.auth).toBe('Bearer the-secret');
    expect(JSON.parse(Buffer.from(JSON.parse(call.body).auth_data, 'base64').toString())).toEqual({terms: ['14', '15']});
    expect(JSON.stringify(body)).not.toContain('the-secret');
  });

  it('refuses an unknown user without calling upstream', async () => {
    const before = seen.length;
    const r = await fetch(`${base}/token?user=nobody`, {method: 'POST'});
    expect(r.status).toBe(404);
    expect(seen.length).toBe(before);
  });

  it('proxies /v1/* with the six headers kept and the streamed body whole', async () => {
    const r = await fetch(`${base}/v1/viewport`, {method: 'POST', headers: {authorization: 'Bearer tok-1', 'content-type': 'application/json'}, body: '{"k":0}'});
    expect(r.status).toBe(200);
    for (const h of SIX) expect(r.headers.get(h)).toBe(`v-${h}`);
    expect(await r.text()).toBe('part-1;part-2');
    const call = seen.find((s) => s.path === '/v1/viewport')!;
    expect(call.auth).toBe('Bearer tok-1');
    expect(call.body).toBe('{"k":0}');
  });

  it('serves the page with the bundle’s integrity hash filled in, and the bundle itself', async () => {
    const page = await (await fetch(`${base}/`)).text();
    expect(page).toContain('integrity="sha384-FAKE"');
    expect(page).not.toContain('__SRI__');
    expect(page).toContain("createElement('tessera-explorer')");
    expect(await (await fetch(`${base}/tessera-components.js`)).text()).toBe('export {};');
  });
});
