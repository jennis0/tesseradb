import type {
  Artifact,
  ArtifactDetail,
  CategoryValue,
  Composition,
  DepthChoice,
  Domain,
  Meta,
  Ranks,
  Session,
  Timings
} from '@tesseradb/client';

/**
 * The demo's own state: what its **instruments** read.
 *
 * The map, the status strip, the filters, the selection panel and the item card are
 * `@tesseradb/components` now and read the store directly. What remains here is a mirror of the
 * store's projections for the instrument panels — the dataset and principal pickers, the layer and
 * colour controls (componentised at step 3), the depth and request readouts — plus the measurement
 * numbers the §4 surface deliberately omits, which the store forwards on its demo-only
 * `instruments` channel.
 */

export type DisplayStatus = 'idle' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused';

export type RequestFailure = {code: string; detail: string; at: number};

export type AppState = {
  meta: Meta | null;
  session: Session | null;
  view: string;
  /** Which of `datasets.json`'s entries is being served — see `panels/source.ts`. */
  datasetId: string;
  /** Whether a dataset change is in flight — the interval with no session at all. */
  switching: boolean;
  termsLabel: string;
  terms: string[];
  /** The composition on screen, by reference — the readouts count its exact and provisional marks. */
  frame: Composition | null;
  status: DisplayStatus;
  sessionWarm: boolean;
  lastError: {code: string; detail: string} | null;
  /** The depth the budget chose for the current view. */
  depthChoice: (DepthChoice & {requestedAt: number}) | null;
  /** Target marks on screen. */
  budget: number;
  /** Calibrated marks-per-tile; seeded from `theta_target_marks` and corrected downward only. */
  mTarget: number;
  lastVisibleInView: number | null;
  lastTimings: Timings | null;
  /** Pan-to-paint breakdown, in ms. */
  latency: {waited: number; fetch: number; server: number; total: number} | null;
  lastBytes: number;
  replicaBytes: number;
  replicaPoints: number;
  replicaBands: number;
  prefetched: number;
  /** How the last ask split between held tiles and asked-for ones — the cache's effectiveness. */
  lastPlan: {omitted: number; fetched: number} | null;
  inFlight: number;
  /** Refusals observed on the store's status, newest last. */
  failures: RequestFailure[];
  colourBy: string | null;
  /** Resolved values per column, from the store's legend — the codes drawn, named. */
  categories: Record<string, CategoryValue[]>;
  categoryErrors: Record<string, {code: string; detail: string}>;
  ranks: Record<string, Ranks>;
  domains: Record<string, Domain>;
  artifactLayer: string | null;
  artifacts: Artifact[];
  artifactVersion: number;
  artifactStatus: 'idle' | 'loading' | 'shown' | 'refused';
  artifactError: {code: string; detail: string} | null;
  selectedArtifact: (ArtifactDetail & {id: bigint}) | null;
  artifactDetailError: {code: string; detail: string} | null;
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
