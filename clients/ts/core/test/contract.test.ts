import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';

/**
 * `TesseraClient` reaches the operations the HTTP contract publishes.
 *
 * The contract file is read here rather than a list kept beside it. An operation is reached by the
 * client method of the same name, or the one `REACHED_AS` gives, and each such method is called
 * against a recording `fetch` to check that it sends the operation's method and path.
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

/** Each plane's route prefix, from the `servers` descriptions: `viewer` → `/v1/`, `session` → `/session/`. */
function planes(text: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of text.matchAll(/description: The (\w+) plane listener .*Routes under `(\/[^`]*)`/g)) out.set(m[1]!, m[2]!);
  return out;
}

/** The operation a request is, preferring the most literal path: `/v1/artifacts/browse` over `/v1/artifacts/{tessera_id}`. */
function operationOf(ops: Operation[], method: string, path: string): string | null {
  const matching = ops.filter((op) => op.method === method && op.pattern.test(path));
  matching.sort((a, b) => b.literal - a.literal);
  return matching[0]?.id ?? null;
}

/** Operations the client reaches under another name. */
const REACHED_AS: Record<string, string> = {
  suggestCategoryValues: 'suggest',
  browseArtifacts: 'browse'
};

/** Operations the client does not reach. Adding a method for one of them fails this test until it is removed here. */
const NOT_REACHED = new Set(['healthz', 'readyz', 'items', 'artifacts']);

/**
 * One call of each method that reaches an operation, with arguments enough to send its request, and
 * the signal where one is given.
 */
const CALLS: Record<string, (c: TesseraClient, signal?: AbortSignal) => Promise<unknown>> = {
  authorise: (c, signal) => c.authorise(['t'], signal),
  revoke: (c, signal) => c.revoke(7, signal),
  meta: (c, signal) => c.meta('tok', signal),
  categories: (c, signal) => c.categories('tok', 'archive', {signal}),
  suggest: (c, signal) => c.suggest('tok', 'archive', 'cs', {signal}),
  viewport: (c, signal) => c.viewport('tok', {view: 's0', zoom: 0}, signal),
  item: (c, signal) => c.item('tok', 7n, signal),
  artifact: (c, signal) => c.artifact('tok', 7n, {view: 's0', signal}),
  browse: (c, signal) => c.browse('tok', {view: 's0', layer: 'l'}, signal)
};

const methodOf = (operation: string) => REACHED_AS[operation] ?? operation;

afterEach(() => vi.unstubAllGlobals());

describe('the client against the HTTP contract', () => {
  const ops = operations(readFileSync(CONTRACT, 'utf8'));

  it('reads operations from the contract', () => {
    expect(ops.map((op) => op.id)).toContain('viewport');
    expect(new Set(ops.map((op) => op.id)).size).toBe(ops.length);
  });

  it('has a method for every operation but the ones listed as not reached', () => {
    const missing = ops.map((op) => op.id).filter((id) => typeof (TesseraClient.prototype as unknown as Record<string, unknown>)[methodOf(id)] !== 'function');
    expect(new Set(missing)).toEqual(NOT_REACHED);
  });

  it('names the plane each route prefix is served on', () => {
    expect(planes(readFileSync(CONTRACT, 'utf8'))).toEqual(new Map([['viewer', '/v1/'], ['session', '/session/']]));
  });

  it('sends each operation’s method and path, to its plane, from the method that reaches it', async () => {
    const prefixes = planes(readFileSync(CONTRACT, 'utf8'));
    const origins: Record<string, string> = {viewer: 'http://viewer', session: 'http://session'};
    const planeOf = (path: string) => [...prefixes].find(([, prefix]) => path.startsWith(prefix))?.[0];
    const sent: {method: string; path: string; origin: string}[] = [];
    vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
      const at = new URL(url);
      sent.push({method: init?.method ?? 'GET', path: at.pathname, origin: at.origin});
      return new Response(JSON.stringify({error: 'contract', detail: 'recorded'}), {status: 422});
    });
    const client = new TesseraClient({viewerUrl: origins.viewer!, sessionUrl: origins.session!, sessionCredential: 'cred'});
    for (const op of ops.filter((o) => !NOT_REACHED.has(o.id))) {
      const call = CALLS[methodOf(op.id)];
      expect(call, `a call for ${methodOf(op.id)}`).toBeDefined();
      sent.length = 0;
      await call!(client).catch(() => {});
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
      sent.push({method: init?.method ?? 'GET', headers: {...(init?.headers as Record<string, string>)}, signal: init?.signal});
      return new Response(JSON.stringify({error: 'contract', detail: 'recorded'}), {status: 422});
    };
    const client = new TesseraClient({
      viewerUrl: 'http://viewer',
      sessionUrl: 'http://session',
      sessionCredential: 'cred',
      fetch: hosted as typeof fetch,
      headers: {'x-host': 'embed', authorization: 'Bearer host'}
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
      // The verb's own credential is sent, not the host's header of the same name.
      expect(sent[0]!.headers.authorization, method).not.toBe('Bearer host');
      expect(sent[0]!.signal, method).toBe(signal);
    }
  });
});
