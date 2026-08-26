import {NO_COUNT, NO_MASKED, servedLineage, SessionArtifactTable, type Projections, type ProjectionName, type Store, type StatusProjection} from '@tesseradb/client';

/**
 * A store with no network and no driver: projections a test sets directly, and the subscription
 * surface the elements read. Every verb is a spy.
 */
export type FakeStore = Store & {
  set<K extends ProjectionName>(name: K, value: Projections[K]): void;
  calls: {name: string; args: unknown[]}[];
};

export function fakeStore(overrides: Partial<Projections> = {}): FakeStore {
  const projections: Projections = {
    meta: null,
    status: {status: 'idle', sessionWarm: false, refusal: null, stale: false, expired: false, retrying: false},
    view: {composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, table: new SessionArtifactTable(), servedOrdinals: new Set(), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, values: {}, valueErrors: {}},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, lastPlan: null},
    ...overrides
  };
  const all = new Set<() => void>();
  const perName = new Map<ProjectionName, Set<() => void>>();
  const calls: {name: string; args: unknown[]}[] = [];
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
    loadFilterValues: async (...args: unknown[]) => {
      calls.push({name: 'loadFilterValues', args});
    },
    setLayers: spy('setLayers'),
    setColourBy: spy('setColourBy'),
    setPalette: spy('setPalette'),
    setBudget: spy('setBudget'),
    pick: async (...args: unknown[]) => {
      calls.push({name: 'pick', args});
    },
    openArtifact: async (...args: unknown[]) => {
      calls.push({name: 'openArtifact', args});
    },
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
