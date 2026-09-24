import {useEffect, useRef, useState, useSyncExternalStore} from 'react';
import {createStore, type ProjectionName, type Projections, type Store, type StoreOptions, type TokenSupplier} from '@tesseradb/client';

/**
 * `@tesseradb/react`: the headless store in React, as two hooks. `useTesseraStore` owns a store
 * for the component's lifetime; `useProjection` reads one projection through
 * `useSyncExternalStore`. Each projection is an immutable object replaced on change, so snapshots
 * are stable. The components are behind the `/components` entry, so this entry pulls in neither
 * Lit nor deck.gl.
 *
 * The store is built and disposed in one effect. StrictMode mounts, unmounts and remounts in
 * development; a store made during render would be disposed by the first cleanup and not rebuilt.
 * The first render therefore sees `null`.
 */

export type UseTesseraStoreOptions = Omit<StoreOptions, 'authorise'> & {
  /**
   * A token supplier the store calls to renew before expiry. Read through a ref, so an inline
   * function neither rebuilds the store nor goes stale.
   */
  authorise?: TokenSupplier;
};

/**
 * A store for this component's lifetime, rebuilt when `viewerUrl`, `token` or `view` changes and
 * disposed on unmount. `null` until the effect that builds it has run.
 */
export function useTesseraStore(options: UseTesseraStoreOptions): Store | null {
  const [store, setStore] = useState<Store | null>(null);
  const latest = useRef(options);
  latest.current = options;
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
 * One projection, read through `useSyncExternalStore`. Re-renders when that projection is
 * replaced and not when another is; `null` while there is no store.
 */
export function useProjection<K extends ProjectionName>(store: Store, name: K): Projections[K];
export function useProjection<K extends ProjectionName>(store: Store | null, name: K): Projections[K] | null;
export function useProjection<K extends ProjectionName>(store: Store | null, name: K): Projections[K] | null {
  const subscribe = store ? (onChange: () => void) => store.subscribe(name, onChange) : unsubscribed;
  const getSnapshot = store ? () => store.get(name) : () => null;
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

export type {Count, Masked, ProjectionName, Projections, Store, StoreOptions, TokenSupplier} from '@tesseradb/client';
export {formatCount, formatMasked, NO_COUNT, NO_MASKED} from '@tesseradb/client';
