/**
 * React hooks for the headless store. {@link useTesseraStore} creates a store for a component's
 * lifetime, and {@link useProjection} reads one of its projections and re-renders when it changes.
 * The React components for the elements are in the `@tesseradb/react/components` entry. This
 * entry imports neither Lit nor deck.gl.
 *
 * @module @tesseradb/react
 */
import {useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore} from 'react';
import {createStore, type ProjectionName, type Projections, type Store, type StoreOptions, type TokenSupplier} from '@tesseradb/client';

/** The options of {@link useTesseraStore}: the options of {@link createStore}, with `authorise` read through a ref. */
export type UseTesseraStoreOptions = Omit<StoreOptions, 'authorise'> & {
  /**
   * A function the store calls for a new token before the current one expires. The hook reads it
   * through a ref, so an inline function neither rebuilds the store nor goes stale. A store serves
   * one viewer: to show another, call `clear()` on the store or give the component a new `key`.
   */
  authorise?: TokenSupplier;
};

/**
 * Creates a store with {@link createStore} for the component's lifetime, and disposes of it on
 * unmount. The store is rebuilt when `viewerUrl`, `token` or `view` changes, or when `authorise` is
 * given or removed. A change to any other option takes effect at the next rebuild.
 *
 * The store is built in an effect, so the first render returns `null`. Options with none of
 * `token`, `authorise` or `client` throw from that effect, as `createStore` does.
 */
export function useTesseraStore(options: UseTesseraStoreOptions): Store | null {
  const [store, setStore] = useState<Store | null>(null);
  const latest = useRef(options);
  useLayoutEffect(() => {
    latest.current = options;
  });
  const {viewerUrl, token, view} = options;
  const hasAuthorise = options.authorise !== undefined;
  useEffect(() => {
    const opts = latest.current;
    const built = createStore({
      ...opts,
      ...(hasAuthorise ? {authorise: () => latest.current.authorise!()} : {})
    });
    setStore(built);
    return () => {
      built.dispose();
      setStore((current) => (current === built ? null : current));
    };
  }, [viewerUrl, token, view, hasAuthorise]);
  return store;
}

const noop = () => {};
const unsubscribed = () => noop;

/**
 * Reads one projection of `store` through `useSyncExternalStore`, and re-renders the component
 * when that projection is replaced. A change to another projection does not re-render it.
 *
 * @param store - The store, such as the one {@link useTesseraStore} returns.
 * @param name - The projection to read, such as `marks` or `legend`.
 * @returns The projection, or `null` while `store` is `null`.
 */
export function useProjection<K extends ProjectionName>(store: Store, name: K): Projections[K];
export function useProjection<K extends ProjectionName>(store: Store | null, name: K): Projections[K] | null;
export function useProjection<K extends ProjectionName>(store: Store | null, name: K): Projections[K] | null {
  const subscribe = store ? (onChange: () => void) => store.subscribe(name, onChange) : unsubscribed;
  const getSnapshot = store ? () => store.get(name) : () => null;
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

export type {ProjectionName, Projections, Store, StoreOptions, TokenSupplier} from '@tesseradb/client';
