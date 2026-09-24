import {mkdtempSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium, type Browser, type Page} from 'playwright';
import {build} from 'vite';
import {afterAll, beforeAll, describe, expect, it} from 'vitest';

/**
 * The theme tokens and the forwarded parts, in Chromium's own cascade, against the single-file
 * bundle a page with no build step loads.
 */

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
let out = '';
let browser: Browser;

beforeAll(async () => {
  out = mkdtempSync(join(tmpdir(), 'tessera-components-'));
  await build({configFile: join(root, 'vite.config.ts'), root, logLevel: 'silent', build: {outDir: out, emptyOutDir: true}});
  browser = await chromium.launch({args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader']});
});

afterAll(async () => {
  await browser?.close();
  if (out) rmSync(out, {recursive: true, force: true});
});

/** A page holding `body` and `css`, with every element defined and rendered. */
async function page(body: string, css = ''): Promise<Page> {
  const p = await browser.newPage();
  await p.setContent(`<!doctype html><html><head><style>${css}</style></head><body>${body}</body></html>`);
  await p.addScriptTag({path: join(out, 'tessera-components.js'), type: 'module'});
  await p.evaluate(async () => {
    await customElements.whenDefined('tessera-explorer');
    const all = (r: ParentNode): Element[] => [...r.querySelectorAll('*')].flatMap((e) => [e, ...(e.shadowRoot ? all(e.shadowRoot) : [])]);
    for (let i = 0; i < 3; i++) for (const e of all(document)) await (e as unknown as {updateComplete?: Promise<unknown>}).updateComplete;
  });
  return p;
}

/**
 * One computed property of the element reached by `path`: each entry a selector, each step after
 * the first taken inside the previous match's shadow root.
 */
function computed(p: Page, path: string[], property: string): Promise<string> {
  return p.evaluate(
    ([path, property]) => {
      let at: Element | null = document.querySelector(path[0]!);
      for (const selector of path.slice(1)) at = at?.shadowRoot?.querySelector(selector) ?? null;
      if (!at) throw new Error(`nothing at ${path.join(' >> ')}`);
      return getComputedStyle(at).getPropertyValue(property);
    },
    [path, property] as const
  );
}

const PAN = ['tessera-map', '[part="controls"] button[aria-pressed="true"]'];
const INNER_PAN = ['tessera-explorer', 'tessera-map', '[part="controls"] button[aria-pressed="true"]'];

describe('theme tokens', () => {
  it('fall back to the light defaults, and to the dark ones under a dark colour scheme', async () => {
    const light = await page('<tessera-map></tessera-map>');
    expect(await computed(light, ['tessera-map'], 'background-color')).toBe('rgb(247, 247, 244)');
    expect(await computed(light, PAN, 'color')).toBe('rgb(36, 87, 163)');
    const dark = await page('<tessera-map></tessera-map>', ':root { color-scheme: dark; }');
    expect(await computed(dark, ['tessera-map'], 'background-color')).toBe('rgb(12, 14, 17)');
    expect(await computed(dark, PAN, 'color')).toBe('rgb(134, 176, 240)');
  });

  it('take a value set on any ancestor', async () => {
    const p = await page('<div><tessera-map></tessera-map></div>', 'body { --tessera-map-bg: rgb(255, 0, 0); --tessera-accent: rgb(0, 128, 0); }');
    expect(await computed(p, ['tessera-map'], 'background-color')).toBe('rgb(255, 0, 0)');
    expect(await computed(p, PAN, 'color')).toBe('rgb(0, 128, 0)');
  });

  it('take a value set on the element itself over one on an ancestor', async () => {
    const p = await page('<tessera-map style="--tessera-map-bg: rgb(0, 0, 255)"></tessera-map>', 'body { --tessera-map-bg: rgb(255, 0, 0); }');
    expect(await computed(p, ['tessera-map'], 'background-color')).toBe('rgb(0, 0, 255)');
  });

  it('reach the elements inside an explorer from the explorer and from above it', async () => {
    const p = await page('<tessera-explorer style="--tessera-accent: rgb(1, 2, 3)"></tessera-explorer>', 'body { --tessera-map-bg: rgb(9, 8, 7); }');
    expect(await computed(p, INNER_PAN, 'color')).toBe('rgb(1, 2, 3)');
    expect(await computed(p, ['tessera-explorer', 'tessera-map'], 'background-color')).toBe('rgb(9, 8, 7)');
  });
});

describe('forwarded parts', () => {
  it('style the explorer’s inner map and status strip from the page', async () => {
    const p = await page(
      '<tessera-explorer></tessera-explorer>',
      'tessera-explorer::part(map-controls) { border-top-color: rgb(4, 5, 6); } tessera-explorer::part(status-strip) { color: rgb(7, 8, 9); }'
    );
    expect(await computed(p, ['tessera-explorer', 'tessera-map', '[part="controls"]'], 'border-top-color')).toBe('rgb(4, 5, 6)');
    expect(await computed(p, ['tessera-explorer', '[part="strip-row"] tessera-status', '[part="strip"]'], 'color')).toBe('rgb(7, 8, 9)');
  });
});
