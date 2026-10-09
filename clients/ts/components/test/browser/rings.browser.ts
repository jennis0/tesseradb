import {existsSync, mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {createServer, type Server} from 'node:http';
import {tmpdir} from 'node:os';
import {dirname, join, normalize} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium, type Browser} from 'playwright';
import {build, defaultClientConditions} from 'vite';
import {afterAll, beforeAll, describe, expect, it} from 'vitest';

/**
 * A point with no value under sizing by a column, drawn by the deck layer in Chromium: a ring whose
 * inside is clear to the eye and still picks the point.
 */

const here = dirname(fileURLToPath(import.meta.url));
let built = '';
let server: Server;
let origin = '';
let browser: Browser;

beforeAll(async () => {
  built = mkdtempSync(join(tmpdir(), 'mosaica-rings-'));
  await build({
    configFile: false,
    root: here,
    logLevel: 'silent',
    resolve: {
      conditions: ['mosaica-source', ...defaultClientConditions],
      alias: [{find: /^\.\/aggregation-loader\.js$/, replacement: join(here, '..', '..', '..', 'deck', 'src', 'aggregation-static.ts')}]
    },
    define: {'process.env.NODE_ENV': JSON.stringify('production')},
    build: {outDir: built, emptyOutDir: true, lib: {entry: join(here, 'rings-page.ts'), formats: ['es'], fileName: () => 'page.js'}, rolldownOptions: {output: {codeSplitting: false}}}
  });
  server = createServer((request, response) => {
    const path = normalize(decodeURIComponent(new URL(request.url ?? '/', 'http://x').pathname));
    if (path === '/') return void response.writeHead(200, {'content-type': 'text/html'}).end('<!doctype html><body style="margin:0"><script type="module" src="/page.js"></script></body>');
    const file = join(built, path);
    if (!existsSync(file)) return void response.writeHead(404).end();
    response.writeHead(200, {'content-type': 'text/javascript'}).end(readFileSync(file));
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

describe('a point with no value under sizing', () => {
  it('draws as a ring whose inside is clear, and picks across its whole disc', async () => {
    const page = await browser.newPage({viewport: {width: 400, height: 400}});
    const failed = new Promise<never>((_, reject) => page.on('pageerror', reject));
    await page.goto(origin);
    await Promise.race([failed, page.waitForFunction(() => (window as unknown as {result?: unknown}).result !== undefined, undefined, {timeout: 30_000})]);
    const r = await page.evaluate(() => (window as unknown as {result: Record<string, number | string>}).result);
    // The valued point is a filled disc; the ring is drawn at its edge, 8 px out, and clear inside.
    expect(r['valued']).toBeGreaterThan(150);
    expect(r['ringEdge']).toBeGreaterThan(100);
    expect(r['ringCentre']).toBe(0);
    expect(r['outside']).toBe(0);
    // Picking finds the point anywhere on the disc the ring bounds, and not past it.
    expect([r['pickCentre'], r['pickEdge'], r['pickOutside']]).toEqual(['22', '22', 'miss']);
  });
});
