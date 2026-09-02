import {NO_COUNT, NO_MASKED, servedLineage, SessionArtifactTable, type BrowsePage, type Projections, type ProjectionName, type Quantisation, type Store, type StatusProjection} from '@tesseradb/client';

/**
 * A store with no network and no driver: projections a test sets directly, and the subscription
 * surface the elements read. Every verb is a spy.
 */
export type FakeStore = Store & {
  set<K extends ProjectionName>(name: K, value: Projections[K]): void;
  /**
   * Move the frame `frame()` answers with — how a test drives a switch across frames
   * (`view-switching.md` §4), which the store makes by pointing at another view's quantisation.
   */
  setFrame(q: Quantisation): void;
  /** Script one browse answer: `roots`, `roots:<cursor>`, `p:<id>`, `p:<id>:<cursor>`, `q:<text>`. */
  setBrowse(key: string, page: BrowsePage): void;
  calls: {name: string; args: unknown[]}[];
};

export function fakeStore(overrides: Partial<Projections> = {}): FakeStore {
  const projections: Projections = {
    meta: null,
    status: {status: 'idle', sessionWarm: false, refusal: null, stale: false, expired: false, retrying: false},
    view: {id: '', composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table: new SessionArtifactTable(), servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null},
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
    // Answered from `browsePages`, which a test sets: keyed by the form the request took, so a
    // walk can be scripted without a network. Every call is still recorded as `browse`.
    browse: async (req: {parent?: bigint; q?: string; cursor?: string}) => {
      calls.push({name: 'browse', args: [req]});
      const key = req.q !== undefined ? `q:${req.q}` : req.parent !== undefined ? `p:${req.parent}${req.cursor ? `:${req.cursor}` : ''}` : `roots${req.cursor ? `:${req.cursor}` : ''}`;
      return browsePages.get(key) ?? {artifacts: [], parents: [], next: null};
    },
    suggest: spy('suggest'),
    setLayers: spy('setLayers'),
    setColourBy: spy('setColourBy'),
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
    // The composed request, as the store composes it: the filter-position leaves, the clauses in
    // that position, and the drawn region's leaf. A test sets `region` and this follows, which is
    // the drift the panel's question was hashing around.
    requestFilters: () => {
      const {expr, members} = projections.filters;
      const leaves: unknown[] = [];
      if (expr) leaves.push(expr);
      for (const c of members) {
        if (c.verb !== 'filter') continue;
        const leaf = {member_of: {layer: c.layer, artifact: c.artifact.toString()}};
        leaves.push(c.outside ? {none_of: [leaf]} : leaf);
      }
      if (projections.region) leaves.push({region: {bbox: [0, 0, 1, 1]}});
      return (leaves.length === 0 ? null : leaves.length === 1 ? leaves[0] : {all_of: leaves}) as never;
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
  } as FakeStore;
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
