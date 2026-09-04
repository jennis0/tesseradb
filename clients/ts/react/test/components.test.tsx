import {act, createElement, createRef} from 'react';
import {createRoot, type Root} from 'react-dom/client';
import {afterEach, beforeEach, describe, expect, it} from 'vitest';
import {deep, fakeStore, settle, status} from '../../components/test/fake-store.js';
import {TesseraCount, TesseraItemCard, TesseraKeyPicker, TesseraStatus, TesseraStore, TesseraViewPicker, type CountElement, type ItemCardElement, type ViewPickerElement} from '../src/components.js';

/**
 * The wrappers: an object prop lands as a property (never an attribute), an `on*` prop receives
 * the element's own event with its detail, and a `<TesseraStore>` above provides by context to a
 * wrapped panel below — the same store precedence the elements decide (`base.ts`).
 */
(globalThis as unknown as {IS_REACT_ACT_ENVIRONMENT: boolean}).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let host: HTMLElement;
beforeEach(() => {
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

describe('@tesseradb/react/components', () => {
  it('an object prop is set as a property and the element renders it', async () => {
    const ref = createRef<CountElement>();
    await act(async () => root.render(createElement(TesseraCount, {ref, label: 'shown', count: {shown: 221, total: 1_994_089, exact: true}})));
    await settle(host);
    expect(ref.current?.count).toEqual({shown: 221, total: 1_994_089, exact: true});
    expect(ref.current?.hasAttribute('count')).toBe(false);
    expect(deep(host, '[part="count"]')?.textContent).toBe('221 of 1,994,089');
  });

  it('an on* prop receives the element’s event with its detail', async () => {
    const ref = createRef<ItemCardElement>();
    const seen: string[] = [];
    await act(async () =>
      root.render(
        createElement(TesseraItemCard, {
          ref,
          item: {id: 7n, detail: {fields: {title: 'x'}, externalId: null, labels: [], views: [], scoped: {}}},
          onOpen: (e) => seen.push(e.detail.id)
        })
      )
    );
    await settle(host);
    (deep(host, '[part="open"]') as HTMLButtonElement).click();
    expect(seen).toEqual(['7']);
  });

  it('the pickers take a store as a property and switch through it', async () => {
    const meta = {
      apiVersion: 1,
      idset: 0,
      views: [
        {id: 'knn', displayName: 'knn', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null},
        {id: 'quarter:a', displayName: 'a', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: {group: 'quarter', key: 'a', metadata: {}}},
        {id: 'quarter:b', displayName: 'b', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: {group: 'quarter', key: 'b', metadata: {}}}
      ],
      groups: [{name: 'quarter', title: 'Quarter', membersOf: null, views: ['quarter:a', 'quarter:b']}],
      declaredScalars: [],
      layers: [],
      selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
      maxTilesPerRequest: 4096,
      filterOperands: []
    } as never;
    const store = fakeStore({meta, status: status({status: 'shown'})});
    store.set('view', {...store.get('view'), id: 'quarter:a'});
    const ref = createRef<ViewPickerElement>();
    const seen: {to: string; sameFrame: boolean}[] = [];
    await act(async () =>
      root.render(
        createElement(
          TesseraStore,
          {store},
          createElement(TesseraViewPicker, {ref, onViewSwitch: (e) => seen.push({to: e.detail.to, sameFrame: e.detail.sameFrame})}),
          createElement(TesseraKeyPicker, {})
        )
      )
    );
    await settle(host);
    // The store arrives by context, as it does for every other wrapped panel.
    expect(ref.current?.source).toBe('context');
    expect((deep(host, 'tessera-view-picker') as HTMLElement).shadowRoot!.querySelector('select')).not.toBeNull();
    const roster = (deep(host, 'tessera-key-picker') as HTMLElement).shadowRoot!.querySelector('select') as HTMLSelectElement;
    roster.value = 'quarter:b';
    roster.dispatchEvent(new Event('change'));
    expect(store.calls.filter((c) => c.name === 'setCurrentView').map((c) => c.args)).toEqual([['quarter:b']]);
    // The event bubbles to the provider above, not sideways: the view picker's handler is not the
    // key picker's, and a host wires whichever it wants.
    expect(seen).toEqual([]);
  });

  it('an on* prop on a picker receives its switch event, with the frames compared', async () => {
    const meta = {
      apiVersion: 1,
      idset: 0,
      views: [
        {id: 'knn', displayName: 'knn', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null},
        {id: 'pca64', displayName: 'pca64', quantisation: {xMin: -1, xMax: 1, yMin: -1, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}
      ],
      groups: [],
      declaredScalars: [],
      layers: [],
      selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
      maxTilesPerRequest: 4096,
      filterOperands: []
    } as never;
    const store = fakeStore({meta, status: status({status: 'shown'})});
    store.set('view', {...store.get('view'), id: 'knn'});
    const seen: {from: string; to: string; sameFrame: boolean}[] = [];
    await act(async () => root.render(createElement(TesseraViewPicker, {store, onViewSwitch: (e) => seen.push(e.detail)})));
    await settle(host);
    const select = (deep(host, 'tessera-view-picker') as HTMLElement).shadowRoot!.querySelector('select') as HTMLSelectElement;
    select.value = 'v:pca64';
    select.dispatchEvent(new Event('change'));
    expect(seen).toEqual([{from: 'knn', to: 'pca64', sameFrame: false}]);
  });

  it('a TesseraStore above provides by context to a wrapped panel below', async () => {
    const store = fakeStore({status: status({status: 'shown'}), view: {...fakeStore().get('view'), served: {shown: 5, total: 9, exact: true}}});
    await act(async () => root.render(createElement(TesseraStore, {store}, createElement(TesseraStatus))));
    await settle(host);
    const el = host.querySelector('tessera-status') as {activeStore: unknown; source: string};
    expect(el.activeStore).toBe(store);
    expect(el.source).toBe('context');
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('shown');
  });
});
