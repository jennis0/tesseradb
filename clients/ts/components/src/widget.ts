import {
  createStore,
  emptyDraft,
  type ColumnDraft,
  type FilterDraft,
  type FilterExpr,
  type FilterOperandSet,
  type Store,
  type TokenSupplier
} from '@tesseradb/client';
import {idString} from './base.js';
import type {TesseraExplorer} from './explorer.js';
import './explorer.js';

/**
 * The notebook widget's JavaScript half (design client-components §7): what anywidget evaluates
 * as `_esm`, exported from the single-file bundle so the widget and a page with no build step
 * load one file. The kernel half is `clients/py/tesseradb/widget.py`; this module and that one are
 * the two ends of one protocol, and the protocol is the thing to read first.
 *
 * **The token is never model state.** Nothing here calls `model.set` with a token, and no traitlet
 * carries one. `initialize` sends `ready` — once per model, so two views of one widget share a
 * token and two widgets do not — and the kernel answers with the token as a **custom message**,
 * which no route that serialises widget state can save: not the frontend's "save widget state",
 * not `nbconvert --execute`, not papermill, not a headless run where no frontend mounts. The
 * store's token supplier is the other half: its first call awaits the answer to `ready`, and every
 * later call (the store renews a beat before expiry, and again on a refusal that means the session
 * ended) sends `reauthorise` and awaits the next answer. A view rendered after a page reload is a
 * new model, so it sends `ready` again.
 *
 * **What crosses the kernel boundary is control and selection, never data.** `url` and `view` come
 * down; `bbox`, `layers`, `colour_by` and `filters` go both ways; `selected`, `selected_artifact`
 * and `region` go up. Up-syncs happen **at the settle** — when the store's status reaches `shown`
 * for a new composition, and when a region's counts arrive — never per frame, so the kernel is
 * off the pan path (client-interaction §7). Ids cross as decimal strings: a `tessera_id` is a
 * `u64`, which is not a JS number, and a `BigInt` does not serialise.
 *
 * **The store is per model, the explorer per view.** anywidget calls `initialize` once per model
 * and `render` once per view of it; the store lives with the model and every view mounts its own
 * `<tessera-explorer .store>`, so a second view of one widget draws the same replica rather than
 * fetching it twice. The explorer never disposes a store it was handed; the store is disposed
 * when the model is.
 *
 * **Echo guard.** Backbone fires `change:<key>` for a `model.set` made here as much as for one
 * made in the kernel, so every up-sync sets a flag the down-sync handlers read and ignore. Without
 * it a `bbox` set in Python would fit the camera, the settle would report the box actually shown
 * (the aspect differs), and that report would fit the camera again, for ever.
 */

/** The subset of anywidget's `AnyModel` this module uses — typed here so a test can fake it. */
export type WidgetModel = {
  get(key: string): unknown;
  set(key: string, value: unknown): void;
  save_changes(): void;
  on(event: string, cb: (...args: unknown[]) => void): void;
  off(event: string, cb?: (...args: unknown[]) => void): void;
  send(content: unknown, callbacks?: unknown, buffers?: ArrayBuffer[]): void;
};

/** What the kernel sends the page. `expires_at` is seconds since the epoch, as `/session/authorise` reports it. */
export type KernelMessage =
  | {type: 'token'; token: string; expires_at: number}
  | {type: 'refused'; detail: string};

/** What the page sends the kernel. */
export type PageMessage = {type: 'ready'} | {type: 'reauthorise'} | {type: 'error'; what: string; detail: string};

type ModelState = {
  store: Store | null;
  supplier: TokenSupplier;
  /** Every explorer mounted for this model, so a down-synced `bbox` reaches each camera. */
  views: Set<TesseraExplorer>;
  /** The box the camera last reported, in data coordinates — what the settle syncs up. */
  lastBbox: [number, number, number, number] | null;
  /** A `bbox` the kernel set before any map could fit it (no view, or no meta yet); fitted at the first chance. */
  pendingFit: [number, number, number, number] | null;
  syncingUp: boolean;
  unsubscribe: (() => void) | null;
  dispose(): void;
};

const states = new WeakMap<object, ModelState>();

/** For a test: the state behind a model, once `initialize` has run. */
export function stateOf(model: object): {store: Store | null; views: number} | null {
  const s = states.get(model);
  return s ? {store: s.store, views: s.views.size} : null;
}

/**
 * The token supplier over the comm. One outstanding request at a time: a `ready` (the first) or a
 * `reauthorise` (every later one) is sent, and the promise settles on the next `token` or
 * `refused` message. A second call while one is outstanding shares its promise rather than asking
 * the kernel twice.
 */
function tokenSupplier(model: WidgetModel, onMessage: (cb: (msg: KernelMessage) => void) => void): TokenSupplier {
  let outstanding: {resolve(v: {token: string; expiresAt: number}): void; reject(e: Error): void} | null = null;
  let asked = false;
  onMessage((msg) => {
    if (!outstanding) return;
    if (msg.type === 'token') {
      const o = outstanding;
      outstanding = null;
      o.resolve({token: msg.token, expiresAt: msg.expires_at});
    } else if (msg.type === 'refused') {
      const o = outstanding;
      outstanding = null;
      o.reject(new Error(msg.detail));
    }
  });
  let inflight: Promise<{token: string; expiresAt: number}> | null = null;
  return () => {
    if (inflight) return inflight;
    inflight = new Promise<{token: string; expiresAt: number}>((resolve, reject) => {
      outstanding = {resolve, reject};
      model.send(asked ? {type: 'reauthorise'} : {type: 'ready'});
      asked = true;
    }).finally(() => {
      inflight = null;
    });
    return inflight;
  };
}

// ---- filters: the wire's expression, as the store's draft --------------------------------------

/**
 * The widget's `filters` traitlet carries the composed expression — the wire's form, which is
 * what a Python caller can write and read without learning the panel's draft — while the store
 * takes a draft, one control per filterable column. This inverts `composeFilters` over the shapes
 * a draft can express: a leaf per column, conjoined at the top. `any_of`, `none_of`, a nested
 * `all_of`, two leaves on one column, an unknown column and an operator the column's family
 * cannot hold are refused with a reason, and the refusal goes to the kernel as an `error` message
 * rather than silently filtering nothing. `null` is the unfiltered request.
 */
export function draftOf(expr: FilterExpr | null, operands: FilterOperandSet[]): FilterDraft {
  const draft = emptyDraft(operands);
  if (expr === null) return draft;
  const leaves: FilterExpr[] = 'all_of' in expr && Array.isArray(expr.all_of) ? (expr.all_of as FilterExpr[]) : [expr];
  const seen = new Set<string>();
  for (const leaf of leaves) {
    const keys = Object.keys(leaf);
    if (keys.length !== 1) throw new Error(`a filter leaf names exactly one column; got ${JSON.stringify(leaf)}`);
    const column = keys[0]!;
    if (column === 'all_of' || column === 'any_of' || column === 'none_of') {
      throw new Error(`only a conjunction of column leaves can be set from the widget; got ${column}`);
    }
    if (seen.has(column)) throw new Error(`two leaves on ${column}; the widget holds one per column`);
    seen.add(column);
    const control = draft[column];
    if (!control) throw new Error(`${column} is not a filterable column of this view`);
    draft[column] = controlOf(column, control, (leaf as Record<string, unknown>)[column] as Record<string, unknown>);
  }
  return draft;
}

function controlOf(column: string, control: ColumnDraft, op: Record<string, unknown>): ColumnDraft {
  const names = Object.keys(op);
  if (names.length !== 1) throw new Error(`${column}: an operator has exactly one key; got ${names.join(', ')}`);
  const name = names[0]!;
  const value = op[name];
  const bad = () => new Error(`${column}: a ${control.family} column cannot hold ${name}`);
  switch (control.family) {
    case 'text': {
      if (name === 'phrase' && typeof value === 'string') return {family: 'text', query: value, mode: 'phrase'};
      if (name === 'match' && typeof value === 'string') return {family: 'text', query: value, mode: 'all'};
      if (name === 'match' && value && typeof value === 'object') {
        const m = value as {query: string; minimum_should_match?: number};
        return {family: 'text', query: m.query, mode: m.minimum_should_match === 1 ? 'any' : 'all'};
      }
      throw bad();
    }
    case 'string':
    case 'keyword': {
      if ((name === 'eq' || name === 'prefix' || name === 'contains') && typeof value === 'string') {
        return {family: control.family, needle: value, op: name};
      }
      throw bad();
    }
    case 'category': {
      if (name === 'in' && Array.isArray(value)) return {family: 'category', keys: value.map(String)};
      if (name === 'eq') return {family: 'category', keys: [String(value)]};
      throw bad();
    }
    case 'numeric': {
      if (name === 'range' && value && typeof value === 'object') {
        const r = value as {gte?: number; lte?: number; gt?: number; lt?: number};
        if (r.gt !== undefined || r.lt !== undefined) throw new Error(`${column}: the widget's range is inclusive (gte, lte)`);
        return {family: 'numeric', gte: r.gte ?? null, lte: r.lte ?? null};
      }
      throw bad();
    }
  }
}

// ---- the anywidget entry ------------------------------------------------------------------------

function sameBbox(a: number[] | null, b: number[] | null): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  return a.every((v, i) => v === b[i]);
}

function buildStore(model: WidgetModel, supplier: TokenSupplier, factory: StoreFactory): Store | null {
  const url = model.get('url');
  if (typeof url !== 'string' || !url) return null;
  const view = model.get('view');
  return factory({viewerUrl: url, authorise: supplier, ...(typeof view === 'string' && view ? {view} : {})});
}

/** How a store is built — `createStore` unless a test injects one with no network. */
export type StoreFactory = (options: {viewerUrl: string; authorise: TokenSupplier; view?: string}) => Store;

export function initialize({model, storeFactory = createStore}: {model: WidgetModel; storeFactory?: StoreFactory}): () => void {
  const listeners: ((msg: KernelMessage) => void)[] = [];
  const onCustom = (...args: unknown[]) => {
    const msg = args[0] as KernelMessage;
    for (const l of listeners) l(msg);
  };
  model.on('msg:custom', onCustom);
  const supplier = tokenSupplier(model, (cb) => listeners.push(cb));

  const state: ModelState = {
    store: null,
    supplier,
    views: new Set(),
    lastBbox: null,
    pendingFit: null,
    syncingUp: false,
    unsubscribe: null,
    dispose() {
      state.unsubscribe?.();
      state.unsubscribe = null;
      state.store?.dispose();
      state.store = null;
      model.off('msg:custom', onCustom);
    }
  };
  states.set(model, state);

  const report = (what: string, e: unknown) => {
    model.send({type: 'error', what, detail: e instanceof Error ? e.message : String(e)} satisfies PageMessage);
  };

  // Up-sync, at the settle and at the region's answer — one `save_changes` per settle.
  const syncUp = (patch: Record<string, unknown>) => {
    state.syncingUp = true;
    try {
      let dirty = false;
      for (const [k, v] of Object.entries(patch)) {
        const cur = model.get(k);
        if (JSON.stringify(cur) === JSON.stringify(v)) continue;
        model.set(k, v);
        dirty = true;
      }
      if (dirty) model.save_changes();
    } finally {
      state.syncingUp = false;
    }
  };

  let lastComposition: object | null = null;
  let lastRegion: object | null = null;
  let lastSelection: object | null = null;
  const follow = (store: Store) => {
    state.unsubscribe?.();
    lastComposition = null;
    lastRegion = null;
    lastSelection = null;
    state.unsubscribe = store.subscribe(() => {
      if (state.pendingFit && store.get('meta')) fit(state.pendingFit);
      const status = store.get('status');
      const view = store.get('view');
      const patch: Record<string, unknown> = {};
      // The settle: a new composition presented as `shown` (or `empty` — a masked-out view is a
      // settled one). The control traits are read from the store, so what the kernel holds is what
      // the store applied, whichever side set it.
      const settled = (status.status === 'shown' || status.status === 'empty') && view.composition !== lastComposition;
      if (settled) {
        lastComposition = view.composition;
        patch.bbox = state.lastBbox;
        patch.layers = store.get('artifacts').layers;
        patch.colour_by = store.get('legend').colourBy;
        patch.filters = store.get('filters').expr;
      }
      const region = store.get('region');
      if (region !== lastRegion && (region === null || region.status !== 'loading')) {
        lastRegion = region;
        patch.region = region
          ? {
              shape: region.shape,
              status: region.status,
              visible: region.visible,
              matched: region.matched,
              served: region.served,
              depth: region.depth,
              tiles: region.tiles,
              refusal: region.refusal
            }
          : null;
      }
      const selection = store.get('selection');
      if (selection !== lastSelection) {
        lastSelection = selection;
        patch.selected = selection.item ? idString(selection.item.id) : null;
        patch.selected_artifact = selection.artifact ? idString(selection.artifact.id) : null;
      }
      if (Object.keys(patch).length) syncUp(patch);
    });
  };

  const rebuild = () => {
    state.unsubscribe?.();
    state.unsubscribe = null;
    state.store?.dispose();
    state.store = buildStore(model, supplier, storeFactory);
    if (state.store) {
      follow(state.store);
      applyControls(state.store);
    }
    for (const el of state.views) el.store = state.store;
  };

  // Down-sync: what the kernel set, applied to the store; ignored while an up-sync is what moved it.
  const applyControls = (store: Store) => {
    const layers = model.get('layers');
    if (Array.isArray(layers) && layers.length) store.setLayers(layers.map(String));
    const colourBy = model.get('colour_by');
    if (typeof colourBy === 'string' && colourBy) store.setColourBy(colourBy);
    const filters = model.get('filters');
    if (filters !== null && filters !== undefined) applyFilters(store, filters as FilterExpr);
  };
  const applyFilters = (store: Store, expr: FilterExpr | null) => {
    const meta = store.get('meta');
    if (!meta) {
      // Before `/v1/meta` there is no operand list to check against; the store re-applies its own
      // draft at meta, and the kernel's expression is applied then through the `meta` change below.
      return;
    }
    try {
      store.setFilters(draftOf(expr, meta.filterOperands));
    } catch (e) {
      report('filters', e);
    }
  };
  model.on('change:url', () => rebuild());
  model.on('change:view', () => rebuild());
  model.on('change:layers', () => {
    if (state.syncingUp || !state.store) return;
    const layers = model.get('layers');
    if (Array.isArray(layers)) state.store.setLayers(layers.map(String));
  });
  model.on('change:colour_by', () => {
    if (state.syncingUp || !state.store) return;
    const c = model.get('colour_by');
    state.store.setColourBy(typeof c === 'string' && c ? c : null);
  });
  model.on('change:filters', () => {
    if (state.syncingUp || !state.store) return;
    applyFilters(state.store, (model.get('filters') as FilterExpr | null) ?? null);
  });
  model.on('change:bbox', () => {
    if (state.syncingUp) return;
    const bbox = model.get('bbox');
    if (!Array.isArray(bbox) || bbox.length !== 4 || sameBbox(bbox as number[], state.lastBbox)) return;
    fit(bbox as [number, number, number, number]);
  });
  const fit = (bbox: [number, number, number, number]) => {
    let done = false;
    for (const el of state.views) done = (el.map?.fitBbox(bbox) ?? false) || done;
    state.pendingFit = done ? null : bbox;
  };
  // A filter expression set before meta arrived is applied once the operand list exists.
  let metaSeen = false;
  const onMeta = () => {
    const store = state.store;
    if (!store || metaSeen || !store.get('meta')) return;
    metaSeen = true;
    const filters = model.get('filters');
    if (filters !== null && filters !== undefined) applyFilters(store, filters as FilterExpr);
  };

  rebuild();
  if (state.store) {
    const store = state.store;
    const stop = store.subscribe(() => {
      onMeta();
      if (metaSeen) stop();
    });
  }
  model.on('destroy', () => state.dispose());
  return () => state.dispose();
}

export function render({model, el, signal}: {model: WidgetModel; el: HTMLElement; signal?: AbortSignal}): () => void {
  let state = states.get(model);
  if (!state) {
    // An anywidget without `initialize` (older than 0.9): set up on first render instead.
    initialize({model});
    state = states.get(model)!;
  }
  const explorer = document.createElement('tessera-explorer') as TesseraExplorer;
  explorer.layout = (model.get('layout') as 'docked' | 'overlay') || 'docked';
  const height = model.get('height');
  explorer.style.setProperty('--tessera-explorer-height', typeof height === 'number' ? `${height}px` : String(height || '480px'));
  explorer.store = state.store;
  const onView = (e: Event) => {
    state!.lastBbox = (e as CustomEvent<{bbox: [number, number, number, number]}>).detail.bbox;
  };
  explorer.addEventListener('tessera-viewchange', onView);
  const onHeight = () => {
    const h = model.get('height');
    explorer.style.setProperty('--tessera-explorer-height', typeof h === 'number' ? `${h}px` : String(h || '480px'));
  };
  model.on('change:height', onHeight);
  el.append(explorer);
  state.views.add(explorer);
  if (state.pendingFit) {
    const pending = state.pendingFit;
    void explorer.updateComplete.then(() => {
      if (state!.pendingFit === pending && explorer.map?.fitBbox(pending)) state!.pendingFit = null;
    });
  }
  const cleanup = () => {
    state!.views.delete(explorer);
    model.off('change:height', onHeight);
    explorer.removeEventListener('tessera-viewchange', onView);
    explorer.remove();
  };
  signal?.addEventListener('abort', cleanup, {once: true});
  return cleanup;
}

export default {initialize, render};
