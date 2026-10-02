#!/usr/bin/env node
// The app server beside the plain-HTML page, and the only holder of an API key that may authorise
// as other principals. The page asks it for a viewer token for the signed-in user, and it calls
// `POST /session/authorise` on the user's behalf. Node's `http`, no framework.
//
//   node server.mjs            # http://localhost:5180, against the demo's servers
//
// It also serves the page and the self-contained bundle, and proxies `/v1/*` to the viewer server
// on the same origin with every response header, including those the client's replica is keyed by.
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {Readable} from 'node:stream';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Mint a viewer token for `principal`, with an API key whose principal holds `authorise-as`. The
 * session carries the terms the deployment's catalogue grants `principal`, so this function decides
 * which principal its user reads as, and the catalogue decides what that principal may see.
 *
 * @param {{sessionUrl: string, apiKey: string}} cfg
 * @param {string} principal  the Tessera principal the signed-in user reads as
 * @returns {Promise<{token: string, expiresAt: number}>}
 */
export async function authorise({sessionUrl, apiKey}, principal) {
  const response = await fetch(`${sessionUrl}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${apiKey}`, 'content-type': 'application/json'},
    body: JSON.stringify({principal})
  });
  if (!response.ok) throw new Error(`authorise: ${response.status} ${await response.text()}`);
  const {token, expires_at} = await response.json();
  return {token, expiresAt: expires_at};
}

/**
 * Forward one request to the viewer server and stream the answer back with every response header,
 * since the client keys its replica on some of them.
 *
 * @param {string} viewerUrl
 * @param {import('node:http').IncomingMessage} req
 * @param {import('node:http').ServerResponse} res
 */
export async function proxy(viewerUrl, req, res) {
  const headers = new Headers();
  for (const [name, value] of Object.entries(req.headers)) {
    if (typeof value === 'string' && name !== 'host' && name !== 'connection') headers.set(name, value);
  }
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : /** @type {ReadableStream} */ (Readable.toWeb(req));
  const init = /** @type {RequestInit} */ ({method: req.method, headers, body, duplex: 'half'});
  const upstream = await fetch(`${viewerUrl}${req.url}`, init);
  /** @type {Record<string, string>} */
  const forwarded = {};
  upstream.headers.forEach((value, name) => {
    forwarded[name] = value;
  });
  res.writeHead(upstream.status, forwarded);
  if (!upstream.body) return res.end();
  const reader = upstream.body.getReader();
  for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) res.write(chunk.value);
  res.end();
}

/**
 * The request handler, built once from its configuration so a test can drive it without a port.
 *
 * @param {{sessionUrl: string, viewerUrl: string, apiKey: string, users: Record<string, {label: string, principal: string}>, bundleDir: string}} cfg
 */
export function createHandler(cfg) {
  /** The bundle's file, read on each request, or `null` where it has not been built. @param {string} name */
  const built = async (name) => {
    try {
      return await readFile(join(cfg.bundleDir, name));
    } catch (e) {
      if (/** @type {NodeJS.ErrnoException} */ (e).code === 'ENOENT') return null;
      throw e;
    }
  };
  /** @param {import('node:http').ServerResponse} res */
  const unbuilt = (res) =>
    json(res, 503, {error: `${join(cfg.bundleDir, 'tessera-components.js')} is not built; run: npm run bundle -w @tesseradb/components`});
  /** @param {import('node:http').IncomingMessage} req @param {import('node:http').ServerResponse} res */
  return async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://localhost');
    try {
      if (url.pathname.startsWith('/v1/')) return await proxy(cfg.viewerUrl, req, res);
      if (url.pathname === '/users') {
        return json(res, 200, Object.entries(cfg.users).map(([name, u]) => ({name, label: u.label})));
      }
      if (url.pathname === '/token' && req.method === 'POST') {
        // The user is a query parameter naming an entry in `users.json`; a real app would read
        // its own session.
        const user = cfg.users[url.searchParams.get('user') ?? ''];
        if (!user) return json(res, 404, {error: 'unknown user'});
        return json(res, 200, await authorise(cfg, user.principal));
      }
      if (url.pathname === '/tessera-components.js') {
        const bundle = await built('tessera-components.js');
        if (!bundle) return unbuilt(res);
        res.writeHead(200, {'content-type': 'text/javascript', 'cache-control': 'no-store'});
        return res.end(bundle);
      }
      if (url.pathname === '/') {
        // The integrity hash of the bundle beside this server. A static page pastes it in, and the
        // browser refuses the bundle if the file changes.
        const sri = await built('tessera-components.js.sri');
        if (!sri) return unbuilt(res);
        const page = await readFile(join(here, 'index.html'), 'utf8');
        res.writeHead(200, {'content-type': 'text/html; charset=utf-8'});
        return res.end(page.replace('__SRI__', sri.toString('utf8').trim()));
      }
      json(res, 404, {error: 'not found'});
    } catch (e) {
      json(res, 502, {error: String(e)});
    }
  };
}

/** @param {import('node:http').ServerResponse} res @param {number} status @param {unknown} body */
function json(res, status, body) {
  res.writeHead(status, {'content-type': 'application/json'});
  res.end(JSON.stringify(body));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const users = JSON.parse(await readFile(join(here, 'users.json'), 'utf8'));
  const apiKey = process.env.TESSERA_API_KEY;
  if (!apiKey) throw new Error('set TESSERA_API_KEY to an API key whose principal holds authorise-as');
  const handler = createHandler({
    sessionUrl: process.env.TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
    viewerUrl: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
    apiKey,
    users,
    bundleDir: join(here, '..', '..', 'components', 'dist')
  });
  const port = Number(process.env.PORT ?? 5180);
  createServer((req, res) => void handler(req, res)).listen(port, () => console.log(`plain-html example on http://localhost:${port}`));
}
