import {NO_COUNT, NO_MASKED, withMembers, type AggregateEntry, type AggregateTable, type BrowsePage, type Projections, type ProjectionName, type Quantisation, type Store, type StatusProjection, type DeclaredScalar, type Meta} from '@tesseradb/client';
import {regionOperand, servedLineage, SessionArtifactTable, withRegion} from '@tesseradb/client/internal';

/**
 * A store with no network and no driver: projections a test sets directly, and the subscription
 * surface the elements read. Every verb is a spy.
 */
export type FakeStore = Store & {
  set<K extends ProjectionName>(name: K, value: Projections[K]): void;
  /** Move the frame `frame()` answers with, as a switch to a view with another quantisation does. */
  setFrame(q: Quantisation): void;
  /** Script one browse answer: `roots`, `roots:<cursor>`, `p:<id>`, `p:<id>:<cursor>`, `q:<text>`. */
  setBrowse(key: string, page: BrowsePage): void;
  calls: {name: string; args: unknown[]}[];
};

/** A declared column that is indexed, not rendered and read from the record, unless `over` says. */
export function scalar(name: string, arrowType: DeclaredScalar['arrowType'], over: Partial<DeclaredScalar> = {}): DeclaredScalar {
  return {name, arrowType, category: null, render: false, index: true, unique: false, analyser: null, homes: ['record'], ...over};
}

/** A deployment of one plain view `s0` over the unit square with nothing declared, and whichever fields `over` names. */
export function meta(over: Partial<Meta> = {}): Meta {
  return {
    apiVersion: 1,
    bundleFormat: 1,
    views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
    groups: [],
    declaredScalars: [],
    scopedScalars: [],
    layers: [],
    selection: {
      kMin: 1,
      kMaxMarks: 500,
      maxK: 5000,
      thetaTargetMarks: 10,
      maxUnderlayOffset: 0,
      maxCategoryValues: 1000,
      maxRegionVertices: 10_000,
      maxRegionCells: 262_144,
      maxBrowseRows: 200,
      maxShapeVertices: 50_000,
      maxSuggestions: 20,
      maxSuggestionWalk: 100_000,
      maxSuggestSetEntities: 10_000_000,
      maxPageRows: 65_536,
      maxPageBytes: 16_777_216,
      maxAggregateGroupings: 16,
      maxAggregateTop: 1000,
      maxAggregateNamed: 1000,
      maxAggregateCells: 1_048_576
    },
    maxTilesPerRequest: 4096,
    filterOperands: [],
    ...over
  };
}

export function fakeStore(overrides: Partial<Projections> = {}): FakeStore {
  const projections: Projections = {
    meta: null,
    status: {status: 'idle', sessionWarm: false, refusal: null, stale: false, expired: false, retrying: false},
    view: {id: '', composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], colourServed: [], attached: new Map(), lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table: new SessionArtifactTable(), servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {filter: {}, highlight: {}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
    legend: {ranks: {}, domains: {}, samples: {}, missing: {}, categories: {}, categoryErrors: {}, colourBy: null, sizeBy: null},
    replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null},
    aggregates: new Map(),
    ...overrides
  };
  let held: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};
  const all = new Set<() => void>();
  const perName = new Map<ProjectionName, Set<() => void>>();
  const calls: {name: string; args: unknown[]}[] = [];
  const browsePages = new Map<string, BrowsePage>();
  const spy =
    (name: string) =>
    (...args: unknown[]) => {
      calls.push({name, args});
      return undefined as never;
    };
  const store: FakeStore = {
    projections,
    calls,
    get: (name) => projections[name],
    set(name, value) {
      projections[name] = value;
      perName.get(name)?.forEach((fn) => fn());
      all.forEach((fn) => fn());
    },
    subscribe(nameOrFn: ProjectionName | (() => void), fn?: (value: never) => void): () => void {
      if (typeof nameOrFn === 'function') {
        all.add(nameOrFn);
        return () => all.delete(nameOrFn);
      }
      const set = perName.get(nameOrFn) ?? new Set();
      const wrapped = () => (fn as (v: unknown) => void)(projections[nameOrFn]);
      set.add(wrapped);
      perName.set(nameOrFn, set);
      return () => set.delete(wrapped);
    },
    setView: spy('setView'),
    setFilters: spy('setFilters'),
    setMembers: spy('setMembers'),
    setAggregate: spy('setAggregate'),
    // Answered from `browsePages`, which a test sets: keyed by the form the request took, so a
    // walk can be scripted without a network. Every call is still recorded as `browse`.
    browse: async (req: {parent?: bigint; q?: string; cursor?: string}) => {
      calls.push({name: 'browse', args: [req]});
      const key = req.q !== undefined ? `q:${req.q}` : req.parent !== undefined ? `p:${req.parent}${req.cursor ? `:${req.cursor}` : ''}` : `roots${req.cursor ? `:${req.cursor}` : ''}`;
      return browsePages.get(key) ?? {artifacts: [], parents: [], next: null};
    },
    suggest: spy('suggest'),
    forgetSuggestions: spy('forgetSuggestions'),
    setLayers: spy('setLayers'),
    setColourBy: spy('setColourBy'),
    setSizeBy: spy('setSizeBy'),
    setPalette: spy('setPalette'),
    setBudget: spy('setBudget'),
    setCurrentView: spy('setCurrentView'),
    // The unit square, so a component's data↔world conversion is the identity here and a test
    // asserting on world coordinates is asserting on what it wrote.
    frame: () => held,
    setFrame(q: Quantisation) {
      held = q;
    },
    setBrowse(key: string, page: BrowsePage) {
      browsePages.set(key, page);
    },
    // The composed request, from the projections a test sets, through the client's own
    // composition: the filter-position expression, the clauses in that position, the region.
    requestFilters: () => {
      const {expr, members} = projections.filters;
      const region = projections.region;
      return withRegion(withMembers(expr, members, 'filter'), region ? regionOperand(region.shape) : null, region?.shape.outside ?? false);
    },
    pick: async (...args: unknown[]) => {
      calls.push({name: 'pick', args});
    },
    describe: async (...args: unknown[]) => {
      calls.push({name: 'describe', args});
      return null;
    },
    openArtifact: async (...args: unknown[]) => {
      calls.push({name: 'openArtifact', args});
    },
    needShape: spy('needShape'),
    clearSelection: spy('clearSelection'),
    setScheme: spy('setScheme'),
    select: spy('select'),
    extentOf: () => null,
    dataXY: (x, y) => [x, y],
    clear: spy('clear'),
    refresh: spy('refresh'),
    dispose: spy('dispose')
  };
  return store;
}

export function status(over: Partial<StatusProjection>): StatusProjection {
  return {status: 'shown', sessionWarm: true, refusal: null, stale: false, expired: false, retrying: false, ...over};
}

/** Mount markup and wait for every Lit update inside it. */
export async function mount(markup: string): Promise<HTMLElement> {
  const host = document.createElement('div');
  host.innerHTML = markup;
  document.body.append(host);
  await settle(host);
  return host;
}

export async function settle(root: ParentNode): Promise<void> {
  for (let i = 0; i < 4; i++) {
    await Promise.resolve();
    for (const el of root.querySelectorAll('*')) {
      const u = (el as unknown as {updateComplete?: Promise<unknown>}).updateComplete;
      if (u) await u;
      const shadow = (el as HTMLElement).shadowRoot;
      if (shadow) await settle(shadow);
    }
  }
}

/** A shadow-piercing query, as the harness's locators are. */
export function deep(root: ParentNode, selector: string): Element | null {
  const direct = root.querySelector(selector);
  if (direct) return direct;
  for (const el of root.querySelectorAll('*')) {
    const shadow = (el as HTMLElement).shadowRoot;
    if (!shadow) continue;
    const found = deep(shadow, selector);
    if (found) return found;
  }
  return null;
}

export function deepAll(root: ParentNode, selector: string): Element[] {
  const out: Element[] = [...root.querySelectorAll(selector)];
  for (const el of root.querySelectorAll('*')) {
    const shadow = (el as HTMLElement).shadowRoot;
    if (shadow) out.push(...deepAll(shadow, selector));
  }
  return out;
}

/** The text of an element including what its descendants' shadow roots render. */
export function deepText(el: Element | null): string {
  if (!el) return '';
  let out = '';
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) out += node.textContent ?? '';
    const shadow = (node as HTMLElement).shadowRoot;
    if (shadow) for (const child of shadow.childNodes) walk(child);
    for (const child of node.childNodes) walk(child);
  };
  walk(el);
  return out;
}

/** One row of an aggregate table as a test writes it; `group` defaults to `listed`. */
export type AggregateRow = {group?: 'listed' | 'rest' | 'none'; key?: string | bigint | null; title?: string | null; cell?: bigint; count: number};

/**
 * An answered aggregate, one table per entry of `tables`, as the store publishes it. The rows are a
 * stand-in for an Arrow table with the columns an aggregate carries, read by name.
 */
export function aggregateEntry(tables: {rows: AggregateRow[]; groups?: number | null; total?: number}[], view = 's0'): AggregateEntry {
  const table = (rows: AggregateRow[]) => {
    const column = (read: (r: AggregateRow) => unknown) => ({get: (i: number) => read(rows[i]!), toArray: () => rows.map(read)});
    const columns: Record<string, {get(i: number): unknown; toArray(): unknown[]}> = {
      group: column((r) => r.group ?? 'listed'),
      key: column((r) => r.key ?? null),
      title: column((r) => r.title ?? null),
      count: column((r) => BigInt(r.count))
    };
    if (rows.some((r) => r.cell !== undefined)) columns.cell = column((r) => r.cell);
    return {numRows: rows.length, getChild: (name: string) => columns[name] ?? null} as unknown as AggregateTable['rows'];
  };
  return {
    status: 'shown',
    view,
    refusal: null,
    result: {
      tables: tables.map((t, grouping) => ({grouping, total: t.total ?? t.rows.reduce((n, r) => n + r.count, 0), referenceTotal: null, groups: t.groups ?? null, rows: table(t.rows)})),
      region: null,
      recomposed: false,
      identityKey: 'ik',
      next: null
    }
  };
}

/** The specs registered with `store.setAggregate` and not since dropped, by id. */
export function registered(store: FakeStore): Map<string, unknown> {
  const out = new Map<string, unknown>();
  for (const {name, args} of store.calls) {
    if (name !== 'setAggregate') continue;
    const [id, spec] = args as [string, unknown];
    if (spec === null) out.delete(id);
    else out.set(id, spec);
  }
  return out;
}

/** Answer every registered aggregate whose id starts with `prefix` with `entry`. */
export function answerAggregate(store: FakeStore, prefix: string, entry: AggregateEntry): void {
  const held = new Map(store.get('aggregates'));
  for (const id of registered(store).keys()) if (id.startsWith(prefix)) held.set(id, entry);
  store.set('aggregates', held);
}
