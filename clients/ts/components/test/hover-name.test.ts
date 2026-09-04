import {afterEach, describe, expect, it, vi} from 'vitest';
import type {Meta} from '@tesseradb/client';
import '../src/map.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

/**
 * What a hover says a point is.
 *
 * **A mark cannot carry its own name.** A text column lives in the record blob and is refused
 * `render` (records-and-search §3), so no viewport response holds one and the opaque id is the
 * whole of what the marks stream knows. The map therefore asks `/v1/items` — through the store's
 * `describe`, which writes no projection and so opens no card — once the pointer has rested, and
 * replaces the id with the name when the answer arrives.
 */

afterEach(() => {
  document.body.innerHTML = '';
  vi.useRealTimers();
});

/** Meta declaring one text column and one that is not, so the choice is a choice. */
const meta = (scalars: {name: string; arrowType: string}[]): Meta =>
  ({
    declaredScalars: scalars.map((s) => ({...s, category: null, render: false, index: true}))
  }) as unknown as Meta;

async function map(scalars: {name: string; arrowType: string}[]) {
  const host = await mount('<tessera-map></tessera-map>');
  const el = host.querySelector('tessera-map') as unknown as {
    store: unknown;
    worldAt: unknown;
    onHover(info: unknown): void;
    hover: {title: string} | null;
  };
  const store = fakeStore({status: status({}), meta: meta(scalars)});
  el.store = store;
  // No deck in jsdom, so the map cannot unproject: the hovered artifact is resolved from a world
  // point and this test is about the tooltip, not about the contours.
  el.worldAt = () => null;
  await settle(host);
  return {el, store};
}

/** Deck's answer for a mark: the slot layer, the row, and the ids that layer was given. */
const markAt = (id: bigint) => ({index: 0, x: 10, y: 20, sourceLayer: {id: 'marks-p0', props: {tesseraIds: new BigUint64Array([id])}}});

describe('a hover over a mark', () => {
  it('shows the id at once and the record’s name once the pointer has rested', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([
      {name: 'confidence', arrowType: 'f32'},
      {name: 'name', arrowType: 'text'}
    ]);
    store.describe = async () => ({name: 'Sheena McCurrach Art', country: 'GB'});

    el.onHover(markAt(31728047486770n));
    // Before the dwell: the id, because that is all the marks stream carries.
    expect(el.hover?.title).toBe('#31728047486770');

    await vi.advanceTimersByTimeAsync(300);
    expect(el.hover?.title).toBe('Sheena McCurrach Art');
  });

  it('asks for nothing where the corpus declares no string column', async () => {
    vi.useFakeTimers();
    const {el, store} = await map([{name: 'confidence', arrowType: 'f32'}]);
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
    const {el, store} = await map([{name: 'name', arrowType: 'text'}]);
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
});
