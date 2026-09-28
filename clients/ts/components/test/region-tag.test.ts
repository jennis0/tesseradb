import {afterEach, describe, expect, it} from 'vitest';
import type {RegionProjection} from '@tesseradb/client';
import '../src/map.js';
import {deep, fakeStore, meta, mount, settle, status} from './fake-store.js';

/** The drawn region's tag: its count on the region's edge, and a × that clears the selection. */

afterEach(() => {
  document.body.innerHTML = '';
});

const region = (outside: boolean): RegionProjection => ({
  shape: {kind: 'box', bbox: [0.2, 0.2, 0.6, 0.6], outside},
  status: 'shown',
  refusal: null,
  visible: {value: 9000, exact: true},
  matched: {value: 4812, exact: true},
  served: {shown: 10, total: 4812, exact: true},
  verdict: {exact: true, depth: null},
  held: {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0}
});

async function mapWith(r: RegionProjection | null) {
  const host = await mount('<tessera-map></tessera-map>');
  const map = host.querySelector('tessera-map') as HTMLElement & {store: unknown};
  const store = fakeStore({meta: meta(), status: status({}), region: r});
  map.store = store;
  await settle(host);
  return {host, store};
}

describe('<tessera-map> region tag', () => {
  it('shows the count matched inside a drawn region, and clears the selection from its ×', async () => {
    const {host, store} = await mapWith(region(false));
    const tag = deep(host, '[part="region-tag"]')!;
    expect(tag).not.toBeNull();
    expect(tag.querySelector('tessera-count')).not.toBeNull();
    (tag.querySelector('button') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'select').map((c) => c.args[0])).toEqual([null]);
  });

  it('has no tag with no region, or for a region drawn as an artifact', async () => {
    expect(deep((await mapWith(null)).host, '[part="region-tag"]')).toBeNull();
    document.body.innerHTML = '';
    const artifact: RegionProjection = {...region(false), shape: {kind: 'artifact', id: 3n, outside: false}} as RegionProjection;
    expect(deep((await mapWith(artifact)).host, '[part="region-tag"]')).toBeNull();
  });
});
