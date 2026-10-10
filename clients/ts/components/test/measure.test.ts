import {afterEach, describe, expect, it} from 'vitest';
import {type Artifact, type ArtifactsProjection, type Composition} from '@mosaicajs/client';
import {SessionArtifactTable, servedLineage} from '@mosaicajs/client/internal';
import '../src/map.js';
import type {MosaicaMap} from '../src/map.js';
import {fakeStore, mount, settle, status, type FakeStore} from './fake-store.js';

/**
 * `measure` is the map's one switch for the work that exists only for the probe: the frame-gap
 * loop, the cluster sample and the composition check. Off, none of it runs.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const frames = (n: number) =>
  new Promise<void>((resolve) => {
    const step = (left: number) => (left === 0 ? resolve() : requestAnimationFrame(() => step(left - 1)));
    step(n);
  });

function artifacts(ids: bigint[]): ArtifactsProjection {
  const served: Artifact[] = ids.map((id) => ({layer: 'clusters', mosaicaId: id, key: null, maskedCount: 1n, centroid: null, box: null, shape: null, content: [], parentIds: [], rung: 0, matched: null, highlighted: null, target: null, slot: null}));
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({mosaicaId: a.mosaicaId, layer: a.layer, parentIds: a.parentIds})));
  return {layer: 'clusters', layers: ['clusters'], served, colourServed: [], attached: new Map(), lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, held: served.length, table, servedOrdinals: new Set(ordinals), shapes: new Map(), colours: new Map(), palette: 'tableau10', overrides: new Map(), coverage: {current: 0, stale: 0}};
}

/** A frame whose drawn and served counts disagree, which the composition check refuses. */
const broken = {exactDrawn: 1, exactServed: 2, tiles: []} as unknown as Composition;

async function map(markup: string): Promise<{el: MosaicaMap; store: FakeStore; host: HTMLElement}> {
  const host = await mount(markup);
  const el = host.querySelector('mosaica-map') as MosaicaMap;
  const store = fakeStore({status: status({})});
  el.store = store;
  await settle(host);
  return {el, store, host};
}

describe('<mosaica-map measure>', () => {
  it('off, runs no frame loop, samples no clusters and checks no composition', async () => {
    const {el, store} = await map('<mosaica-map></mosaica-map>');
    store.set('artifacts', artifacts([7n, 9n]));
    expect(() => store.set('view', {...store.get('view'), composition: broken})).not.toThrow();
    await frames(4);
    expect(el.probe.timings.frame.n).toBe(0);
    expect(el.probe.cluster.servedIds).toEqual([]);
  });

  it('on, fills the frame gaps and the cluster sample, and checks each composition', async () => {
    const {el, store} = await map('<mosaica-map measure></mosaica-map>');
    store.set('artifacts', artifacts([7n, 9n]));
    await frames(4);
    expect(el.probe.timings.frame.n).toBeGreaterThan(0);
    expect(el.probe.cluster.servedIds).toEqual(['7', '9']);
    expect(() => store.set('view', {...store.get('view'), composition: broken})).toThrow();
  });

  it('turned on after the store is adopted, samples what is already served', async () => {
    const {el, store, host} = await map('<mosaica-map></mosaica-map>');
    store.set('artifacts', artifacts([3n]));
    el.measure = true;
    await settle(host);
    expect(el.probe.cluster.servedIds).toEqual(['3']);
    await frames(4);
    expect(el.probe.timings.frame.n).toBeGreaterThan(0);
    el.measure = false;
    await settle(host);
    const counted = el.probe.timings.frame.n;
    await frames(4);
    expect(el.probe.timings.frame.n).toBe(counted);
  });

  it('on reconnect, runs the frame loop only if measure is on', async () => {
    const off = await map('<mosaica-map></mosaica-map>');
    off.el.remove();
    off.host.append(off.el);
    await settle(off.host);
    await frames(4);
    expect(off.el.probe.timings.frame.n).toBe(0);

    const on = await map('<mosaica-map measure></mosaica-map>');
    on.el.remove();
    await frames(2);
    const whileAway = on.el.probe.timings.frame.n;
    await frames(4);
    expect(on.el.probe.timings.frame.n).toBe(whileAway);
    on.host.append(on.el);
    await settle(on.host);
    await frames(4);
    expect(on.el.probe.timings.frame.n).toBeGreaterThan(whileAway);
  });
});
