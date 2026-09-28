import {GRID32, WORLD_SIZE, composeFilters, type Artifact, type ArtifactsProjection, type Band, type BrowseRow, type CategoryValue, type ColumnDraft, type FilterDraft, type FiltersProjection, type ItemDetail, type Layer, type LegendProjection, type MarksProjection, type Meta, type PaletteScheme, type Rgba, type SuggestValue, type TilesProjection, type ViewInfo, type ViewProjection} from '@tesseradb/client';
import {SessionArtifactTable, artifactColours, attachedTextOf, compose, mortonOfTile, servedLineage} from '@tesseradb/client/internal';
import {meta as baseMeta, scalar} from '../../components/test/fake-store.js';

/**
 * A made-up corpus of research papers, shaped as the store's projections: a category field, a year,
 * citation counts, titles, authors and abstracts, a two-level topic hierarchy with labels, a filter
 * layer of venues, and several thousand marks in clusters. Every figure is invented; the titles are
 * long and the counts large so the elements can be judged by them.
 */

/** A deterministic generator, so every load of the page draws the same marks. */
function random(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function gaussian(rand: () => number): number {
  const u = Math.max(rand(), 1e-9);
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * rand());
}

/** The arXiv-style subject field, a declared public vocabulary. Codes start at 1: 0 is no value. */
export const FIELDS: CategoryValue[] = [
  {code: 1, key: 'cs.LG', title: 'Machine Learning'},
  {code: 2, key: 'cs.CV', title: 'Computer Vision and Pattern Recognition'},
  {code: 3, key: 'cs.CL', title: 'Computation and Language'},
  {code: 4, key: 'stat.ML', title: 'Machine Learning (Statistics)'},
  {code: 5, key: 'q-bio.NC', title: 'Neurons and Cognition'},
  {code: 6, key: 'astro-ph.GA', title: 'Astrophysics of Galaxies'},
  {code: 7, key: 'hep-th', title: 'High Energy Physics – Theory'},
  {code: 8, key: 'cond-mat.str-el', title: 'Strongly Correlated Electrons'},
  {code: 9, key: 'math.PR', title: 'Probability'},
  {code: 10, key: 'econ.EM', title: 'Econometrics'},
  {code: 11, key: 'physics.soc-ph', title: 'Physics and Society'}
];

/** A longer vocabulary than the palette holds, for the legend's overflow. */
export const MANY_FIELDS: CategoryValue[] = [
  ...FIELDS,
  ...['cs.AI', 'cs.RO', 'cs.CR', 'cs.DB', 'cs.DS', 'math.AG', 'math.NT', 'nucl-th', 'gr-qc', 'quant-ph', 'eess.SP', 'q-fin.ST'].map((key, i) => ({code: 12 + i, key, title: null}))
];

export const ranksOf = (values: CategoryValue[]): Record<number, number> => Object.fromEntries(values.map((v, i) => [v.code, i]));

/** One cluster of papers: where it sits in the 512-unit world, how wide, and what it is about. */
type Cluster = {name: string | null; area: number; centre: [number, number]; spread: number; n: number; fields: number[]; labels: string};

const CLUSTERS: Cluster[] = [
  {name: 'Diffusion models and score-based generative modelling', area: 0, centre: [150, 140], spread: 26, n: 900, fields: [2, 1, 4], labels: 'denoising · score matching · guidance'},
  {name: 'Large language models: alignment, evaluation and the long tail of instruction following', area: 0, centre: [262, 108], spread: 30, n: 1100, fields: [3, 1], labels: 'RLHF · benchmarks · preference data'},
  {name: 'Graph neural networks', area: 0, centre: [112, 262], spread: 22, n: 650, fields: [1, 4, 9], labels: 'message passing · over-smoothing'},
  {name: 'Spiking networks and cortical coding', area: 0, centre: [214, 330], spread: 20, n: 420, fields: [5, 1], labels: 'spike timing · plasticity'},
  {name: 'Galaxy formation at high redshift', area: 1, centre: [386, 132], spread: 28, n: 760, fields: [6], labels: 'JWST · reionisation'},
  {name: 'Holography and quantum gravity', area: 1, centre: [420, 254], spread: 24, n: 540, fields: [7, 8], labels: 'AdS/CFT · entanglement'},
  {name: 'Causal inference with panel data', area: 2, centre: [340, 398], spread: 22, n: 480, fields: [10, 4], labels: 'difference in differences · synthetic control'},
  {name: 'Epidemic spreading on networks', area: 2, centre: [446, 424], spread: 18, n: 380, fields: [11, 9], labels: 'SIR · percolation'},
  {name: null, area: 2, centre: [70, 440], spread: 14, n: 160, fields: [9], labels: ''},
  {name: null, area: 0, centre: [318, 226], spread: 16, n: 300, fields: [1, 3], labels: 'attention · sparse transformers'}
];

const AREAS = ['Learning, vision and language', 'Physical sciences', 'Quantitative social science'];

/** `tessera_id`s look like the blinded 64-bit identifiers the server sends. */
const AREA_IDS = [7_302_118_455_901n, 7_302_118_471_266n, 7_302_118_503_417n];
const TOPIC_IDS = CLUSTERS.map((_, i) => 9_184_302_775_000n + BigInt(i) * 7_919n);
const LABEL_IDS = CLUSTERS.map((_, i) => 11_470_028_310_000n + BigInt(i) * 104_729n);
const VENUE_IDS = [5_551_000_001n, 5_551_000_002n, 5_551_000_003n, 5_551_000_004n, 5_551_000_005n, 5_551_000_006n, 5_551_000_007n];

/** How many items one served mark stands for, so the counts read as a corpus of tens of millions. */
const SCALE = 3_121;

const layer = (over: Partial<Layer> & {name: string}): Layer => ({
  title: null,
  views: ['umap'],
  membership: 'enumerated',
  hierarchy: {kind: 'flat', pruneChildren: false},
  levels: [],
  computedContent: ['centroid'],
  shape: null,
  suppliedContent: [],
  depsOn: [],
  version: 1,
  ...over
});

export const LAYERS: Layer[] = [
  layer({
    name: 'topics',
    title: 'Research topics',
    hierarchy: {kind: 'nested', pruneChildren: false},
    levels: [
      {level: 0, title: 'Area', zoom: null},
      {level: 1, title: 'Topic', zoom: null}
    ],
    computedContent: ['centroid', 'box', 'hull'],
    shape: 'derived',
    suppliedContent: ['name']
  }),
  layer({name: 'topic_labels', title: 'Topic keywords', computedContent: [], depsOn: ['topics'], suppliedContent: ['text']}),
  layer({name: 'research_institutions_by_country_and_funding_body', title: 'Institutions', shape: 'authored', suppliedContent: ['name']}),
  layer({name: 'venues', title: 'Publication venues', computedContent: [], hierarchy: {kind: 'nested', pruneChildren: false}, suppliedContent: ['name']})
];

const UNIT = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};
const view = (id: string, displayName: string, roster: ViewInfo['roster'] = null): ViewInfo => ({
  id,
  displayName,
  quantisation: UNIT,
  projection: 'none',
  worldAspect: null,
  tileScheme: null,
  tile: null,
  roster
});

const DECADES = [
  {key: '1990s', label: '1990 – 1999'},
  {key: '2000s', label: '2000 – 2009'},
  {key: '2010s', label: '2010 – 2019'},
  {key: '2020s', label: '2020 – 2026, including the preprints posted since the last snapshot'}
];

const VIEWS: ViewInfo[] = [
  view('umap', 'UMAP of abstracts'),
  view('tsne', 't-SNE of the citation graph'),
  view('specter', 'SPECTER2 embeddings, projected with PaCMAP at perplexity 30 and a fixed seed'),
  ...DECADES.map((d) => view(`decade:${d.key}`, d.key, {group: 'decade', key: d.key, metadata: {label: {type: 'text', value: d.label}}}))
];

export const META: Meta = baseMeta({
  views: VIEWS,
  groups: [{name: 'decade', title: 'By decade', membersOf: null, views: DECADES.map((d) => `decade:${d.key}`)}],
  declaredScalars: [
    scalar('field', 'u16', {category: {vocabulary: 'arxiv_fields', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']}),
    scalar('year', 'u16', {render: true, homes: ['rendered']}),
    scalar('citations', 'u32', {render: true, homes: ['rendered']}),
    scalar('title', 'utf8', {analyser: 'english'}),
    scalar('authors', 'utf8'),
    scalar('abstract', 'utf8', {analyser: 'english'}),
    scalar('published_at', 'timestamp_us'),
    scalar('doi', 'utf8', {unique: true})
  ],
  layers: LAYERS,
  filterOperands: [
    {column: 'field', family: 'category', operands: ['eq', 'in']},
    {column: 'title', family: 'text', operands: ['match', 'phrase']},
    {column: 'abstract', family: 'text', operands: ['match']},
    {column: 'authors', family: 'keyword', operands: ['contains', 'prefix', 'eq']},
    {column: 'citations', family: 'numeric', operands: ['range']},
    {column: 'published_at', family: 'numeric', operands: ['range']}
  ]
});

/** The world position of a grid-unit coordinate and back. */
const toGrid = (world: number): number => Math.round((world / WORLD_SIZE) * GRID32);

/** A topic's outline: an irregular octagon around its centre, in grid units. */
function hull(c: Cluster, rand: () => number): [number, number][][][] {
  const ring: [number, number][] = [];
  for (let k = 0; k < 8; k++) {
    const a = (k / 8) * 2 * Math.PI;
    const r = c.spread * (1.6 + rand() * 0.6);
    ring.push([toGrid(c.centre[0] + Math.cos(a) * r), toGrid(c.centre[1] + Math.sin(a) * r)]);
  }
  return [[ring]];
}

const artifact = (over: Partial<Artifact> & {layer: string; tesseraId: bigint}): Artifact => ({
  key: null,
  maskedCount: 0n,
  centroid: null,
  box: null,
  shape: null,
  content: [],
  parentIds: [],
  rung: 0,
  matched: null,
  highlighted: null,
  target: null,
  ...over
});

/** The served `topics` artifacts, areas then topics, and the keyword labels attached to them. */
export function topicArtifacts(): {areas: Artifact[]; topics: Artifact[]; labels: Artifact[]} {
  const rand = random(7);
  const topics = CLUSTERS.map((c, i) => {
    const r = c.spread * 2;
    return artifact({
      layer: 'topics',
      tesseraId: TOPIC_IDS[i]!,
      key: `topic-${String(i + 1).padStart(3, '0')}`,
      maskedCount: BigInt(c.n * SCALE),
      centroid: [toGrid(c.centre[0]), toGrid(c.centre[1])],
      box: [toGrid(c.centre[0] - r), toGrid(c.centre[1] - r), toGrid(c.centre[0] + r), toGrid(c.centre[1] + r)],
      shape: hull(c, rand),
      content: c.name ? [c.name] : [],
      parentIds: [AREA_IDS[c.area]!],
      rung: 1
    });
  });
  const areas = AREAS.map((name, a) => {
    const members = CLUSTERS.filter((c) => c.area === a);
    const cx = members.reduce((s, c) => s + c.centre[0], 0) / members.length;
    const cy = members.reduce((s, c) => s + c.centre[1], 0) / members.length;
    return artifact({
      layer: 'topics',
      tesseraId: AREA_IDS[a]!,
      key: `area-${a + 1}`,
      maskedCount: BigInt(members.reduce((s, c) => s + c.n, 0) * SCALE),
      centroid: [toGrid(cx), toGrid(cy)],
      box: [toGrid(cx - 120), toGrid(cy - 120), toGrid(cx + 120), toGrid(cy + 120)],
      content: [name],
      rung: 0
    });
  });
  const labels = CLUSTERS.flatMap((c, i) =>
    c.labels
      ? [artifact({layer: 'topic_labels', tesseraId: LABEL_IDS[i]!, maskedCount: BigInt(c.n * SCALE), centroid: [toGrid(c.centre[0]), toGrid(c.centre[1])], content: [c.labels], target: TOPIC_IDS[i]!})]
      : []
  );
  return {areas, topics, labels};
}

/** One mark before it is put in its tile's band. */
type Mark = {id: bigint; x: number; y: number; field: number; year: number; citations: number; cluster: number};

function generateMarks(): Mark[] {
  const rand = random(20260927);
  const marks: Mark[] = [];
  CLUSTERS.forEach((c, cluster) => {
    for (let i = 0; i < c.n; i++) {
      const x = Math.min(511, Math.max(1, c.centre[0] + gaussian(rand) * c.spread));
      const y = Math.min(511, Math.max(1, c.centre[1] + gaussian(rand) * c.spread));
      const field = rand() < 0.82 ? c.fields[Math.floor(rand() * c.fields.length)]! : 1 + Math.floor(rand() * FIELDS.length);
      const year = 1991 + Math.floor(Math.pow(rand(), 0.45) * 35);
      const citations = Math.floor(Math.exp(rand() * rand() * 9.5));
      // A random 53-bit identifier, as a blinded `tessera_id` is spread over the whole range.
      const id = BigInt(Math.floor(rand() * 2 ** 21)) * 2n ** 32n + BigInt(Math.floor(rand() * 2 ** 32));
      marks.push({id, x, y, field, year, citations, cluster});
    }
  });
  return marks;
}

const MARKS = generateMarks();

/** The depth the frame is composed at: 32 × 32 tiles of 16 world units. */
const DEPTH = 5;

export type MapOptions = {
  /** Points that satisfy the highlight, which sets each band's highlight bits. */
  highlight?: (m: {field: number; cluster: number}) => boolean;
  scheme?: PaletteScheme;
};

/**
 * The frame on screen and every projection the map reads from it: the bands at {@link DEPTH} with
 * their topic ordinals on a fresh session table, the composition, and the served topics coloured
 * by the positional palette.
 */
export function mapState(opts: MapOptions = {}): {marks: MarksProjection; tiles: TilesProjection; view: ViewProjection; artifacts: ArtifactsProjection} {
  const {areas, topics, labels} = topicArtifacts();
  const table = new SessionArtifactTable();
  const served = [...areas, ...topics, ...labels];
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds, centroid: a.centroid})));
  const topicOrdinal = topics.map((t) => table.ordinalOf('topics', t.tesseraId));
  const dim = 2 ** DEPTH;
  const tile = WORLD_SIZE / dim;
  const byTile = new Map<number, Mark[]>();
  for (const m of MARKS) {
    const tx = Math.min(dim - 1, Math.floor(m.x / tile));
    const ty = Math.min(dim - 1, Math.floor(m.y / tile));
    const k = tx * dim + ty;
    const list = byTile.get(k) ?? [];
    list.push(m);
    byTile.set(k, list);
  }
  const bands: Band[] = [];
  for (const [k, all] of [...byTile.entries()].sort((a, b) => a[0] - b[0])) {
    const tx = Math.floor(k / dim);
    const ty = k % dim;
    const visible = all.length;
    const list = [...all].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    const n = list.length;
    if (n === 0) continue;
    const lit = opts.highlight ? list.filter(opts.highlight).length : n;
    const ords = Uint32Array.from(list, (m) => topicOrdinal[m.cluster]!);
    bands.push({
      depth: DEPTH,
      prefix: mortonOfTile(tx, ty, DEPTH),
      x: tx,
      y: ty,
      ids: BigUint64Array.from(list, (m) => m.id),
      positions: Float32Array.from(list.flatMap((m) => [m.x, m.y])),
      scalars: {
        field: {arrowType: 'u16', values: Uint16Array.from(list, (m) => m.field)},
        year: {arrowType: 'u16', values: Uint16Array.from(list, (m) => m.year)},
        citations: {arrowType: 'u32', values: Uint32Array.from(list, (m) => m.citations)}
      },
      membership: {topics: {ordinals: ords, distinct: Uint32Array.from(new Set(ords)).sort()}},
      highlightBits: opts.highlight ? Uint8Array.from(list, (m) => (opts.highlight!(m) ? 1 : 0)) : null,
      served: n,
      capUsed: 500,
      visible: BigInt(visible * SCALE),
      matched: BigInt(n * SCALE),
      highlighted: BigInt(lit * SCALE),
      heldBelow: list[n - 1]!.id + 1n,
      identityKey: 'gallery',
      contentKey: 'gallery',
      bytes: n * 40,
      touchedAt: 0
    });
  }
  const composition = compose({depth: DEPTH, want: {x0: 0, y0: 0, x1: dim - 1, y1: dim - 1}, version: 1, exact: bands, fallback: [], response: null, plan: {wanted: 0, novel: 0, requests: 0, bytes: 0}});
  const visible = bands.reduce((s, b) => s + Number(b.visible), 0);
  const matched = bands.reduce((s, b) => s + Number(b.matched), 0);
  const highlighted = bands.reduce((s, b) => s + Number(b.highlighted), 0);
  const shown = bands.reduce((s, b) => s + b.ids.length, 0);
  const colours = artifactColours(
    served.map((a, i) => ({ordinal: ordinals[i]!, centroid: a.centroid})),
    'positional',
    opts.scheme ?? 'light'
  );
  return {
    marks: {bands: composition.exact, standIn: composition.standIn, count: {shown, total: matched, exact: true}},
    tiles: {tiles: composition.tiles},
    view: {
      id: 'umap',
      composition,
      depth: DEPTH,
      visible: {value: visible, exact: true},
      matched: {value: matched, exact: true},
      highlighted: {value: highlighted, exact: true},
      highlighting: opts.highlight !== undefined,
      served: {shown, total: matched, exact: true},
      provisional: 0
    },
    artifacts: {
      layer: 'topics',
      layers: ['topics', 'topic_labels'],
      served,
      colourServed: [...areas, ...topics],
      lineage: servedLineage(served),
      attached: attachedTextOf(served),
      status: 'shown',
      refusal: null,
      version: 1,
      held: served.length,
      table,
      servedOrdinals: new Set(ordinals),
      shapes: new Map(),
      colours: colours as Map<number, Rgba>,
      palette: 'positional',
      coverage: {current: bands.length, stale: 0}
    }
  };
}

/** The legend a frame of these marks accumulates: ranks by frequency, the domains, the names. */
export function legendOf(colourBy: string | null, over: Partial<LegendProjection> = {}): LegendProjection {
  const counts = new Map<number, number>();
  for (const m of MARKS) counts.set(m.field, (counts.get(m.field) ?? 0) + 1);
  const ranked = [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([code]) => code);
  return {
    ranks: {field: Object.fromEntries(ranked.map((code, i) => [code, i]))},
    domains: {
      citations: {min: 0, max: Math.max(...MARKS.map((m) => m.citations))},
      year: {min: 1991, max: 2025}
    },
    categories: {field: FIELDS},
    categoryErrors: {},
    colourBy,
    ...over
  };
}

/** A key's value for the checklist and the typeahead, with the span a query matched. */
export function suggestion(v: CategoryValue, q = ''): SuggestValue {
  const title = v.title ?? v.key;
  const at = q ? title.toLowerCase().indexOf(q.toLowerCase()) : -1;
  return {code: v.code, key: v.key, title: v.title, match: at >= 0 ? {field: 'title', start: at, len: q.length} : {field: 'key', start: 0, len: 0}};
}

/** A filters projection over `draft`, with its expressions composed as the store would. */
export function filtersOf(draft: FilterDraft, over: Partial<FiltersProjection> = {}): FiltersProjection {
  return {
    draft,
    expr: composeFilters(draft, 'filter'),
    highlight: composeFilters(draft, 'highlight'),
    members: [],
    suggestions: {},
    suggestErrors: {},
    suggestEpoch: 0,
    ...over
  };
}

/** Every control empty, in declaration order, as a fresh session holds them. */
export function emptyDraft(): FilterDraft {
  return {
    filter: {
      field: {family: 'category', keys: []},
      title: {family: 'text', query: '', mode: 'all'},
      abstract: {family: 'text', query: '', mode: 'all'},
      authors: {family: 'keyword', needle: '', op: 'contains'},
      citations: {family: 'numeric', gte: null, lte: null},
      published_at: {family: 'numeric', gte: null, lte: null}
    },
    highlight: {}
  };
}

/** The empty draft with `filter` over its filter controls and `highlight` as its highlight controls. */
export const withDraft = (filter: Record<string, ColumnDraft>, highlight: Record<string, ColumnDraft> = {}): FilterDraft => ({filter: {...emptyDraft().filter, ...filter}, highlight});

const at = (y: number, m: number, d: number) => Date.UTC(y, m - 1, d, 12) * 1000;

/** A paper's record as `/v1/items/{id}` answers it. */
export function paper(long = false): {id: bigint; detail: ItemDetail} {
  return {
    id: 1_883_204_551_907_331n,
    detail: {
      fields: {
        field: 'cs.CV',
        year: 2024,
        citations: long ? 18_204 : 412,
        title: long
          ? 'Scalable diffusion transformers with classifier-free guidance, rectified flow and adaptive layer normalisation: an empirical study across eleven image and video benchmarks at resolutions up to 4096 × 4096'
          : 'Consistency distillation for few-step image synthesis',
        authors: long
          ? 'Amara Okonkwo-Lindqvist, Jean-Baptiste Morel, Hiroshi Tanaka, Priyanka Venkataraman, Oluwaseun Adeyemi, Magdalena Wiśniewska, Tomás Ó Fearghail, Chen Xiaoming, et al. (41 more)'
          : 'A. Okonkwo, J.-B. Morel, H. Tanaka',
        abstract:
          'We study how far a single network can be distilled from a pretrained diffusion model so that it samples in one to four steps without a loss in fidelity. We find that consistency training on a curriculum of noise levels, together with a learned schedule for the discretisation, closes most of the gap to the teacher on ImageNet 512 and on text-to-image benchmarks, and we release the weights and the evaluation harness.' +
          (long ? ' Further ablations show that the distilled student inherits the teacher’s failure modes on compositional prompts, counting and text rendering, and that these are not repaired by more distillation steps. '.repeat(3) : ''),
        published_at: at(2024, 3, 14),
        doi: '10.48550/arXiv.2403.09931',
        ...(long ? {licence: 'CC BY-NC-SA 4.0', comments: '63 pages, 29 figures; v3 adds the video experiments', journal_ref: null, withdrawn: false} : {})
      },
      views: [
        {id: 'umap', x: 0.29 * GRID32, y: 0.27 * GRID32},
        {id: 'tsne', x: 0.61 * GRID32, y: 0.4 * GRID32},
        {id: 'decade:2020s', x: 0.3 * GRID32, y: 0.3 * GRID32},
        ...(long ? [{id: 'specter', x: 0.5 * GRID32, y: 0.5 * GRID32}] : [])
      ],
      scoped: {rank_in_decade: {'2020s': 412}, cluster_confidence: {'2020s': 0.87}},
      labels: long ? ['public', 'group:generative-models-reading-list', 'org:institute-for-advanced-imaging-research'] : ['public']
    }
  };
}

const TOPICS = topicArtifacts();
export const TOPIC = (i: number): Artifact => TOPICS.topics[i]!;
export const AREA = (i: number): Artifact => TOPICS.areas[i]!;

/** A hierarchy row as `POST /v1/artifacts/browse` answers it. */
export function browseRow(a: Artifact, matched: bigint | null = null): BrowseRow {
  return {tesseraId: a.tesseraId, key: a.key, name: a.content[0] ?? null, maskedCount: a.maskedCount, matchedCount: matched, rung: a.rung, parentIds: a.parentIds};
}

/** The venues filter layer's rows: two kinds of venue, and venues under them. */
export const VENUES = {
  roots: [
    {tesseraId: VENUE_IDS[0]!, key: 'conf', name: 'Conferences', maskedCount: 11_204_330n, matchedCount: null, rung: 0, parentIds: []},
    {tesseraId: VENUE_IDS[1]!, key: 'jour', name: 'Journals', maskedCount: 6_981_002n, matchedCount: null, rung: 0, parentIds: []}
  ] satisfies BrowseRow[],
  conferences: [
    {tesseraId: VENUE_IDS[2]!, key: 'neurips', name: 'NeurIPS', maskedCount: 1_402_118n, matchedCount: null, rung: 1, parentIds: [VENUE_IDS[0]!]},
    {tesseraId: VENUE_IDS[3]!, key: 'cvpr', name: 'CVPR', maskedCount: 988_201n, matchedCount: null, rung: 1, parentIds: [VENUE_IDS[0]!]},
    {tesseraId: VENUE_IDS[4]!, key: 'acl', name: 'Annual Meeting of the Association for Computational Linguistics', maskedCount: 402_770n, matchedCount: null, rung: 1, parentIds: [VENUE_IDS[0]!, VENUE_IDS[1]!]}
  ] satisfies BrowseRow[]
};

