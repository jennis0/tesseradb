import type {CategoryValue, DepthChoice, Meta, Session, Timings} from '@tessera/client';
import type {Assembled} from './assemble.js';
import type {Domain, Ranks} from './colour.js';

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
  /**
   * The current view, assembled from held bands rather than from one response.
   *
   * Tiles reach it by three routes — their own band, an ancestor's restricted by Morton prefix, or
   * the union of held descendants — and only the first is the served set. `Assembled.tiles` carries
   * which, and every non-exact tile is stale-marked with no count shown against it.
   */
  assembled: Assembled | null;
  status: DisplayStatus;
  /**
   * Whether this session has received its first viewport response.
   *
   * The visible set materialises lazily inside that first request, and at 10^9 items a broad
   * principal's union measured ~10 s — re-paid per session, since nothing is shared across them
   * yet. Until it lands, "loading" means something different from every later loading state, and
   * the panel says so instead of letting a session's establishment read as a hung fetch.
   */
  sessionWarm: boolean;
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
  /** Bytes the replica holds, so the cache's cost is visible rather than implicit. */
  replicaBytes: number;
  /** Points held and bands holding them — the figure a cache budget should be argued from. */
  replicaPoints: number;
  replicaBands: number;
  /** Tiles the anticipatory ring fetched on the last idle pause — look-ahead, made visible. */
  prefetched: number;
  /** How the last ask split between held tiles and asked-for ones — the cache's effectiveness. */
  lastPlan: {omitted: number; fetched: number} | null;
  inFlight: number;
  failures: RequestFailure[];
  selected: {id: bigint; scalars: unknown[]; externalId: string | null} | null;
  selectedWorldXY: [number, number] | null;
  /** A refused `/v1/items` call. Distinct from `selected: null`, which means nothing is picked. */
  itemError: {code: string; detail: string} | null;

  /**
   * The declared column marks are coloured by, or `null` for the uniform colour.
   *
   * **Changing it must not refetch.** Every declared column is already in the response, so a
   * different encoding is a layer rebuild — which is also what makes the switch a usable check
   * that no selection changed: the mark count cannot move.
   */
  colourBy: string | null;
  /**
   * Resolved category values per **column**, not per vocabulary.
   *
   * Keyed by column even though two columns may share a vocabulary, because visibility is
   * per-column (contracts §3.2): a value resolvable under one is not thereby resolvable under
   * another. Sharing the map would be the one shortcut that turns a correct gate into a leak.
   */
  categories: Record<string, CategoryValue[]>;
  /** A refused `/v1/categories` call, per column — notably the ⊘ `per_viewer` refusal. */
  categoryErrors: Record<string, {code: string; detail: string}>;
  /**
   * Palette rank per code, per column: assigned by observed frequency and never reordered, so a
   * pan cannot change what a colour means. See `colour.ts`'s `extendRanks`.
   */
  ranks: Record<string, Ranks>;
  /**
   * Sticky numeric domains per column: widened as marks arrive, never narrowed, cleared on
   * principal change. See `colour.ts` for why the server does not supply these.
   */
  domains: Record<string, Domain>;
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
