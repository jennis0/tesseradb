import {useEffect, useRef, useState, useSyncExternalStore} from 'react';
import {createStore, type ProjectionName, type Projections, type Store, type StoreOptions, type TokenSupplier} from '@tesseradb/client';

/**
 * `@tesseradb/react` — the headless store in React (design client-components §4, adapters).
 *
 * Two hooks. `useTesseraStore` owns a store for the component's lifetime and `useProjection`
 * reads one projection through `useSyncExternalStore`, which the store's projections were shaped
 * for: each is an immutable object replaced on change, so the snapshot is stable between changes
 * and React never sees a tear. Nothing here renders; the map, the panels and their wrappers are
 * behind the `/components` entry, so a hooks-only install pulls neither Lit nor deck.gl.
 *
 * **The store is built in an effect, not during render.** StrictMode mounts, unmounts and
 * remounts every component in development, running each effect's cleanup between; a store made
 * during render would be disposed by the first cleanup and never rebuilt, while one made with no
 * cleanup at all would leak a driver — its timers and its decode worker — on every remount. With
 * the build and the `dispose` paired inside one effect, the double mount costs one store built
 * and thrown away, and the one left alive is the one the second effect made. The first render
 * therefore sees `null`, which is the honest state: there is no store until the effect runs.
 */

export type UseTesseraStoreOptions = Omit<StoreOptions, 'authorise'> & {
  /**
   * A token supplier the store renews before expiry. Read through a ref, so a supplier written
   * inline — a new function on every render — neither rebuilds the store nor is left behind
   * holding a stale closure.
   */
  authorise?: TokenSupplier;
};

/**
 * A store for this component's lifetime. Rebuilt only when what identifies the session changes
 * — `viewerUrl`, `token`, `view` — and disposed when the component unmounts or those change;
 * `null` until the effect that builds it has run.
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
 * replaced and not when another is; `null` while there is no store (the first render under
 * {@link useTesseraStore}, or a host that has not opened one).
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
