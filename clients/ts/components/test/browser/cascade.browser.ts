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
    expect(await computed(light, ['tessera-map'], 'background-color')).toBe('rgb(246, 246, 244)');
    expect(await computed(light, PAN, 'background-color')).toBe('rgb(27, 29, 33)');
    const dark = await page('<tessera-map></tessera-map>', ':root { color-scheme: dark; }');
    expect(await computed(dark, ['tessera-map'], 'background-color')).toBe('rgb(17, 19, 23)');
    expect(await computed(dark, PAN, 'background-color')).toBe('rgb(236, 238, 241)');
  });

  it('take a value set on any ancestor', async () => {
    const p = await page('<div><tessera-map></tessera-map></div>', 'body { --tessera-map-bg: rgb(255, 0, 0); --tessera-accent: rgb(0, 128, 0); }');
    expect(await computed(p, ['tessera-map'], 'background-color')).toBe('rgb(255, 0, 0)');
    expect(await computed(p, PAN, 'background-color')).toBe('rgb(0, 128, 0)');
  });

  it('take a value set on the element itself over one on an ancestor', async () => {
    const p = await page('<tessera-map style="--tessera-map-bg: rgb(0, 0, 255)"></tessera-map>', 'body { --tessera-map-bg: rgb(255, 0, 0); }');
    expect(await computed(p, ['tessera-map'], 'background-color')).toBe('rgb(0, 0, 255)');
  });

  it('reach the elements inside an explorer from the explorer and from above it', async () => {
    const p = await page('<tessera-explorer style="--tessera-accent: rgb(1, 2, 3)"></tessera-explorer>', 'body { --tessera-map-bg: rgb(9, 8, 7); }');
    expect(await computed(p, INNER_PAN, 'background-color')).toBe('rgb(1, 2, 3)');
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

describe('the explorer by the width of its container', () => {
  /** What the explorer drew: which arrangement, where the tools sit, and whether the strip is short. */
  const layout = (p: Page) =>
    p.evaluate(() => {
      const root = document.querySelector('tessera-explorer')!.shadowRoot!;
      return {
        sidebar: root.querySelector('[part="sidebar"]') !== null,
        panel: root.querySelector('[part="panel"]') !== null,
        corner: root.querySelector('tessera-map')!.getAttribute('controls-corner'),
        compact: root.querySelector('tessera-map tessera-status')!.hasAttribute('compact')
      };
    });
  const settled = (p: Page) =>
    p.waitForFunction(() => {
      const root = document.querySelector('tessera-explorer')?.shadowRoot;
      return root?.querySelector('tessera-map tessera-status') != null;
    });

  it('keeps the docked sidebar where there is room, and folds it into the card in a compact container', async () => {
    const wide = await page('<div style="width: 1300px; height: 700px"><tessera-explorer layout="docked"></tessera-explorer></div>');
    await settled(wide);
    expect(await layout(wide)).toEqual({sidebar: true, panel: false, corner: 'top-left', compact: false});
    const compact = await page('<div style="width: 900px; height: 560px"><tessera-explorer layout="docked"></tessera-explorer></div>');
    await compact.waitForFunction(() => document.querySelector('tessera-explorer')?.shadowRoot?.querySelector('[part="panel"]') != null);
    expect(await layout(compact)).toEqual({sidebar: false, panel: true, corner: 'bottom-left', compact: true});
  });
});

describe('the explorer’s sections', () => {
  it('show one heading when open: the summary, not the panel’s own title as well', async () => {
    const p = await page('<div style="width: 1300px; height: 700px"><tessera-explorer layout="docked"></tessera-explorer></div>');
    const seen = await p.evaluate(async () => {
      const root = document.querySelector('tessera-explorer')!.shadowRoot!;
      const section = root.querySelector('details')!;
      section.open = true;
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      const inner = [...section.querySelectorAll('tessera-artifact-list, tessera-hierarchy')].map((e) => e.shadowRoot!.querySelector('[part="title"]'));
      return {summary: section.querySelector('summary')!.checkVisibility(), titles: inner.filter((t) => t?.checkVisibility()).length, panels: inner.length};
    });
    expect(seen).toEqual({summary: true, titles: 0, panels: 1});
  });
});

describe('the map’s toolbar in a bottom corner', () => {
  it('sits against the bottom edge of the map, on its side', async () => {
    const p = await page('<tessera-map controls-corner="bottom-left" style="--tessera-map-height: 400px; width: 600px"></tessera-map><tessera-map controls-corner="bottom-right" style="--tessera-map-height: 400px; width: 600px"></tessera-map>');
    const at = await p.evaluate(() =>
      [...document.querySelectorAll('tessera-map')].map((m) => {
        const map = m.getBoundingClientRect();
        const bar = m.shadowRoot!.querySelector('[part="controls"]')!.getBoundingClientRect();
        return {bottom: map.bottom - bar.bottom < 40, left: bar.left - map.left < 40, right: map.right - bar.right < 40};
      })
    );
    expect(at).toEqual([
      {bottom: true, left: true, right: false},
      {bottom: true, left: false, right: true}
    ]);
  });
});
