import type {DepthChoice, Meta, Session, TileCounts, Timings, ViewportResult} from '@tessera/client';

/** What one loaded tile contributes. Counts come from the server; nothing here is derived. */
export type LoadedTile = {
  z: number;
  counts: TileCounts[];
  pointCount: number;
};

/**
 * The display states, kept distinct because collapsing them is how a fail-closed server becomes a
 * fail-misleading picture (client-interaction §9 — a named conformance item).
 *
 * `empty` (the principal sees nothing here) and `refused` (we do not know) are semantic opposites;
 * `loading` and `retrying` are neither; `shown` is the only one that may display counts.
 */
export type DisplayStatus = 'idle' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused';

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
  /** The whole current view's response — one request, not one per tile. */
  result: ViewportResult | null;
  worldPositions: Float32Array | null;
  status: DisplayStatus;
  lastError: {code: string; detail: string} | null;
  /** The depth the budget chose for the current view. */
  view: (DepthChoice & {requestedAt: number}) | null;
  /** Target marks on screen. */
  budget: number;
  /** Calibrated marks-per-tile; seeded from `theta_target_marks` and corrected downward only. */
  mTarget: number;
  /** The previous response's visible count over the view — the saturation term for depth choice. */
  lastVisibleInView: number | null;
  lastTimings: Timings | null;
  /** Pan-to-paint breakdown, in ms. */
  latency: {waited: number; fetch: number; server: number; total: number} | null;
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
