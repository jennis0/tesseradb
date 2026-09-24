import {existsSync, mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {createServer, type Server} from 'node:http';
import {tmpdir} from 'node:os';
import {dirname, extname, join, normalize} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium, type Browser} from 'playwright';
import {build} from 'vite';
import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {decodeViewport} from '@tesseradb/client';

/**
 * The packages as `npm run build` wrote them to `dist/`, in Chromium: the decode worker loaded
 * from its file in a page with no bundler, and the elements built by Vite from their `dist/`
 * modules. Run after `npm run build` at `clients/ts`.
 */

const workspace = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const fixture = join(workspace, 'core', 'test', 'fixtures', 'viewport-plain.bin');
let built = '';
let server: Server;
let origin = '';
let browser: Browser;

const TYPES: Record<string, string> = {'.js': 'text/javascript', '.mjs': 'text/javascript', '.html': 'text/html', '.bin': 'application/octet-stream'};

/** An import map for the client and Arrow's own dependencies, which the page loads unbundled. */
const PLAIN = `<!doctype html><html><head><script type="importmap">${JSON.stringify({
  imports: {
    '@tesseradb/client': '/core/dist/index.js',
    'apache-arrow': '/node_modules/apache-arrow/Arrow.dom.mjs',
    flatbuffers: '/node_modules/flatbuffers/mjs/flatbuffers.js',
    tslib: '/node_modules/tslib/tslib.es6.mjs',
    'json-with-bigint': '/node_modules/json-with-bigint/json-with-bigint.js'
  }
})}</script></head><body><script type="module">
import {workerDecoder} from '@tesseradb/client';
const bytes = new Uint8Array(await (await fetch('/core/test/fixtures/viewport-plain.bin')).arrayBuffer());
const decoder = workerDecoder();
const decoded = await decoder.decode(bytes);
window.result = {ids: decoded.ids.length, workerMs: decoder.lastWorkerMs};
</script></body></html>`;

beforeAll(async () => {
  for (const pkg of ['core', 'deck', 'components']) {
    if (!existsSync(join(workspace, pkg, 'dist', 'index.js'))) throw new Error(`${pkg}/dist is not built; run npm run build in clients/ts first.`);
  }
  built = mkdtempSync(join(tmpdir(), 'tessera-dist-'));
  const page = join(dirname(fileURLToPath(import.meta.url)));
  await build({
    configFile: false,
    root: page,
    logLevel: 'silent',
    build: {outDir: built, emptyOutDir: true, lib: {entry: join(page, 'dist-page.ts'), formats: ['es'], fileName: () => 'page.js'}},
    define: {'process.env.NODE_ENV': JSON.stringify('production')}
  });
  server = createServer((request, response) => {
    const path = normalize(decodeURIComponent(new URL(request.url ?? '/', 'http://x').pathname));
    if (path === '/plain.html') return void response.writeHead(200, {'content-type': 'text/html'}).end(PLAIN);
    if (path === '/elements.html') {
      return void response.writeHead(200, {'content-type': 'text/html'}).end('<!doctype html><script type="module" src="/built/page.js"></script>');
    }
    const file = path.startsWith('/built/') ? join(built, path.slice('/built/'.length)) : join(workspace, path);
    if (!existsSync(file)) return void response.writeHead(404).end();
    response.writeHead(200, {'content-type': TYPES[extname(file)] ?? 'application/octet-stream'}).end(readFileSync(file));
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  origin = `http://127.0.0.1:${typeof address === 'object' && address ? address.port : 0}`;
  browser = await chromium.launch({args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader']});
});

afterAll(async () => {
  await browser?.close();
  await new Promise((resolve) => server?.close(resolve));
  if (built) rmSync(built, {recursive: true, force: true});
});

/** What the page at `path` published as `window.result`; an uncaught error in the page fails it. */
async function result(path: string): Promise<Record<string, unknown>> {
  const page = await browser.newPage();
  const failed = new Promise<never>((_, reject) => page.on('pageerror', reject));
  await page.goto(origin + path);
  await Promise.race([failed, page.waitForFunction(() => (window as unknown as {result?: unknown}).result !== undefined)]);
  return page.evaluate(() => (window as unknown as {result: Record<string, unknown>}).result);
}

describe('the built packages', () => {
  it('decode a response in the worker file beside the decoder, with no bundler', async () => {
    const r = await result('/plain.html');
    expect(r['ids']).toBe(decodeViewport(new Uint8Array(readFileSync(fixture))).ids.length);
    expect(r['workerMs']).toEqual(expect.any(Number));
  });

  it('define and render the elements from dist/, their decorated properties read from attributes', async () => {
    expect(await result('/elements.html')).toEqual({instance: true, layout: 'overlay', wash: true, controls: true});
  });
});
