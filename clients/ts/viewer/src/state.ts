import type {Composition, Meta, Session, Timings} from '@mosaicajs/client';
import type {DepthChoice} from '@mosaicajs/client/internal';

/**
 * The state the viewer's instrument panels read: a mirror of the store's projections for the
 * dataset and principal pickers and the depth and request readouts, and the measurements the store
 * reports on its `instruments` channel. The explorer's components read the store directly.
 */

export type DisplayStatus = 'idle' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused';

export type RequestFailure = {code: string; detail: string; at: number};

export type AppState = {
  meta: Meta | null;
  session: Session | null;
  view: string;
  /** Which of the dataset document's entries is being served. */
  datasetId: string;
  /** Whether a dataset change is in flight, when there is no session. */
  switching: boolean;
  termsLabel: string;
  terms: string[];
  /** The composition on screen, whose exact and provisional marks the readouts count. */
  frame: Composition | null;
  status: DisplayStatus;
  sessionWarm: boolean;
  lastError: {code: string; detail: string} | null;
  /** The depth the budget chose for the current view. */
  depthChoice: (DepthChoice & {requestedAt: number}) | null;
  /** Target marks on screen. */
  budget: number;
  /** The most artifacts one level shows in one tile, which the session's store was opened with. */
  artifactsPerTile: number;
  /** Calibrated marks-per-tile; seeded from `theta_target_marks` and corrected downward only. */
  mTarget: number;
  lastVisibleInView: number | null;
  lastTimings: Timings | null;
  /** Pan-to-paint breakdown, in milliseconds. */
  latency: {waited: number; fetch: number; server: number; total: number} | null;
  lastBytes: number;
  replicaBytes: number;
  replicaPoints: number;
  replicaBands: number;
  prefetched: number;
  /** How the last request split between held tiles and fetched ones. */
  lastPlan: {omitted: number; fetched: number} | null;
  inFlight: number;
  /** Refusals observed on the store's status, newest last. */
  failures: RequestFailure[];
  /** The colour and layer a session's store opens on. */
  colourBy: string | null;
  artifactLayer: string | null;
};

type Listener = (state: AppState) => void;

export type Store = {
  readonly state: AppState;
  update(mutate: (state: AppState) => void): void;
  subscribe(fn: Listener): () => void;
};

/** A minimal mutable store: one state object, in-place mutation, synchronous notification. */
export function createStore(initial: AppState): Store {
  const state = initial;
  const listeners = new Set<Listener>();
  return {
    get state() {
      return state;
    },
    update(mutate) {
      mutate(state);
      for (const fn of listeners) fn(state);
    },
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    }
  };
}

/** Coalesce rapid updates into one call per animation frame. */
export function coalesce(fn: () => void): () => void {
  let queued = false;
  return () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      fn();
    });
  };
}
