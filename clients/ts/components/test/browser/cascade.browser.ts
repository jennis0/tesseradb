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
  out = mkdtempSync(join(tmpdir(), 'mosaica-components-'));
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
  await p.addScriptTag({path: join(out, 'mosaica-components.js'), type: 'module'});
  await p.evaluate(async () => {
    await customElements.whenDefined('mosaica-explorer');
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

const PAN = ['mosaica-map', '[part="controls"] button[aria-pressed="true"]'];
const INNER_PAN = ['mosaica-explorer', 'mosaica-map', '[part="controls"] button[aria-pressed="true"]'];

describe('theme tokens', () => {
  it('fall back to the light defaults, and to the dark ones under a dark colour scheme', async () => {
    const light = await page('<mosaica-map></mosaica-map>');
    expect(await computed(light, ['mosaica-map'], 'background-color')).toBe('rgb(246, 246, 244)');
    expect(await computed(light, PAN, 'background-color')).toBe('rgb(27, 29, 33)');
    const dark = await page('<mosaica-map></mosaica-map>', ':root { color-scheme: dark; }');
    expect(await computed(dark, ['mosaica-map'], 'background-color')).toBe('rgb(17, 19, 23)');
    expect(await computed(dark, PAN, 'background-color')).toBe('rgb(236, 238, 241)');
  });

  it('take a value set on any ancestor', async () => {
    const p = await page('<div><mosaica-map></mosaica-map></div>', 'body { --mosaica-map-bg: rgb(255, 0, 0); --mosaica-accent: rgb(0, 128, 0); }');
    expect(await computed(p, ['mosaica-map'], 'background-color')).toBe('rgb(255, 0, 0)');
    expect(await computed(p, PAN, 'background-color')).toBe('rgb(0, 128, 0)');
  });

  it('take a value set on the element itself over one on an ancestor', async () => {
    const p = await page('<mosaica-map style="--mosaica-map-bg: rgb(0, 0, 255)"></mosaica-map>', 'body { --mosaica-map-bg: rgb(255, 0, 0); }');
    expect(await computed(p, ['mosaica-map'], 'background-color')).toBe('rgb(0, 0, 255)');
  });

  it('reach the elements inside an explorer from the explorer and from above it', async () => {
    const p = await page('<mosaica-explorer style="--mosaica-accent: rgb(1, 2, 3)"></mosaica-explorer>', 'body { --mosaica-map-bg: rgb(9, 8, 7); }');
    expect(await computed(p, INNER_PAN, 'background-color')).toBe('rgb(1, 2, 3)');
    expect(await computed(p, ['mosaica-explorer', 'mosaica-map'], 'background-color')).toBe('rgb(9, 8, 7)');
  });
});

describe('forwarded parts', () => {
  it('style the explorer’s inner map and status strip from the page', async () => {
    const p = await page(
      '<mosaica-explorer></mosaica-explorer>',
      'mosaica-explorer::part(map-controls) { border-top-color: rgb(4, 5, 6); } mosaica-explorer::part(status-strip) { color: rgb(7, 8, 9); }'
    );
    expect(await computed(p, ['mosaica-explorer', 'mosaica-map', '[part="controls"]'], 'border-top-color')).toBe('rgb(4, 5, 6)');
    expect(await computed(p, ['mosaica-explorer', '[part="strip-row"] mosaica-status', '[part="strip"]'], 'color')).toBe('rgb(7, 8, 9)');
  });
});

describe('the explorer by the width of its container', () => {
  /** What the explorer drew: which arrangement, where the tools sit, and whether the strip is short. */
  const layout = (p: Page) =>
    p.evaluate(() => {
      const root = document.querySelector('mosaica-explorer')!.shadowRoot!;
      return {
        sidebar: root.querySelector('[part="sidebar"]') !== null,
        panel: root.querySelector('[part="panel"]') !== null,
        corner: root.querySelector('mosaica-map')!.getAttribute('controls-corner'),
        compact: root.querySelector('mosaica-map mosaica-status')!.hasAttribute('compact')
      };
    });
  const settled = (p: Page) =>
    p.waitForFunction(() => {
      const root = document.querySelector('mosaica-explorer')?.shadowRoot;
      return root?.querySelector('mosaica-map mosaica-status') != null;
    });

  it('keeps the docked sidebar where there is room, and folds it into the card in a compact container', async () => {
    const wide = await page('<div style="width: 1300px; height: 700px"><mosaica-explorer layout="docked"></mosaica-explorer></div>');
    await settled(wide);
    expect(await layout(wide)).toEqual({sidebar: true, panel: false, corner: 'top-left', compact: false});
    const compact = await page('<div style="width: 900px; height: 560px"><mosaica-explorer layout="docked"></mosaica-explorer></div>');
    await compact.waitForFunction(() => document.querySelector('mosaica-explorer')?.shadowRoot?.querySelector('[part="panel"]') != null);
    expect(await layout(compact)).toEqual({sidebar: false, panel: true, corner: 'bottom-left', compact: true});
  });
});

describe('a list that opens over what sits below it', () => {
  it('shows whole past the bottom of a card that scrolls inside itself', async () => {
    const p = await page('<div id="card" style="width: 340px; height: 160px; overflow-y: auto"><mosaica-filter-panel></mosaica-filter-panel></div>');
    const seen = await p.evaluate(async () => {
      const columns = ['archive', 'title', 'submitted_at', 'author', 'venue', 'year'];
      const projections: Record<string, unknown> = {
        meta: {declaredScalars: [], layers: [], views: [], filterOperands: columns.map((column) => ({column, family: 'keyword', operands: ['eq']})), selection: {}},
        status: {status: 'shown', sessionWarm: true, refusal: null, stale: false, expired: false, retrying: false},
        view: {id: 's0'},
        filters: {draft: {filter: {}, highlight: {}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
        legend: {colourBy: null},
        artifacts: {served: [], colours: new Map()},
        aggregates: new Map()
      };
      const store = {get: (name: string) => projections[name], subscribe: () => () => {}, setAggregate: () => {}};
      const panel = document.querySelector('mosaica-filter-panel') as HTMLElement & {store: unknown; updateComplete: Promise<unknown>};
      panel.store = store;
      await panel.updateComplete;
      panel.shadowRoot!.querySelector<HTMLElement>('[part="add"]')!.click();
      await panel.updateComplete;
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      const list = panel.shadowRoot!.querySelector<HTMLElement>('[part="add-list"]')!;
      const last = [...list.querySelectorAll<HTMLElement>('[part~="add-option"]')].at(-1)!.getBoundingClientRect();
      const card = document.getElementById('card')!.getBoundingClientRect();
      const hit = panel.shadowRoot!.elementFromPoint(last.left + 10, last.top + last.height / 2);
      return {open: list.matches(':popover-open'), below: last.bottom > card.bottom, onTop: hit?.closest('[part~="add-option"]') != null};
    });
    expect(seen).toEqual({open: true, below: true, onTop: true});
  });
});

describe('the map’s toolbar in a bottom corner', () => {
  it('sits against the bottom edge of the map, on its side', async () => {
    const p = await page('<mosaica-map controls-corner="bottom-left" style="--mosaica-map-height: 400px; width: 600px"></mosaica-map><mosaica-map controls-corner="bottom-right" style="--mosaica-map-height: 400px; width: 600px"></mosaica-map>');
    const at = await p.evaluate(() =>
      [...document.querySelectorAll('mosaica-map')].map((m) => {
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

describe('the field column', () => {
  it('renders a card’s rows from the counts registered, and Tab from its search box reaches a row’s Highlight and shows it', async () => {
    const p = await page('<div style="width: 340px"><mosaica-filter-panel pinned="archive"></mosaica-filter-panel></div>');
    const seen = await p.evaluate(async () => {
      const rows = [
        {key: 'cs', count: 30},
        {key: 'math', count: 20}
      ];
      const columns: Record<string, {get(i: number): unknown}> = {
        group: {get: () => 'listed'},
        key: {get: (i: number) => rows[i]!.key},
        title: {get: () => null},
        count: {get: (i: number) => BigInt(rows[i]!.count)}
      };
      const table = {numRows: rows.length, getChild: (name: string) => columns[name] ?? null};
      const answer = {status: 'shown', view: 's0', refusal: null, summaries: [null], result: {tables: [{grouping: 0, total: 50, referenceTotal: null, groups: 2, sample: null, rows: table}], region: null, recomposed: false, identityKey: 'ik', next: null}};
      const listeners = new Set<() => void>();
      let aggregates = new Map<string, unknown>();
      const projections: Record<string, unknown> = {
        meta: {declaredScalars: [{name: 'archive', arrowType: 'u16', category: {}, render: false}], layers: [], views: [], filterOperands: [{column: 'archive', family: 'category', operands: ['in']}], selection: {maxAggregateTop: 1000}},
        status: {status: 'shown', sessionWarm: true, refusal: null, stale: false, expired: false, retrying: false},
        view: {id: 's0', inView: null},
        filters: {draft: {filter: {}, highlight: {}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
        legend: {colourBy: null, categories: {}, ranks: {}},
        artifacts: {served: [], colours: new Map()}
      };
      const store = {
        get: (name: string) => (name === 'aggregates' ? aggregates : projections[name]),
        subscribe: (fn: () => void) => {
          listeners.add(fn);
          return () => listeners.delete(fn);
        },
        setAggregate: (id: string, spec: unknown) => {
          aggregates = new Map(aggregates);
          if (spec === null) aggregates.delete(id);
          else aggregates.set(id, answer);
          queueMicrotask(() => listeners.forEach((fn) => fn()));
        },
        suggest: () => {},
        forgetSuggestions: () => {}
      };
      const panel = document.querySelector('mosaica-filter-panel') as HTMLElement & {store: unknown; updateComplete: Promise<unknown>};
      panel.store = store;
      for (let i = 0; i < 6; i++) {
        await new Promise((r) => requestAnimationFrame(r));
        await panel.updateComplete;
      }
      const card = panel.shadowRoot!.querySelector('mosaica-field-card')!;
      await (card as unknown as {updateComplete: Promise<unknown>}).updateComplete;
      const names = [...card.shadowRoot!.querySelectorAll('[part="name"]')].map((n) => n.textContent);
      card.shadowRoot!.querySelector('mosaica-filter')!.shadowRoot!.querySelector<HTMLInputElement>('[part="entry"]')!.focus();
      return {names};
    });
    expect(seen.names).toEqual(['cs', 'math']);
    await p.keyboard.press('Tab');
    const focused = await p.evaluate(() => {
      let at: Element | null = document.activeElement;
      while (at?.shadowRoot?.activeElement) at = at.shadowRoot.activeElement;
      const verbs = at?.closest('[part="verbs"]');
      return {part: at?.getAttribute('part') ?? null, row: at?.closest('[part~="row"]')?.getAttribute('data-key') ?? null, shown: verbs ? getComputedStyle(verbs).opacity : null};
    });
    expect(focused).toEqual({part: 'highlight', row: 'cs', shown: '1'});
  });
});
