import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {Control} from '../src/control.js';
import {headersOf} from './support.js';

/**
 * `TesseraClient` and `Control` reach the operations the HTTP contract publishes.
 *
 * The contract file is read here rather than a list kept beside it. An operation on the viewer or
 * session plane is reached by the `TesseraClient` method of the same name, and one on the control
 * plane by the `Control` method of the same name, or in either case the one `REACHED_AS` gives.
 * Each such method is called against a recording `fetch` to check that it sends the operation's
 * method and path to its plane's listener.
 */

const CONTRACT = join(import.meta.dirname, '../../../../docs/openapi/tessera.yaml');

type Operation = {id: string; method: string; path: string; pattern: RegExp; literal: number};

/** Every operation under `paths:`, read line by line. */
function operations(text: string): Operation[] {
  const out: Operation[] = [];
  let path: string | null = null;
  let method: string | null = null;
  let inPaths = false;
  for (const line of text.split('\n')) {
    if (/^\S/.test(line)) inPaths = line.startsWith('paths:');
    if (!inPaths) continue;
    const p = /^ {2}(\/\S*):\s*$/.exec(line);
    if (p) {
      path = p[1]!;
      method = null;
      continue;
    }
    const m = /^ {4}(get|post|put|patch|delete):\s*$/.exec(line);
    if (m) {
      method = m[1]!.toUpperCase();
      continue;
    }
    const o = /^ {6}operationId:\s*(\S+)\s*$/.exec(line);
    if (o && path && method) {
      const pattern = new RegExp(`^${path.replace(/\{[^}]+\}/g, '[^/]+')}$`);
      out.push({id: o[1]!, method, path, pattern, literal: path.replace(/\{[^}]+\}/g, '').length});
    }
  }
  return out;
}

/** Each plane's route prefix, from the `x-tessera-plane` and `x-tessera-route-prefix` of each server. */
function planes(text: string): Map<string, string> {
  const out = new Map<string, string>();
  let inServers = false;
  let plane: string | null = null;
  for (const line of text.split('\n')) {
    if (/^\S/.test(line)) inServers = line.startsWith('servers:');
    if (!inServers) continue;
    const named = /^ {4}x-tessera-plane:\s*(\S+)\s*$/.exec(line);
    if (named) plane = named[1]!;
    const prefix = /^ {4}x-tessera-route-prefix:\s*(\S+)\s*$/.exec(line);
    if (prefix && plane) out.set(plane, prefix[1]!);
  }
  return out;
}

/** The operation a request is, preferring the most literal path: `/v1/artifacts/browse` over `/v1/artifacts/{tessera_id}`. */
function operationOf(ops: Operation[], method: string, path: string): string | null {
  const matching = ops.filter((op) => op.method === method && op.pattern.test(path));
  matching.sort((a, b) => b.literal - a.literal);
  return matching[0]?.id ?? null;
}

/** Operations reached under another name. */
const REACHED_AS: Record<string, string> = {
  suggestCategoryValues: 'suggest',
  suggestCategoryValuesFiltered: 'suggest',
  browseArtifacts: 'browse',
  addVocabularyValues: 'vocabularyValues',
  declarePlainView: 'declareView',
  registerLayer: 'declareLayer',
  publishArtifacts: 'publish',
  growMemberships: 'grow'
};

/**
 * Operations neither client reaches, each with the reason. Adding a method for one of them fails
 * this test until it is removed here.
 */
const NOT_REACHED = new Map([
  ['healthz', 'a probe for a supervisor, unauthenticated, on the viewer and session listeners'],
  ['readyz', 'a probe for a supervisor, unauthenticated, on the viewer and session listeners']
]);

/**
 * One call of each `TesseraClient` method that reaches an operation, with arguments enough to send
 * its request, and the signal where one is given. A method reaching two operations has a call per
 * operation, keyed by the operation's id.
 */
const CALLS: Record<string, (c: TesseraClient, signal?: AbortSignal) => Promise<unknown>> = {
  login: (c, signal) => c.login({apiKey: 'key'}, signal),
  logout: (c, signal) => c.logout('tok', signal),
  authorise: (c, signal) => c.authorise({principal: 'ann'}, signal),
  revoke: (c, signal) => c.revoke(7, signal),
  meta: (c, signal) => c.meta('tok', signal),
  categories: (c, signal) => c.categories('tok', 'archive', {signal}),
  suggest: (c, signal) => c.suggest('tok', 'archive', 'cs', {signal}),
  suggestCategoryValuesFiltered: (c, signal) =>
    c.suggest('tok', 'archive', 'cs', {view: 's0', filters: {archive: {eq: 'cs'}}, signal}),
  viewport: (c, signal) => c.viewport('tok', {view: 's0', zoom: 0}, {signal}),
  viewportArtifacts: (c, signal) => c.viewportArtifacts('tok', {view: 's0', zoom: 0, tiles: [0n], perTile: 1}, {signal}),
  item: (c, signal) => c.item('tok', 7n, signal),
  artifact: (c, signal) => c.artifact('tok', 7n, {view: 's0', signal}),
  browse: (c, signal) => c.browse('tok', {view: 's0', layer: 'l'}, signal),
  items: (c, signal) => c.items('tok', {view: 's0', fields: []}, signal),
  aggregate: (c, signal) => c.aggregate('tok', {view: 's0', groupings: [{}]}, signal),
  artifacts: (c, signal) => c.artifacts('tok', {view: 's0', layer: 'l', fields: []}, signal)
};

/** One call of each `Control` method that reaches an operation, with arguments enough to send its request. */
const CONTROL_CALLS: Record<string, (c: Control) => Promise<unknown>> = {
  ingest: (c) => c.ingest(new Uint8Array()),
  changes: (c) => c.changes([]),
  status: (c) => c.status(),
  flush: (c) => c.flush(),
  compact: (c) => c.compact(),
  declareAttribute: (c) => c.declareAttribute({}),
  declareVocabulary: (c) => c.declareVocabulary('genre', {}),
  vocabularyValues: (c) => c.vocabularyValues('genre', {}),
  declareViewGroup: (c) => c.declareViewGroup('quarter', {}),
  declareView: (c) => c.declareView('s1', {}),
  createView: (c) => c.createView('quarter', 'q1', {}),
  dropView: (c) => c.dropView('quarter', 'q1'),
  declareLayer: (c) => c.declareLayer({}),
  dropLayer: (c) => c.dropLayer('clusters'),
  publish: (c) => c.publish('clusters', {artifacts: []}),
  grow: (c) => c.grow('clusters', {artifacts: []}),
  listPrincipals: (c) => c.listPrincipals(),
  showPrincipal: (c) => c.showPrincipal('ann'),
  createPrincipal: (c) => c.createPrincipal('ann', 'person'),
  changePrincipal: (c) => c.changePrincipal('ann', {disabled: true}),
  deletePrincipal: (c) => c.deletePrincipal('ann'),
  setPassword: (c) => c.setPassword('ann', 'a long enough password'),
  clearPassword: (c) => c.clearPassword('ann'),
  listKeys: (c) => c.listKeys('ann'),
  createKey: (c) => c.createKey('ann'),
  revokeKey: (c) => c.revokeKey('abc'),
  listGroups: (c) => c.listGroups(),
  showGroup: (c) => c.showGroup('readers'),
  createGroup: (c) => c.createGroup('readers'),
  deleteGroup: (c) => c.deleteGroup('readers'),
  addMember: (c) => c.addMember('readers', 'ann'),
  removeMember: (c) => c.removeMember('readers', 'ann'),
  grant: (c) => c.grant({principal: 'ann', terms: ['a', 'b']}),
  revokeGrant: (c) => c.revokeGrant({principal: 'ann', permission: 'read'}),
  listProviders: (c) => c.listProviders(),
  showProvider: (c) => c.showProvider('corp'),
  putProvider: (c) => c.putProvider('corp', {issuer: 'https://i', audience: 'a', jwks_url: 'https://i/keys'}),
  dropProvider: (c) => c.dropProvider('corp'),
  listSessions: (c) => c.listSessions({principal: 'ann'}),
  endSessions: (c) => c.endSessions({token_id: 7})
};

const methodOf = (operation: string) => REACHED_AS[operation] ?? operation;

/** The client class whose methods reach operations under `path`. */
const surfaceOf = (path: string) => (path.startsWith('/control/') ? Control : TesseraClient);

afterEach(() => vi.unstubAllGlobals());

describe('the client against the HTTP contract', () => {
  const ops = operations(readFileSync(CONTRACT, 'utf8'));

  it('reads operations from the contract', () => {
    expect(ops.map((op) => op.id)).toContain('viewport');
    expect(new Set(ops.map((op) => op.id)).size).toBe(ops.length);
  });

  it('has a method for every operation but the ones listed as not reached', () => {
    const missing = ops
      .filter((op) => typeof (surfaceOf(op.path).prototype as unknown as Record<string, unknown>)[methodOf(op.id)] !== 'function')
      .map((op) => op.id);
    expect(new Set(missing)).toEqual(new Set(NOT_REACHED.keys()));
  });

  it('names the plane each route prefix is served on', () => {
    expect(planes(readFileSync(CONTRACT, 'utf8'))).toEqual(
      new Map([
        ['viewer', '/v1/'],
        ['session', '/session/'],
        ['control', '/control/']
      ])
    );
  });

  it('sends each operation’s method and path, to its plane, from the method that reaches it', async () => {
    const prefixes = planes(readFileSync(CONTRACT, 'utf8'));
    const origins: Record<string, string> = {viewer: 'http://viewer', session: 'http://session', control: 'http://control'};
    const planeOf = (path: string) => [...prefixes].find(([, prefix]) => path.startsWith(prefix))?.[0];
    const sent: {method: string; path: string; origin: string}[] = [];
    vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
      const at = new URL(url);
      sent.push({method: init?.method ?? 'GET', path: at.pathname, origin: at.origin});
      return new Response(JSON.stringify({error: 'contract', detail: 'recorded'}), {status: 422});
    });
    const client = new TesseraClient({viewerUrl: origins.viewer!, sessionUrl: origins.session!, sessionCredential: 'cred'});
    const control = new Control({controlUrl: origins.control!, credential: 'operator'});
    for (const op of ops.filter((o) => !NOT_REACHED.has(o.id))) {
      const method = methodOf(op.id);
      const call: (() => Promise<unknown>) | undefined =
        surfaceOf(op.path) === Control
          ? CONTROL_CALLS[method] && (() => CONTROL_CALLS[method]!(control))
          : (CALLS[op.id] ?? CALLS[method]) && (() => (CALLS[op.id] ?? CALLS[method])!(client));
      expect(call, `a call for ${method}`).toBeDefined();
      sent.length = 0;
      await call!().catch(() => {});
      expect(sent.map((s) => operationOf(ops, s.method, s.path)), op.id).toEqual([op.id]);
      expect(sent[0]!.origin, op.id).toBe(origins[planeOf(op.path)!]);
    }
  });

  it('sends every request through the host’s fetch, with its headers and the caller’s signal', async () => {
    vi.stubGlobal('fetch', async () => {
      throw new Error('the global fetch was used');
    });
    const sent: {method: string; headers: Record<string, string>; signal: AbortSignal | null | undefined}[] = [];
    const hosted = async (_url: string | URL | Request, init?: RequestInit) => {
      sent.push({method: init?.method ?? 'GET', headers: headersOf(init), signal: init?.signal});
      return new Response(JSON.stringify({error: 'contract', detail: 'recorded'}), {status: 422});
    };
    const client = new TesseraClient({
      viewerUrl: 'http://viewer',
      sessionUrl: 'http://session',
      sessionCredential: 'cred',
      fetch: hosted as typeof fetch,
      headers: {'X-Host': 'embed', Authorization: 'Bearer host', 'Content-Type': 'text/plain'}
    });
    for (const [method, call] of Object.entries(CALLS)) {
      sent.length = 0;
      const signal = new AbortController().signal;
      const thrown = await call(client, signal).then(
        () => null,
        (error: unknown) => error
      );
      expect(thrown, method).toMatchObject({status: 422});
      expect(sent, method).toHaveLength(1);
      expect(sent[0]!.headers['x-host'], method).toBe('embed');
      // The verb's own credential replaces the host's header of the same name, whatever its case.
      // Login sends its credential in the body, so the host's header is sent unchanged.
      expect(sent[0]!.headers.authorization, method).toMatch(method === 'login' ? /^Bearer host$/ : /^Bearer (tok|cred)$/);
      // Logout sends no body.
      if (sent[0]!.method === 'POST' && method !== 'logout') expect(sent[0]!.headers['content-type'], method).toBe('application/json');
      expect(sent[0]!.signal, method).toBe(signal);
    }
  });
});
