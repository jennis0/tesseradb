import {afterEach, describe, expect, it, vi} from 'vitest';
import type {Meta} from '@mosaica/client';
import '../src/map.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

/**
 * What a hover says a point is. The host names the field that titles a point (`title-field`);
 * unset, the title is the point's id and nothing is fetched. A field the marks do not carry is
 * asked of the record through the store's `describe`, which opens no card, once the pointer rests.
 */

afterEach(() => {
  document.body.innerHTML = '';
  vi.useRealTimers();
});

const meta = (scalars: {name: string; arrowType: string}[]): Meta =>
  ({
    declaredScalars: scalars.map((s) => ({...s, category: null, render: false, index: true}))
  }) as unknown as Meta;

type HoverMap = {
  store: unknown;
  worldAt: unknown;
  slab: {markAt: unknown};
  onHover(info: unknown): void;
  hover: {title: string; lines: string[]} | null;
};

async function map(scalars: {name: string; arrowType: string}[], attributes = '') {
  const host = await mount(`<mosaica-map ${attributes}></mosaica-map>`);
  const el = host.querySelector('mosaica-map') as unknown as HoverMap;
  const store = fakeStore({status: status({}), meta: meta(scalars)});
  el.store = store;
  // No deck here, so the map cannot unproject: the hovered artifact is resolved from a world
  // point and these tests are about the tooltip, not about the contours.
  el.worldAt = () => null;
  await settle(host);
  return {el, store};
}

/** Deck's answer for a mark: the slot layer, the row, and the ids that layer was given. */
const markAt = (id: bigint) => ({index: 0, x: 10, y: 20, sourceLayer: {id: 'marks-p0', props: {tesseraIds: new BigUint64Array([id])}}});

/** The marks under the pointer carrying `scalars`, one row each. */
const carrying = (scalars: Record<string, {arrowType: string; value: unknown}>) => () => ({
  band: {scalars: Object.fromEntries(Object.entries(scalars).map(([k, v]) => [k, {arrowType: v.arrowType, values: [v.value]}]))},
  i: 0
});

describe('a hover over a mark', () => {
  it('shows the id at once and the title field from the record once the pointer has rested', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'name', arrowType: 'text'}], 'title-field="name"');
    store.describe = async () => ({name: 'Sheena McCurrach Art', country: 'GB'});

    el.onHover(markAt(31728047486770n));
    expect(el.hover?.title).toBe('#31728047486770');

    await vi.advanceTimersByTimeAsync(300);
    expect(el.hover?.title).toBe('Sheena McCurrach Art');
  });

  it('with no title field, shows the id and asks for nothing, whatever the bundle declares', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'name', arrowType: 'text'}]);
    let asked = 0;
    store.describe = async () => {
      asked += 1;
      return {name: 'unreachable'};
    };

    el.onHover(markAt(7n));
    await vi.advanceTimersByTimeAsync(300);
    expect(asked).toBe(0);
    expect(el.hover?.title).toBe('#7');
  });

  it('asks once for the mark the pointer settled on, not for the ones it crossed', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'name', arrowType: 'text'}], 'title-field="name"');
    const asked: string[] = [];
    store.describe = async (id: bigint) => {
      asked.push(id.toString());
      return {name: `n-${id}`};
    };

    for (const id of [1n, 2n, 3n, 4n]) {
      el.onHover(markAt(id));
      await vi.advanceTimersByTimeAsync(20);
    }
    await vi.advanceTimersByTimeAsync(300);
    expect(asked).toEqual(['4']);
  });

  it('reads a title field the marks carry off the mark, and asks for nothing', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'code', arrowType: 'utf8'}], 'title-field="code"');
    el.slab.markAt = carrying({code: {arrowType: 'utf8', value: 'GB-LND'}});
    let asked = 0;
    store.describe = async () => {
      asked += 1;
      return null;
    };

    el.onHover(markAt(5n));
    await vi.advanceTimersByTimeAsync(300);
    expect(el.hover?.title).toBe('GB-LND');
    expect(asked).toBe(0);
  });

  it('never takes the title from tooltip-fields: they are lines beneath the id', async () => {
    const {el} = await map([{name: 'code', arrowType: 'utf8'}, {name: 'name', arrowType: 'utf8'}], 'tooltip-fields="code,name"');
    el.slab.markAt = carrying({code: {arrowType: 'utf8', value: 'GB-LND'}, name: {arrowType: 'utf8', value: 'London'}});

    el.onHover(markAt(5n));
    expect(el.hover?.title).toBe('#5');
    expect(el.hover?.lines).toEqual(['GB-LND', 'London']);
  });

  it('shows a timestamp in full to the second, in UTC, without its fraction', async () => {
    const at = 1_700_000_000_123_000;
    const {el} = await map([{name: 'published', arrowType: 'timestamp_us'}], 'tooltip-fields="published"');
    el.slab.markAt = carrying({published: {arrowType: 'timestamp_us', value: at}});

    el.onHover(markAt(5n));
    expect(el.hover?.lines).toEqual(['14 November 2023, 22:13:20 UTC']);
  });
});

describe('a hover across a change of viewer', () => {
  it('is dropped when the store’s meta goes null', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'name', arrowType: 'text'}], 'title-field="name"');
    store.describe = async () => ({name: 'Sheena McCurrach Art'});
    el.onHover(markAt(31728047486770n));
    await vi.advanceTimersByTimeAsync(300);
    expect(el.hover?.title).toBe('Sheena McCurrach Art');

    store.set('meta', null);
    expect(el.hover).toBeNull();
  });
});

describe('a hover under a highlight', () => {
  it('reads a lit mark, and a dulled one, from the pass that drew it', async () => {
    const {el} = await map([{name: 'name', arrowType: 'utf8'}], 'title-field="name"');
    let asked: number | null = null;
    el.slab.markAt = (slot: number) => {
      asked = slot;
      return carrying({name: {arrowType: 'utf8', value: 'London'}})();
    };
    for (const pass of ['lit', 'dull']) {
      asked = null;
      el.onHover({index: 0, x: 10, y: 20, sourceLayer: {id: `marks-p3-${pass}`, props: {tesseraIds: new BigUint64Array([5n])}}});
      expect(asked).toBe(3);
      expect(el.hover?.title).toBe('London');
    }
  });
});

describe('the columns a hover reads', () => {
  /** The columns the map has asked of `store`, as its last `setPointColumns` left them. */
  const askedOf = (store: ReturnType<typeof fakeStore>): unknown[] =>
    (store.calls.filter((c) => c.name === 'setPointColumns').at(-1)?.args[1] as unknown[] | undefined) ?? [];

  it('are asked of the store, again when they change, and withdrawn from a store the map leaves', async () => {
    const {el, store} = await map([{name: 'name', arrowType: 'text'}], 'title-field="name" tooltip-fields="score, year"');
    expect(askedOf(store)).toEqual(['score', 'year', 'name']);

    const host = (el as unknown as HTMLElement).parentElement!;
    (el as unknown as {tooltipFields: string}).tooltipFields = 'year';
    await settle(host);
    expect(askedOf(store)).toEqual(['year', 'name']);

    const next = fakeStore({status: status({}), meta: meta([])});
    el.store = next;
    await settle(host);
    expect(askedOf(store)).toEqual([]);
    expect(askedOf(next)).toEqual(['year', 'name']);
  });
});

describe('the map in the tab order', () => {
  it('is one stop, after its tools and before the other corners, and focus() lands on it', async () => {
    const {el} = await map([]);
    const root = (el as unknown as HTMLElement).shadowRoot!;
    const stops = [...root.querySelectorAll<HTMLElement>('button, [tabindex]')].filter((e) => e.tabIndex >= 0).map((e) => e.getAttribute('part') ?? e.getAttribute('aria-label'));
    expect(stops).toEqual(['Pan', 'Box select', 'Lasso select', 'Fit to extent', 'canvas']);
    expect((el as unknown as HTMLElement).tabIndex).toBe(-1);
    (el as unknown as HTMLElement).focus();
    expect(root.activeElement?.getAttribute('part')).toBe('canvas');
  });

  it('moves a tabindex the host sets to the canvas, and names the canvas by the host’s aria-label', async () => {
    const {el} = await map([]);
    const host = el as unknown as HTMLElement;
    host.setAttribute('tabindex', '-1');
    host.setAttribute('aria-label', 'Papers');
    await (el as unknown as {updateComplete: Promise<unknown>}).updateComplete;
    const canvas = host.shadowRoot!.querySelector<HTMLElement>('[part="canvas"]')!;
    expect(canvas.tabIndex).toBe(-1);
    expect(canvas.getAttribute('aria-label')).toBe('Papers');
    host.setAttribute('tabindex', '2');
    await (el as unknown as {updateComplete: Promise<unknown>}).updateComplete;
    expect(canvas.tabIndex).toBe(2);
    expect(host.getAttribute('tabindex')).toBe('-1');
  });
});

describe('the tooltip through a press', () => {
  it('goes at a press, stays away while the pointer is held, and comes back on the next hover after it', async () => {
    const {el} = await map([]);
    el.onHover(markAt(5n));
    expect(el.hover?.title).toBe('#5');
    const canvas = (el as unknown as HTMLElement).shadowRoot!.querySelector('[part="canvas"]')!;
    canvas.dispatchEvent(new PointerEvent('pointerdown', {button: 0, bubbles: true, composed: true}));
    expect(el.hover).toBeNull();
    el.onHover(markAt(5n));
    expect(el.hover).toBeNull();
    window.dispatchEvent(new PointerEvent('pointerup'));
    el.onHover(markAt(5n));
    expect(el.hover?.title).toBe('#5');
  });
});
