import type {Meta, Session, TileCounts, Timings} from '@tessera/client';

/** What one loaded tile contributes. Counts come from the server; nothing here is derived. */
export type LoadedTile = {
  z: number;
  counts: TileCounts[];
  pointCount: number;
};

export type RequestFailure = {tileId: string; code: string; detail: string; at: number};

export type AppState = {
  meta: Meta | null;
  session: Session | null;
  slice: string;
  termsLabel: string;
  terms: string[];
  /** Undefined means "do not send k", so the deployment's own ceiling applies (contracts §3.2). */
  k: number | undefined;
  underlayOffset: number;
  /** Keyed by deck tile id, replaced wholesale each time the viewport finishes loading. */
  tiles: Map<string, LoadedTile>;
  lastTimings: Timings | null;
  lastBytes: number;
  inFlight: number;
  failures: RequestFailure[];
  selected: {id: bigint; scalars: unknown[]; externalId: string | null} | null;
  selectedWorldXY: [number, number] | null;
  /** A refused `/v1/items` call. Distinct from `selected: null`, which means nothing is picked. */
  itemError: {code: string; detail: string} | null;
};

export type Store = {
  state: AppState;
  update(fn: (s: AppState) => void): void;
  subscribe(fn: (s: AppState) => void): void;
};

export function createStore(initial: AppState): Store {
  const listeners: ((s: AppState) => void)[] = [];
  const store: Store = {
    state: initial,
    update(fn) {
      fn(store.state);
      for (const l of listeners) l(store.state);
    },
    subscribe(fn) {
      listeners.push(fn);
    }
  };
  return store;
}

/**
 * Call `fn` at most once per animation frame.
 *
 * Every in-flight tile bumps `inFlight`, so an uncoalesced subscriber would rebuild the layer list
 * and replace the panels' DOM several times per tile — which also steals focus from whichever
 * control the user is dragging.
 */
export function coalesce(fn: () => void): () => void {
  let pending = false;
  return () => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => {
      pending = false;
      fn();
    });
  };
}
