// A static harness for the highlight work's element screenshots: the built components against a
// hand-made store, so the boards can be compared with what actually renders. Untracked.
import {NO_COUNT, NO_MASKED, SessionArtifactTable, servedLineage, type BrowsePage, type Projections, type ProjectionName, type Store} from '@tesseradb/client';
import '@tesseradb/components';

const layer = (name: string, kind: string, computedContent: string[], title: string) =>
  ({name, title, views: ['s0'], hierarchy: {kind, pruneChildren: false}, levels: [], computedContent,
    shape: computedContent.includes('hull') ? 'derived' : null, suppliedContent: ['name'], depsOn: [], version: 1}) as never;

const meta = {
  apiVersion: 1, idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
  groups: [], declaredScalars: [],
  layers: [layer('clusters/kmeans', 'flat', ['centroid', 'box', 'hull'], 'k-means clusters'),
           layer('topics/ctfidf', 'flat', ['centroid'], 'topics'),
           layer('mesh/descriptors', 'dag', [], 'MeSH descriptors')],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144, maxBrowseRows: 200},
  maxTilesPerRequest: 4096,
  filterOperands: [{column: 'title', family: 'text', operands: ['match', 'phrase']},
                   {column: 'archive', family: 'category', operands: ['in']}]
} as never;

const row = (id: bigint, name: string, masked: bigint, matched: bigint, parents: bigint[] = []) =>
  ({tesseraId: id, key: `D${id}`, name, maskedCount: masked, matchedCount: matched, rung: 0, parentIds: parents});

const pages: Record<string, BrowsePage> = {
  roots: {artifacts: [row(1n, 'Neoplasms', 4_812_004n, 21_309n), row(2n, 'Anatomy', 9_115_442n, 44_002n), row(3n, 'Chemicals and Drugs', 8_240_119n, 39_551n)], parents: [], next: 'p2'},
  'p:1': {artifacts: [row(11n, 'Neoplasms by Site', 1_204_881n, 8_142n), row(12n, 'Neoplasms by Histologic Type', 962_110n, 4_508n)], parents: [], next: null},
  'p:11': {artifacts: [row(111n, 'Breast Neoplasms', 288_412n, 3_004n, [11n, 2n]), row(112n, 'Lung Neoplasms', 251_770n, 2_118n), row(113n, 'Digestive System Neoplasms', 214_005n, 1_442n)], parents: [], next: null}
};

const projections = {
  meta,
  status: {status: 'shown', sessionWarm: true, refusal: null, stale: false, expired: false, retrying: false},
  view: {id: 's0', composition: null, depth: 9, visible: {value: 181_900, exact: true}, matched: {value: 12_465, exact: true},
         highlighted: {value: 3_204, exact: true}, highlighting: true, served: {shown: 4_812, total: 181_900, exact: true}, provisional: 0},
  marks: {bands: [], standIn: [], count: NO_COUNT},
  tiles: {tiles: []},
  artifacts: {layer: 'clusters/kmeans', layers: ['clusters/kmeans'], served: [], colourServed: [], lineage: servedLineage([]), status: 'shown', refusal: null,
              version: 1, held: 0, table: new SessionArtifactTable(), servedOrdinals: new Set<number>(), shapes: new Map(), colours: new Map(),
              palette: 'positional', coverage: {current: 0, stale: 0}},
  selection: {item: null, itemRefusal: null,
              artifact: {id: 111n, detail: {layer: 'mesh/descriptors', key: 'D001943', maskedCount: 288_412n, centroid: null, box: null, shape: null}},
              artifactRefusal: null},
  region: null,
  filters: {draft: {title: {family: 'text', query: 'quantum entanglement', mode: 'all', verb: 'filter'},
                    archive: {family: 'category', keys: ['quant-ph'], verb: 'highlight'}},
            expr: {title: {match: 'quantum entanglement'}},
            highlight: {archive: {in: ['quant-ph']}},
            members: [{layer: 'mesh/descriptors', artifact: 111n, outside: false, verb: 'highlight'}],
            suggestions: {archive: {q: '', more: false, values: [{code: 1, key: 'quant-ph', title: null, match: {field: 'key', start: 0, len: 0}}]}},
            suggestErrors: {}},
  legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
  replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null}
} as unknown as Projections;

const listeners = new Set<() => void>();
const store = {
  projections,
  get: <K extends ProjectionName>(name: K) => projections[name],
  subscribe: (a: unknown, b?: unknown) => {
    const fn = (typeof a === 'function' ? a : b) as () => void;
    listeners.add(fn);
    return () => listeners.delete(fn);
  },
  browse: async (req: {parent?: bigint; q?: string; cursor?: string}) => {
    const key = req.q !== undefined ? `q:${req.q}` : req.parent !== undefined ? `p:${req.parent}` : 'roots';
    return pages[key] ?? {artifacts: [], parents: [], next: null};
  },
  setFilters: () => {},
  setMembers: () => {},
  setLayers: () => {},
  openArtifact: async () => {},
  suggest: () => {},
  requestFilters: () => ({title: {match: 'quantum entanglement'}}),
  dataXY: (x: number, y: number) => [x, y] as [number, number],
  extentOf: () => null,
  frame: () => null
} as unknown as Store;

for (const id of ['strip', 'filters', 'tree', 'card', 'layers']) {
  (document.getElementById(id) as unknown as {store: Store}).store = store;
}
// Open the walk two levels down, so the screenshot shows children and *also under*.
setTimeout(() => {
  const tree = document.getElementById('tree')!;
  const click = (i: number) => (tree.shadowRoot!.querySelectorAll('[part="expander"]')[i] as HTMLButtonElement | undefined)?.click();
  click(0);
  setTimeout(() => { click(1); }, 200);
}, 400);
