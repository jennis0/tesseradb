import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium} from 'playwright';
import {createServer} from 'vite';

/** The gallery package's directory. */
export const root = join(dirname(fileURLToPath(import.meta.url)), '..');

/** Every element's name, as the page's `el` parameter takes it. */
export const ELEMENTS = [
  'store',
  'count',
  'status',
  'view-picker',
  'key-picker',
  'layer-picker',
  'legend',
  'filter',
  'filter-panel',
  'selection',
  'item-card',
  'artifact-card',
  'artifact-list',
  'hierarchy',
  'map',
  'explorer'
];

/**
 * Serve the gallery with Vite on a free port.
 *
 * @returns {Promise<{url: string, close: () => Promise<void>}>}
 */
export async function serve() {
  const server = await createServer({configFile: join(root, 'vite.config.ts'), root, logLevel: 'error', server: {port: 0, strictPort: false}});
  await server.listen();
  const url = server.resolvedUrls?.local[0];
  if (!url) {
    await server.close();
    throw new Error('gallery: Vite did not report a local URL');
  }
  return {url, close: () => server.close()};
}

/** Chromium with a software GL, as the components' browser suite launches it, so the maps draw headless. */
export function launch() {
  return chromium.launch({args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader']});
}

/**
 * Open the gallery at `query` and wait for every specimen to settle.
 *
 * @param {import('playwright').Page} page
 * @param {string} base
 * @param {Record<string, string>} query
 */
export async function open(page, base, query) {
  await page.goto(`${base}?${new URLSearchParams(query)}`);
  await page.waitForFunction(() => document.documentElement.dataset.ready === 'true', undefined, {timeout: 60_000});
}
