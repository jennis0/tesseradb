import {
  CLUSTER_PREFIX,
  colourLayers,
  createStore,
  emptyDraft,
  textQueryOf,
  type ColumnDraft,
  type FilterDraft,
  type FilterExpr,
  type FilterOperandSet,
  type Store,
  type TokenSupplier
} from '@tesseradb/client';
import type {Sizing} from '@tesseradb/deck';
import {idString} from './base.js';
import {setSizing, sizingOf} from './colouring.js';
import type {TesseraExplorer} from './explorer.js';
import './explorer.js';

/**
 * The notebook widget's JavaScript half: what anywidget evaluates as `_esm`, exported from the
 * single-file bundle. The kernel half is `clients/py/tesseradb/widget.py`.
 *
 * The token is not model state: no traitlet carries it, so saving widget state, `nbconvert
 * --execute` or papermill cannot write it into a notebook. `initialize` sends `ready` once per
 * model and the kernel answers with the token as a custom message. Later calls to the token
 * supplier (renewal before expiry, or after a refusal that ends the session) send `reauthorise`.
 * A page reload makes a new model, which sends `ready` again.
 *
 * Only controls and selections cross the kernel boundary. `url`, `explorer_layout`, `height`,
 * `title_field` and `artifacts_per_tile` come down; `view`, `bbox`, `layers`, `colour_by`, `size_by`, `size_min`,
 * `size_max`, `size_scale` and `filters` go both ways;
 * `selected`, `selected_artifact` and `region` go up. Up-syncs happen at the settle (a new
 * composition shown, or a region's counts), not per frame. Ids cross as decimal strings, since a
 * `tessera_id` is a `u64`.
 *
 * The token is per model; the store and camera are per view, since a store has one view input.
 * Each view builds its own store over the shared supplier, which returns a held token without a
 * round trip. Up-syncs come from the active view, the one whose camera last moved; down-syncs go
 * to every view.
 *
 * Backbone fires `change:<key>` for a `model.set` made here as well as one from the kernel, so
 * each up-sync sets a flag the down-sync handlers check. Without it a `bbox` from Python would fit
 * the camera, the settle would report a box of a different aspect, and that would fit the camera
 * again, without end.
 */

/** The part of anywidget's `AnyModel` this module uses, typed here so a test can fake it. */
export type WidgetModel = {
  get(key: string): unknown;
  set(key: string, value: unknown): void;
  save_changes(): void;
  on(event: string, cb: (...args: unknown[]) => void): void;
  off(event: string, cb?: (...args: unknown[]) => void): void;
  send(content: unknown, callbacks?: unknown, buffers?: ArrayBuffer[]): void;
};

/** What the kernel sends the page. `expires_at` is seconds since the epoch, as `/v1/login` and `/session/authorise` report it, or `null` when unknown. */
export type KernelMessage =
  | {type: 'token'; token: string; expires_at: number | null}
  | {type: 'refused'; detail: string};

/** What the page sends the kernel. */
export type PageMessage = {type: 'ready'} | {type: 'reauthorise'} | {type: 'error'; what: string; detail: string};

type ViewState = {
  explorer: TesseraExplorer;
  store: Store | null;
  unsubscribe: (() => void) | null;
  /** The box this view's camera last reported, in data coordinates, which its settle syncs up. */
  lastBbox: [number, number, number, number] | null;
};

type ModelState = {
  supplier: TokenSupplier;
  storeFactory: StoreFactory;
  views: Map<TesseraExplorer, ViewState>;
  /** The view whose settles sync up: the last one mounted or moved. */
  active: ViewState | null;
  /** A `bbox` the kernel set before any map could fit it (no view, or no meta yet); fitted at the first chance. */
  pendingFit: [number, number, number, number] | null;
  syncingUp: boolean;
  dispose(): void;
};

const states = new WeakMap<object, ModelState>();

/** For a test: the state behind a model, once `initialize` has run. */
export function stateOf(model: object): {stores: (Store | null)[]; views: number} | null {
  const s = states.get(model);
  return s ? {stores: [...s.views.values()].map((v) => v.store), views: s.views.size} : null;
}

/**
 * The token supplier over the comm, shared by every view of the model. A held token not about to
 * expire is returned without a round trip. Otherwise one request is outstanding at a time
 * (`ready` first, then `reauthorise`), settled by the next `token` or `refused` message, and
 * concurrent calls share it. A token with `expires_at: null` is not renewed early; when the
 * server refuses it the store reports `expired`.
 */
function tokenSupplier(model: WidgetModel, onMessage: (cb: (msg: KernelMessage) => void) => void): TokenSupplier {
  type Got = {token: string; expiresAt: number};
  let outstanding: {resolve(v: Got): void; reject(e: Error): void} | null = null;
  let asked = false;
  let held: Got | null = null;
  onMessage((msg) => {
    if (!outstanding) return;
    const o = outstanding;
    outstanding = null;
    if (msg.type === 'token') {
      held = {token: msg.token, expiresAt: msg.expires_at === null ? Number.POSITIVE_INFINITY : msg.expires_at};
      o.resolve(held);
    } else if (msg.type === 'refused') {
      o.reject(new Error(msg.detail));
    }
  });
  let inflight: Promise<Got> | null = null;
  return () => {
    if (held && (held.expiresAt === Number.POSITIVE_INFINITY || Date.now() / 1000 < held.expiresAt - 60)) return Promise.resolve(held);
    if (inflight) return inflight;
    inflight = new Promise<Got>((resolve, reject) => {
      outstanding = {resolve, reject};
      model.send(asked ? {type: 'reauthorise'} : {type: 'ready'});
      asked = true;
    }).finally(() => {
      inflight = null;
    });
    return inflight;
  };
}

/** The one column every leaf under `expr` names, or `null` where they name several or none. */
function columnOf(expr: FilterExpr): string | null {
  const keys = Object.keys(expr);
  if (keys.length !== 1) return null;
  const key = keys[0]!;
  if (key === 'none_of') return null;
  if (key !== 'all_of' && key !== 'any_of') return key;
  const inner = (expr as Record<string, FilterExpr[]>)[key]!;
  const columns = new Set(inner.map(columnOf));
  return columns.size === 1 && !columns.has(null) ? [...columns][0]! : null;
}

/**
 * The store's draft for the `filters` traitlet's expression, which is in the wire's form, with every
 * control in the `filter` position and none in `highlight`. This inverts `composeFilters`: the
 * expression is an `all_of` over the columns, or one column's expression alone, and each column
 * holds one leaf, except a text column, which may hold any expression on itself. A text column's
 * expression becomes the query that writes it where there is one, and is otherwise kept as sent
 * and sent back as it is. Anything else (`none_of`, an `any_of` across columns, two leaves on
 * another column, an unknown column, an operator the column's family cannot hold) throws with a
 * reason, which goes to the kernel as an `error` message. `null` is no filter.
 */
export function draftOf(expr: FilterExpr | null, operands: FilterOperandSet[]): FilterDraft {
  const draft = emptyDraft(operands);
  if (expr === null) return draft;
  const top: FilterExpr[] = columnOf(expr) === null && 'all_of' in expr && Array.isArray(expr.all_of) ? (expr.all_of as FilterExpr[]) : [expr];
  const byColumn = new Map<string, FilterExpr[]>();
  for (const part of top) {
    const column = columnOf(part);
    if (column === null) throw new Error(`each part of a filter set from the widget names one column; got ${JSON.stringify(part)}`);
    byColumn.set(column, [...(byColumn.get(column) ?? []), part]);
  }
  for (const [column, parts] of byColumn) {
    const control = draft.filter[column];
    if (!control) throw new Error(`${column} is not a filterable column of this view`);
    if (control.family === 'text') {
      const held = parts.length === 1 ? parts[0]! : {all_of: parts};
      const query = textQueryOf(column, held, control.phrase);
      draft.filter[column] = query === null ? {...control, expr: held} : {...control, query};
      continue;
    }
    if (parts.length !== 1) throw new Error(`two leaves on ${column}; the widget holds one per column`);
    const leaf = parts[0]!;
    if (Object.keys(leaf)[0] !== column) throw new Error(`${column}: only a text column holds a combination of leaves`);
    draft.filter[column] = controlOf(column, control, (leaf as Record<string, unknown>)[column] as Record<string, unknown>);
  }
  return draft;
}

/** One leaf back into a control of the family `control` has, for a family other than text. */
function controlOf(column: string, control: Exclude<ColumnDraft, {family: 'text'}>, op: Record<string, unknown>): ColumnDraft {
  const names = Object.keys(op);
  if (names.length !== 1) throw new Error(`${column}: an operator has exactly one key; got ${names.join(', ')}`);
  const name = names[0]!;
  const value = op[name];
  const bad = () => new Error(`${column}: a ${control.family} column cannot hold ${name}`);
  switch (control.family) {
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

function sameBbox(a: number[] | null, b: number[] | null): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  return a.every((v, i) => v === b[i]);
}

/** How a store is built: `createStore`, unless a test injects one. */
export type StoreFactory = (options: {viewerUrl: string; authorise: TokenSupplier; view?: string; artifacts?: {perTile: number}}) => Store;

function buildStore(model: WidgetModel, state: ModelState): Store | null {
  const url = model.get('url');
  if (typeof url !== 'string' || !url) return null;
  const view = model.get('view');
  const perTile = model.get('artifacts_per_tile');
  return state.storeFactory({
    viewerUrl: url,
    authorise: state.supplier,
    ...(typeof view === 'string' && view ? {view} : {}),
    ...(typeof perTile === 'number' ? {artifacts: {perTile}} : {})
  });
}

function heightOf(model: WidgetModel): string {
  const h = model.get('height');
  return typeof h === 'number' ? `${h}px` : String(h || '480px');
}

export function initialize({model, storeFactory = createStore}: {model: WidgetModel; storeFactory?: StoreFactory}): () => void {
  const listeners: ((msg: KernelMessage) => void)[] = [];
  const onCustom = (...args: unknown[]) => {
    const msg = args[0] as KernelMessage;
    for (const l of listeners) l(msg);
  };
  model.on('msg:custom', onCustom);

  const state: ModelState = {
    supplier: tokenSupplier(model, (cb) => listeners.push(cb)),
    storeFactory,
    views: new Map(),
    active: null,
    pendingFit: null,
    syncingUp: false,
    dispose() {
      for (const v of state.views.values()) teardown(v);
      state.views.clear();
      state.active = null;
      model.off('msg:custom', onCustom);
    }
  };
  states.set(model, state);

  const report = (what: string, e: unknown) => {
    model.send({type: 'error', what, detail: e instanceof Error ? e.message : String(e)} satisfies PageMessage);
  };

  // Up-sync: one `save_changes` per settle or region answer.
  const syncUp = (patch: Record<string, unknown>) => {
    state.syncingUp = true;
    try {
      let dirty = false;
      for (const [k, v] of Object.entries(patch)) {
        if (JSON.stringify(model.get(k)) === JSON.stringify(v)) continue;
        model.set(k, v);
        dirty = true;
      }
      if (dirty) model.save_changes();
    } finally {
      state.syncingUp = false;
    }
  };

  const applyFilters = (store: Store, expr: FilterExpr | null) => {
    const meta = store.get('meta');
    // Before `/v1/meta` there is no operand list to check against; `follow` applies the kernel's
    // expression when meta arrives.
    if (!meta) return;
    try {
      // The traitlet is the `filters` expression, so the highlight is kept.
      store.setFilters({...draftOf(expr, meta.filterOperands), highlight: store.get('filters').draft.highlight});
    } catch (e) {
      report('filters', e);
    }
  };

  /**
   * A `cluster:<layer>` naming no layer this view can colour by is reported, as a bad filter is;
   * the store still takes it and draws the points uniform. Checked once `/v1/meta` is in hand.
   */
  const checkColour = (store: Store, colourBy: unknown) => {
    const meta = store.get('meta');
    if (!meta || typeof colourBy !== 'string' || !colourBy.startsWith(CLUSTER_PREFIX)) return;
    const offered = colourLayers(meta.layers).map((l) => l.name);
    const layer = colourBy.slice(CLUSTER_PREFIX.length);
    if (offered.includes(layer)) return;
    const instead = offered.length === 0 ? 'this view has no layer to colour by' : `use one of ${offered.map((n) => CLUSTER_PREFIX + n).join(', ')}`;
    report('colour_by', new Error(`${layer} is not a layer this view can colour by; ${instead}`));
  };

  /** The size range and scale the kernel holds, shared with every element over `store`; `null` leaves the choice standing. */
  const applySizing = (store: Store) => {
    const patch: Partial<Sizing> = {};
    const min = model.get('size_min');
    const max = model.get('size_max');
    const scale = model.get('size_scale');
    if (typeof min === 'number') patch.min = min;
    if (typeof max === 'number') patch.max = max;
    if (typeof scale === 'string') patch.scale = scale as Sizing['scale'];
    setSizing(store, patch);
  };

  /** The column the kernel sizes by, `null` for one size, with a sample kept under rank. */
  const applySizeBy = (store: Store) => {
    const sizeBy = model.get('size_by');
    store.setSizeBy(typeof sizeBy === 'string' && sizeBy ? sizeBy : null, {rank: sizingOf(store).scale === 'rank'});
  };

  // Down-sync: what the kernel holds, applied to one store (a new one, or one whose meta arrived).
  const applyControls = (store: Store) => {
    // `null` leaves the explorer's default; `[]` is none.
    const layers = model.get('layers');
    if (Array.isArray(layers)) store.setLayers(layers.map(String));
    const colourBy = model.get('colour_by');
    if (typeof colourBy === 'string' && colourBy) store.setColourBy(colourBy);
    applySizing(store);
    const sizeBy = model.get('size_by');
    if (typeof sizeBy === 'string' && sizeBy) applySizeBy(store);
    const filters = model.get('filters');
    if (filters !== null && filters !== undefined) applyFilters(store, filters as FilterExpr);
  };

  const fit = (bbox: [number, number, number, number]) => {
    let done = false;
    for (const v of state.views.values()) done = (v.explorer.map?.fitBbox(bbox) ?? false) || done;
    state.pendingFit = done ? null : bbox;
  };

  /** Follow one view's store: its settles and answers sync up while it is the active view. */
  const follow = (v: ViewState, store: Store) => {
    let lastComposition: object | null = null;
    let lastRegion: object | null = null;
    let lastSelection: object | null = null;
    let metaSeen = false;
    v.unsubscribe = store.subscribe(() => {
      if (!metaSeen && store.get('meta')) {
        metaSeen = true;
        const filters = model.get('filters');
        if (filters !== null && filters !== undefined) applyFilters(store, filters as FilterExpr);
        checkColour(store, model.get('colour_by'));
        if (state.pendingFit) fit(state.pendingFit);
      }
      if (state.active !== v) return;
      const status = store.get('status');
      const view = store.get('view');
      const patch: Record<string, unknown> = {};
      // The settle: a new composition presented as `shown` or `empty`. The control traits are read
      // from the store, so the kernel holds what the store applied, whichever side set it.
      const settled = (status.status === 'shown' || status.status === 'empty') && view.composition !== lastComposition;
      if (settled) {
        lastComposition = view.composition;
        patch.bbox = v.lastBbox;
        patch.view = view.id;
        patch.layers = store.get('artifacts').layers;
        patch.colour_by = store.get('legend').colourBy;
        const sizing = sizingOf(store);
        patch.size_by = store.get('legend').sizeBy;
        patch.size_min = sizing.min;
        patch.size_max = sizing.max;
        patch.size_scale = sizing.scale;
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
              verdict: region.verdict,
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

  const teardown = (v: ViewState) => {
    v.unsubscribe?.();
    v.unsubscribe = null;
    v.store?.dispose();
    v.store = null;
    v.explorer.store = null;
  };

  /** Give a view a store, at mount and again when `url` changes. */
  const equip = (v: ViewState) => {
    teardown(v);
    v.store = buildStore(model, state);
    if (v.store) {
      follow(v, v.store);
      applyControls(v.store);
    }
    v.explorer.store = v.store;
  };

  const rebuildAll = () => {
    for (const v of state.views.values()) equip(v);
  };
  model.on('change:url', rebuildAll);
  model.on('change:artifacts_per_tile', rebuildAll);
  // A view change calls `setCurrentView` on each store; a store holds several views, so stepping
  // through a roster rebuilds nothing. A `url` change rebuilds, since a `tessera_id` from one
  // bundle means nothing in another. A switch made in one view reaches only that view's store,
  // through the echo guard; one written from the kernel reaches every view.
  model.on('change:view', () => {
    if (state.syncingUp) return;
    const view = model.get('view');
    if (typeof view !== 'string' || !view) return;
    for (const v of state.views.values()) v.store?.setCurrentView(view);
  });
  model.on('change:layers', () => {
    if (state.syncingUp) return;
    const layers = model.get('layers');
    if (!Array.isArray(layers)) return;
    for (const v of state.views.values()) v.store?.setLayers(layers.map(String));
  });
  model.on('change:colour_by', () => {
    if (state.syncingUp) return;
    const c = model.get('colour_by');
    for (const v of state.views.values()) {
      if (!v.store) continue;
      v.store.setColourBy(typeof c === 'string' && c ? c : null);
      checkColour(v.store, c);
    }
  });
  model.on('change:size_by', () => {
    if (state.syncingUp) return;
    for (const v of state.views.values()) if (v.store) applySizeBy(v.store);
  });
  for (const key of ['size_min', 'size_max', 'size_scale']) {
    model.on(`change:${key}`, () => {
      if (state.syncingUp) return;
      for (const v of state.views.values()) if (v.store) applySizing(v.store);
    });
  }
  model.on('change:filters', () => {
    if (state.syncingUp) return;
    const expr = (model.get('filters') as FilterExpr | null) ?? null;
    for (const v of state.views.values()) if (v.store) applyFilters(v.store, expr);
  });
  model.on('change:bbox', () => {
    if (state.syncingUp) return;
    const bbox = model.get('bbox');
    if (!Array.isArray(bbox) || bbox.length !== 4 || sameBbox(bbox as number[], state.active?.lastBbox ?? null)) return;
    fit(bbox as [number, number, number, number]);
  });
  model.on('change:height', () => {
    for (const v of state.views.values()) v.explorer.style.setProperty('--tessera-explorer-height', heightOf(model));
  });
  model.on('change:title_field', () => {
    for (const v of state.views.values()) v.explorer.titleField = titleFieldOf(model);
  });
  model.on('destroy', () => state.dispose());

  // What `render` calls, kept on the state so a test can mount through it.
  mounts.set(state, (explorer) => {
    const v: ViewState = {explorer, store: null, unsubscribe: null, lastBbox: null};
    state.views.set(explorer, v);
    state.active = v;
    equip(v);
    return () => {
      state.views.delete(explorer);
      teardown(v);
      if (state.active === v) state.active = [...state.views.values()].at(-1) ?? null;
    };
  });
  return () => state.dispose();
}

/** The record field that titles a point, as the kernel set it; empty where it set none. */
function titleFieldOf(model: WidgetModel): string {
  const f = model.get('title_field');
  return typeof f === 'string' ? f : '';
}

const mounts = new WeakMap<ModelState, (explorer: TesseraExplorer) => () => void>();

export function render({model, el, signal}: {model: WidgetModel; el: HTMLElement; signal?: AbortSignal}): () => void {
  let state = states.get(model);
  if (!state) {
    // An anywidget without `initialize` (before 0.9): set up on first render.
    initialize({model});
    state = states.get(model)!;
  }
  const explorer = document.createElement('tessera-explorer') as TesseraExplorer;
  explorer.layout = (model.get('explorer_layout') as 'docked' | 'overlay') || 'docked';
  explorer.style.setProperty('--tessera-explorer-height', heightOf(model));
  explorer.titleField = titleFieldOf(model);
  el.append(explorer);
  const unmount = mounts.get(state)!(explorer);
  const v = state.views.get(explorer)!;
  const onView = (e: Event) => {
    v.lastBbox = (e as CustomEvent<{bbox: [number, number, number, number]}>).detail.bbox;
    state!.active = v;
  };
  explorer.addEventListener('tessera-viewchange', onView);
  if (state.pendingFit) {
    const pending = state.pendingFit;
    void explorer.updateComplete.then(() => {
      if (state!.pendingFit === pending && explorer.map?.fitBbox(pending)) state!.pendingFit = null;
    });
  }
  const cleanup = () => {
    explorer.removeEventListener('tessera-viewchange', onView);
    unmount();
    explorer.remove();
  };
  signal?.addEventListener('abort', cleanup, {once: true});
  return cleanup;
}

export default {initialize, render};
