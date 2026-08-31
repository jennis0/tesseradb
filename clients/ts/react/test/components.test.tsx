import {act, createElement, createRef} from 'react';
import {createRoot, type Root} from 'react-dom/client';
import {afterEach, beforeEach, describe, expect, it} from 'vitest';
import {deep, fakeStore, settle, status} from '../../components/test/fake-store.js';
import {TesseraCount, TesseraItemCard, TesseraStatus, TesseraStore, type CountElement, type ItemCardElement} from '../src/components.js';

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
