import {act} from 'react';
import {StrictMode, createElement, useState, type ReactNode} from 'react';
import {createRoot, type Root} from 'react-dom/client';
import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import type {Projections, Store, StoreOptions} from '@tesseradb/client';
import {fakeStore} from '../../components/test/fake-store.js';

/**
 * The hooks against a mocked `createStore`: the module is replaced so every store the hook
 * builds is a fake with a counted `dispose`, and the network never enters.
 */
const built: {options: StoreOptions; store: ReturnType<typeof fakeStore>; disposed: number}[] = [];
vi.mock('@tesseradb/client', async (importActual) => {
  const actual = await importActual<typeof import('@tesseradb/client')>();
  return {
    ...actual,
    createStore: (options: StoreOptions) => {
      const store = fakeStore();
      const entry = {options, store, disposed: 0};
      store.dispose = () => {
        entry.disposed += 1;
      };
      built.push(entry);
      return store;
    }
  };
});
const {useProjection, useTesseraStore} = await import('../src/index.js');

(globalThis as unknown as {IS_REACT_ACT_ENVIRONMENT: boolean}).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let host: HTMLElement;
beforeEach(() => {
  built.length = 0;
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

const render = async (node: ReactNode) => act(async () => root.render(node));

describe('useTesseraStore', () => {
  it('builds one store in an effect and disposes it on unmount', async () => {
    const seen: (Store | null)[] = [];
    function Host() {
      seen.push(useTesseraStore({viewerUrl: 'http://x', token: 't'}));
      return null;
    }
    await render(createElement(Host));
    expect(seen[0]).toBeNull();
    expect(built).toHaveLength(1);
    expect(seen.at(-1)).toBe(built[0]!.store);
    await act(async () => root.unmount());
    root = createRoot(host);
    expect(built[0]!.disposed).toBe(1);
  });

  it('under StrictMode the double mount leaks nothing: every store but the live one is disposed', async () => {
    let live: Store | null = null;
    function Host() {
      live = useTesseraStore({viewerUrl: 'http://x', token: 't'});
      return null;
    }
    await render(createElement(StrictMode, null, createElement(Host)));
    expect(built.length).toBeGreaterThanOrEqual(2);
    const alive = built.filter((b) => b.disposed === 0);
    expect(alive).toHaveLength(1);
    expect(live).toBe(alive[0]!.store);
    await act(async () => root.unmount());
    root = createRoot(host);
    expect(built.every((b) => b.disposed === 1)).toBe(true);
  });

  it('an inline authorise does not rebuild the store, and the latest one is the one called', async () => {
    const calls: string[] = [];
    let rerender = () => {};
    function Host({tag}: {tag: string}) {
      useTesseraStore({
        viewerUrl: 'http://x',
        authorise: async () => {
          calls.push(tag);
          return {token: tag, expiresAt: 0};
        }
      });
      return null;
    }
    function Outer() {
      const [tag, setTag] = useState('a');
      rerender = () => setTag('b');
      return createElement(Host, {tag});
    }
    await render(createElement(Outer));
    await act(async () => rerender());
    expect(built).toHaveLength(1);
    await built[0]!.options.authorise!();
    expect(calls).toEqual(['b']);
  });

  it('a different token is a different store', async () => {
    let setToken = (_: string) => {};
    function Host() {
      const [token, set] = useState('t1');
      setToken = set;
      useTesseraStore({viewerUrl: 'http://x', token});
      return null;
    }
    await render(createElement(Host));
    await act(async () => setToken('t2'));
    expect(built).toHaveLength(2);
    expect(built[0]!.disposed).toBe(1);
    expect(built[1]!.disposed).toBe(0);
  });
});

describe('useProjection', () => {
  it('re-renders on that projection and not on another, with a stable snapshot between', async () => {
    const store = fakeStore();
    const snapshots: Projections['status'][] = [];
    function Host() {
      snapshots.push(useProjection(store, 'status'));
      return null;
    }
    await render(createElement(Host));
    const first = snapshots.at(-1)!;
    await act(async () => store.set('view', {...store.get('view'), depth: 3}));
    expect(snapshots.at(-1)).toBe(first);
    const renders = snapshots.length;
    await act(async () => store.set('status', {...first, status: 'shown'}));
    expect(snapshots.length).toBeGreaterThan(renders);
    expect(snapshots.at(-1)!.status).toBe('shown');
    expect(snapshots.at(-1)).not.toBe(first);
  });

  it('null store reads null and subscribes to nothing', async () => {
    let value: unknown = 'unset';
    function Host() {
      value = useProjection(null, 'marks');
      return null;
    }
    await render(createElement(Host));
    expect(value).toBeNull();
  });

  it('unsubscribes on unmount', async () => {
    const store = fakeStore();
    let renders = 0;
    function Host() {
      renders += 1;
      useProjection(store, 'status');
      return null;
    }
    await render(createElement(Host));
    await act(async () => root.unmount());
    root = createRoot(host);
    const before = renders;
    await act(async () => store.set('status', {...store.get('status'), status: 'shown'}));
    expect(renders).toBe(before);
  });
});
