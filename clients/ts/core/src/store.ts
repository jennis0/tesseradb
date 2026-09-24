import {ArtifactChannel, requestLevels, servedLineage, type ArtifactChannelState, type ServedLineage} from './artifactChannel.js';
import {artifactBudgetFor} from './artifactBudget.js';
import {SessionArtifactTable} from './artifactTable.js';
import {BandBudget} from './bands.js';
import {tileRectOfBbox, type DepthChoice} from './budget.js';
import {TesseraClient, type TesseraClientOptions} from './client.js';
import {ArtifactColours, ColourCoverage} from './colours.js';
import type {Composition} from './compose.js';
import {dataToWorldXY, gridToWorld, MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {NO_COUNT, NO_MASKED, type Count, type Masked} from './counts.js';
import type {Clock, DriverOptions, ViewState as DriverViewState} from './driver.js';
import {composeFilters, emptyDraft, type FilterDraft} from './filters.js';
import {HeldRecords, HeldShapes} from './held.js';
import {HeldViews, type ViewMachinery} from './heldViews.js';
import {colourLayers, isFilterLayer, layerClosure} from './layers.js';
import {CLUSTER_PREFIX, Legend, type LegendProjection} from './legend.js';
import {withMembers, type MemberClause} from './members.js';
import type {PaletteKind, PaletteScheme, Rgba} from './palette.js';
import {worldBbox} from './prefetch.js';
import {Presenter, defaultFrameScheduler, refusalOf, type FrameScheduler, type Presented, type PresentedStatus, type Refusal} from './presented.js';
import {regionOperand, withRegion} from './region.js';
import {DEFAULT_CACHE_BYTES, Replica, type ReplicaOptions} from './replica.js';
import {SelectedRegion, type RegionProjection, type SelectionShape} from './selectedRegion.js';
import {Suggestions, type SuggestState} from './suggestions.js';
import {TokenSupply, type TokenSupplier} from './token.js';
import type {
  Artifact,
  ArtifactDetail,
  BrowsePage,
  BrowseRequest,
  FilterExpr,
  ItemDetail,
  Meta,
  Quantisation,
  Shape,
  ShapeKind,
  Timings,
  ViewportResult
} from './types.js';

/**
 * The headless store: told where the viewer is looking, it hands back what to draw, from its
 * cache, on its own schedule. It holds no camera; the host calls `setView`. It hands the vis side
 * a `Composition` by reference and typed counts, never pixels.
 *
 * The frame scheduler and the clock are injected, defaulting to `requestAnimationFrame` and
 * `setTimeout`, so the store runs in node against a fake client.
 *
 * The store composes parts that each own their state: the token ({@link TokenSupply}), each view's
 * replica, presenter and artifact channel ({@link HeldViews}), the typeahead ({@link Suggestions}),
 * shapes and hovered records ({@link HeldShapes}, {@link HeldRecords}), the selected region
 * ({@link SelectedRegion}), artifact colours ({@link ArtifactColours}, {@link ColourCoverage}) and
 * the legend ({@link Legend}). What remains here is what they share: `meta`, the current view id,
 * the camera, the layers, the filters and the projections.
 */

export type {Count, Masked} from './counts.js';
export {formatCount, formatMasked} from './counts.js';
export type {TokenSupplier} from './token.js';
export {CLUSTER_PREFIX, type LegendProjection} from './legend.js';
export {REGION_HELD_LIMIT, type RegionProjection, type SelectionShape} from './selectedRegion.js';

/** A data-coordinates bbox and the pixel size it is drawn at. */
export type ViewInput = {bbox: [number, number, number, number]; width: number; height: number};

export type StoreOptions = {
  viewerUrl: string;
  /** A fixed token, or {@link authorise} for one the store renews. One of the two, or a `client`, is required. */
  token?: string;
  authorise?: TokenSupplier;
  /** Which view to answer for; defaults to `meta.views[0]`. */
  view?: string;
  /** The marks-on-screen budget the driver starts from. */
  budget?: number;
  /** How served artifacts are coloured: `positional` by default, or `spread` over the served set. */
  palette?: PaletteKind;
  prefetch?: boolean;
  /** Injected for tests; browser defaults otherwise. */
  scheduler?: FrameScheduler;
  clock?: Clock;
  driver?: DriverOptions;
  replica?: Pick<ReplicaOptions, 'cacheBytes' | 'cache' | 'revalidateAfterMs' | 'onPhase'>;
  /**
   * A `/v1/meta` the host has already read, so the store does not fetch it again. It must have been
   * fetched under this principal's token: another principal's meta lists layers and views this one
   * may not be served.
   */
  meta?: Meta;
  /** A client already built, such as a test's fake; else one is made. */
  client?: TesseraClient;
  clientOptions?: Omit<TesseraClientOptions, 'viewerUrl'>;
  /** Measurements the projections do not carry, for a demo's instrument panels. */
  instruments?: {
    onFrame?(info: {plan: {choice: DepthChoice}; timings: Timings | null; bytes: number; held: number; fetched: number; calibration: {mTarget: number; visibleInView: number | undefined}; replica: {bytes: number; points: number; bands: number}}): void;
    onTrace?(kind: string, fields: Record<string, number | string>): void;
  };
};

/** The store's projections. Each is immutable and replaced whole when it changes. */
export type Projections = {
  meta: Meta | null;
  status: StatusProjection;
  view: ViewProjection;
  marks: MarksProjection;
  tiles: TilesProjection;
  artifacts: ArtifactsProjection;
  selection: SelectionProjection;
  region: RegionProjection | null;
  filters: FiltersProjection;
  legend: LegendProjection;
  replica: ReplicaProjection;
};

export type ProjectionName = keyof Projections;

export type StatusProjection = {
  status: PresentedStatus;
  sessionWarm: boolean;
  refusal: Refusal | null;
  /** True when the content key the latest response observed differs from the presented frame's. */
  stale: boolean;
  /** Set with a refusal that means the session ended. */
  expired: boolean;
  retrying: boolean;
};

export type ViewProjection = {
  /**
   * The view the store answers from: `''` before `meta`, then an id `meta.views` lists. A component
   * compares this to learn that a switch happened.
   */
  id: string;
  /** The composition on screen, by reference. */
  composition: Composition | null;
  depth: number;
  visible: Masked;
  matched: Masked;
  /**
   * The sum of the frame's per-tile `highlighted`. Equal to `matched` where no highlight is set,
   * and also where a highlight matches everything the filter does, so {@link highlighting} says
   * whether one was asked.
   */
  highlighted: Masked;
  /** Whether the request behind this frame carried a `highlight`. */
  highlighting: boolean;
  served: Count;
  /** Provisional marks on screen: a plain mark count, not a masked quantity. */
  provisional: number;
};

export type MarksProjection = {
  /** The exact bands, by reference. */
  bands: Composition['exact'];
  standIn: Composition['standIn'];
  count: Count;
};

export type TilesProjection = {tiles: Composition['tiles']};

export type ArtifactsProjection = {
  /** The first layer drawn. */
  layer: string | null;
  /** Every layer drawn, with its closure. The colour layer is here only when it is drawn too. */
  layers: string[];
  /** The served artifacts of the drawn layers. */
  served: Artifact[];
  /** The served artifacts of the layer named by `colourBy = "cluster:<layer>"`, drawn or not; else empty. */
  colourServed: Artifact[];
  lineage: ServedLineage;
  status: ArtifactChannelState['status'];
  refusal: {code: string; detail: string} | null;
  version: number;
  /** How many artifact payloads the session holds, the served set and those kept from earlier answers. */
  held: number;
  /** The session artifact table, which resolves ordinals. */
  table: SessionArtifactTable;
  /** The drawn served set's ordinals. */
  servedOrdinals: ReadonlySet<number>;
  /**
   * Shapes fetched by {@link Store.needShape}, by `tesseraId`. An artifact with no entry has not
   * been asked for or has not answered, and is drawn as its `box`.
   */
  shapes: ReadonlyMap<bigint, Shape>;
  /**
   * A colour for every ordinal the session table holds. A band fetched under a coarser cut names
   * artifacts outside `servedOrdinals`, and its points take those artifacts' colours. An ordinal
   * not here resolves to neutral.
   */
  colours: ReadonlyMap<number, Rgba>;
  palette: PaletteKind;
  /** How many bands in view resolve to a colour for every layer asked for, and how many are being refetched. */
  coverage: {current: number; stale: number};
};

export type SelectionProjection = {
  item: {id: bigint; detail: ItemDetail} | null;
  itemRefusal: Refusal | null;
  artifact: {id: bigint; detail: ArtifactDetail} | null;
  artifactRefusal: Refusal | null;
};

export type FiltersProjection = {
  draft: FilterDraft;
  /** The controls in the `filter` position, composed. Null for none. */
  expr: FilterExpr | null;
  /**
   * The controls and `member_of` clauses in the `highlight` position, composed. A highlight moves
   * no mask and a filter moves no highlight: a clause's position decides which field it reaches.
   */
  highlight: FilterExpr | null;
  /** The `member_of` clauses, in either position. */
  members: MemberClause[];
} & SuggestState;

export type ReplicaProjection = {
  /** Bytes held across every view's bands, the figure the one byte budget bounds. */
  bytes: number;
  points: number;
  bands: number;
  /** How many views hold any band. */
  views: number;
  lastPlan: {held: number; fetched: number} | null;
};

type Listener = () => void;

export interface Store {
  readonly projections: Projections;
  get<K extends ProjectionName>(name: K): Projections[K];
  subscribe(fn: Listener): () => void;
  subscribe<K extends ProjectionName>(name: K, fn: (value: Projections[K]) => void): () => void;

  setView(input: ViewInput): void;
  setFilters(draft: FilterDraft): void;
  /**
   * Replace the `member_of` clauses: a card's "filter to this" and "outside this", and a hierarchy
   * panel's nodes. Like `setFilters` it takes the whole set; compose it with `withMember` and
   * `withoutMember`.
   */
  setMembers(clauses: readonly MemberClause[]): void;
  /**
   * One page of a layer's hierarchy: roots, an artifact's children and parents, or a name search.
   * `filters` defaults to {@link requestFilters}, so the tree counts what the map counts, and
   * `view` to the store's current view.
   */
  browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage>;
  /**
   * The `filters` every request carries: the filter-position controls, the `member_of` clauses in
   * that position and the selected region's leaf. Null for the unfiltered request. `filters.expr`
   * holds the controls alone. The expression is JSON-safe: a `member_of` leaf carries its artifact
   * as a decimal string.
   */
  requestFilters(): FilterExpr | null;
  /**
   * Ask a category's typeahead for `q`, debounced per column. The page lands in
   * `filters.suggestions[column]`, a refusal in `filters.suggestErrors[column]`.
   */
  suggest(column: string, q: string): void;
  /** Draw layers, each with its closure; `[]` draws none. */
  setLayers(names: string[]): void;
  /**
   * Colour by a declared column, by `cluster:<layer>` for a layer {@link colourLayers} lists, or
   * `null` for uniform. Colouring by a layer does not draw it. A layer `meta` does not list as one
   * that can colour is named in no request and traced as `colour-by`, and the points draw uniform.
   */
  setColourBy(column: string | null): void;
  setPalette(kind: PaletteKind): void;
  setBudget(budget: number): void;
  /**
   * Make `id` the view the store answers from. Called before `meta` arrives, it names the view to
   * open with; an id `meta.views` does not list is ignored and traced.
   */
  setCurrentView(id: string): void;
  /** The extent the current view is quantised against, or `null` before `meta` has arrived. */
  frame(): Quantisation | null;
  pick(id: bigint): Promise<void>;
  /**
   * One item's record for a hover, asked for once per id and held, a refusal as `null`. It writes
   * no projection: {@link pick} opens a card, and a pointer crossing the map opens nothing.
   * {@link clear} and a change of token drop what is held.
   */
  describe(id: bigint): Promise<Record<string, unknown> | null>;
  openArtifact(id: bigint): Promise<void>;
  /**
   * Ask for one artifact's shape, which lands in `artifacts.shapes`, generalised to the view's
   * zoom. The viewport serves centroids and boxes only, because deriving a shape costs a pass over
   * the principal's visible members. A shape held or in flight is not asked for again, so this may
   * be called on every pointer move.
   */
  needShape(id: bigint): void;
  /** Drop the picked item and the opened artifact. */
  clearSelection(): void;
  /** The colour scheme the map draws on, which the positional palette is chosen against. */
  setScheme(scheme: PaletteScheme): void;
  select(shape: SelectionShape | null): void;
  /** A data-coordinates bbox for a served artifact. */
  extentOf(artifactId: bigint): [number, number, number, number] | null;
  /** Data coordinates of a world position. */
  dataXY(worldX: number, worldY: number): [number, number];
  /**
   * Forget everything answered under the current principal, for a re-authorise: every view's
   * marks, the shapes, records, legend, suggestions and region, and the selected item and artifact.
   */
  clear(): void;
  refresh(): void;
  dispose(): void;
}

/** The `view` projection before a frame has been drawn in view `id`. */
function noFrame(id: string): ViewProjection {
  return {id, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0};
}

const NO_STATUS: StatusProjection = {
  status: 'idle',
  sessionWarm: false,
  refusal: null,
  stale: false,
  expired: false,
  retrying: false
};

export function createStore(options: StoreOptions): Store {
  if (!options.token && !options.authorise && !options.client) {
    throw new Error('createStore needs a token, an authorise supplier, or a client');
  }

  const scheduler = options.scheduler ?? defaultFrameScheduler();
  const clock: Clock =
    options.clock ??
    ({
      now: () => (typeof performance !== 'undefined' ? performance.now() : Date.now()),
      after: (ms, fire) => setTimeout(fire, ms),
      cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)
    } as Clock);

  const client =
    options.client ??
    new TesseraClient({viewerUrl: options.viewerUrl, sessionUrl: '', ...options.clientOptions});

  const table = new SessionArtifactTable();

  let meta: Meta | null = null;
  let viewId = options.view ?? '';
  let budget = options.budget ?? 500_000;
  let contentKeyAtFrame = '';

  /** One byte budget across every view's bands, evicted least recently drawn across them. */
  const bandBudget = new BandBudget(options.replica?.cacheBytes ?? DEFAULT_CACHE_BYTES);
  const views = new HeldViews(clock, buildView);

  /** The layers drawn, with their closure, as `setLayers` last named them. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  /** A `setView` before meta. */
  let queuedView: ViewInput | null = null;
  /** A `setCurrentView` before meta, applied at warm-up in place of `options.view`. */
  let queuedCurrentView: string | null = null;
  /**
   * A switch published `loading`, and a frame drawn from the incoming view's held bands ends it,
   * since no request's status will. Cleared by {@link setView}, after which a cold switch stays at
   * `loading` until its request answers.
   */
  let awaitingSwitchFrame = false;

  const tokens = new TokenSupply(options.authorise, options.token, clock, (changed) => {
    // A derived shape and a hovered record answer one principal; a new token may be another.
    if (changed) {
      shapes.forget('derived');
      records.forget();
    }
    // A warm-up that failed for want of a token runs again now there is one.
    if (meta === null) void ready().catch(() => {});
  });

  const colours = new ArtifactColours(table, options.palette ?? 'positional', (map, palette) =>
    replaceProjection('artifacts', {...projections.artifacts, colours: map, palette})
  );
  const coverage = new ColourCoverage();

  const legend = new Legend(
    async (column, codes) => client.categories(await tokens.get(), column, {codes, view: viewId}),
    (value) => replaceProjection('legend', value)
  );

  const suggestions = new Suggestions(
    clock,
    async (column, q) => {
      const asked = await viewed();
      return client.suggest(asked.token, column, q, {view: asked.view});
    },
    (state) => replaceProjection('filters', {...projections.filters, ...state})
  );

  const shapes = new HeldShapes(
    async (id) => {
      const asked = await viewed();
      // Generalised to the pixel of the view's own zoom.
      const zoom = views.current?.presenter.view?.view.zoom;
      const detail = await client.artifact(asked.token, id, {view: asked.view, ...(zoom === undefined ? {} : {zoom})});
      return detail.shape;
    },
    shapeKindOf,
    (held) => replaceProjection('artifacts', {...projections.artifacts, shapes: held})
  );

  const region = new SelectedRegion({
    clock,
    frame: frameOrNull,
    extentOf,
    covered: (box, depth) => {
      const replica = views.current?.replica;
      return replica !== undefined && meta !== null && replica.novelIn(tileRectOfBbox(box, depth), depth, meta.selection.kMaxMarks) === 0;
    },
    publish: (value) => replaceProjection('region', value),
    trace: (kind, fields) => options.instruments?.onTrace?.(kind, fields)
  });

  const records = new HeldRecords((id) => tokens.get().then((t) => client.item(t, id)).then((detail) => detail.fields));

  const projections: Projections = {
    meta: null,
    status: NO_STATUS,
    view: noFrame(''),
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], colourServed: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table, servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette: colours.palette, coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null}
  };

  const all: Set<Listener> = new Set();
  const perName = new Map<ProjectionName, Set<() => void>>();

  /**
   * Publish one projection to every subscriber. A subscriber that throws is reported to the console
   * and to `onTrace`, and the subscribers after it are still told.
   */
  function replaceProjection<K extends ProjectionName>(name: K, value: Projections[K]): void {
    projections[name] = value;
    perName.get(name)?.forEach(deliver);
    all.forEach(deliver);
  }

  function deliver(fn: () => void): void {
    try {
      fn();
    } catch (error) {
      console.error('a store subscriber threw; the other subscribers were still told', error);
      options.instruments?.onTrace?.('subscriber-fault', {message: error instanceof Error ? error.message : String(error)});
    }
  }

  /** The token and the view a verb naming a view asks under, once the store has read its meta. */
  async function viewed(): Promise<{token: string; view: string}> {
    await ready();
    return {token: await tokens.get(), view: viewId};
  }

  /** The store's first `/v1/meta`, read once and shared. A failed warm-up is forgotten, so the next call runs it again. */
  let warming: Promise<void> | null = null;

  function ready(): Promise<void> {
    warming ??= warm().catch((error: unknown) => {
      warming = null;
      if (!disposed) {
        const refusal = refusalOf(error);
        replaceProjection('status', {...projections.status, status: 'refused', refusal, expired: tokens.isExpiry(refusal)});
      }
      throw error;
    });
    return warming;
  }

  /** The current view's frame, or `null` before `meta`. Each view of a bundle may declare its own. */
  function frameOrNull(): Quantisation | null {
    return quantisationOf(viewId);
  }

  /** One named view's frame, or `null` where the bundle declares no such view. */
  function quantisationOf(id: string): Quantisation | null {
    return meta?.views.find((v) => v.id === id)?.quantisation ?? null;
  }

  /** Whether two views quantise against one extent, compared by value. The camera carries between them. */
  function sameFrame(a: Quantisation | null, b: Quantisation | null): boolean {
    return (
      a !== null && b !== null && a.xMin === b.xMin && a.xMax === b.xMax && a.yMin === b.yMin && a.yMax === b.yMax
    );
  }

  /** {@link frameOrNull} for a caller that holds `meta`. There is no default extent to fall back to. */
  function frame(): Quantisation {
    const q = frameOrNull();
    if (!q) throw new Error(`the bundle declares no view '${viewId}'`);
    return q;
  }

  /**
   * Build one view's machinery, bound to that view's id and frame. Its events reach the projections
   * only while it is the current view. The session artifact table is shared, so an artifact served
   * in two views has one ordinal and one colour.
   */
  function buildView(id: string): ViewMachinery {
    const m = meta;
    if (!m) throw new Error('a view is built after meta');
    const q = quantisationOf(id);
    if (!q) throw new Error(`the bundle declares no view '${id}'`);
    /** Assigned below; the fetch reads it. */
    let ownPresenter: Presenter | null = null;
    const current = () => id === viewId;

    const built = new Replica(
      async (req, signal, background, onPart) => {
        const tok = await tokens.use();
        // The layers named put a membership column on each band. A counts-only revalidation
        // (`k = 0`) absorbs no points, so it names none. The artifact budget and levels are the
        // channel's, so a point's membership names an artifact of the cut the panels show.
        const zoom = ownPresenter?.view?.view.zoom ?? 0;
        const layers = req.k === 0 ? [] : layersAsked();
        return client.viewport(
          tok,
          {
            ...req,
            view: id,
            filters: requestFilters(),
            highlight: requestHighlight(),
            layers,
            ...(layers.length === 0 ? {} : {artifactBudget: artifactBudgetFor(zoom)}),
            ...(layers.length === 0 || requestLevels(m.layers, layers, zoom) === undefined ? {} : {levels: requestLevels(m.layers, layers, zoom)})
          },
          signal,
          background,
          onPart
        );
      },
      q,
      {
        view: id,
        table,
        budget: bandBudget,
        cache: options.replica?.cache,
        revalidateAfterMs: options.replica?.revalidateAfterMs,
        onPhase: (kind, ms, n) => {
          options.replica?.onPhase?.(kind, ms, n);
          // A stored slice is drawable, so the first marks arrive with the first slice.
          if (kind === 'piece' || kind === 'store') ownPresenter?.absorbed();
        },
        now: () => clock.now()
      }
    );

    ownPresenter = new Presenter(
      built,
      {
        kMaxMarks: m.selection.kMaxMarks,
        maxTilesPerRequest: m.maxTilesPerRequest,
        thetaTargetMarks: m.selection.thetaTargetMarks
      },
      clock,
      scheduler,
      {
        onPresented: (p) => {
          if (current()) onPresented(p);
        },
        onStatus: (status, refusal) => {
          if (current()) onStatus(status, refusal);
        },
        onTrace: (kind, fields) => {
          if (current()) onTrace(kind, fields);
        }
      },
      {budget, ...options.driver},
      options.prefetch ?? true
    );

    const viewChannel = new ArtifactChannel(client, {
      clock,
      view: id,
      quantisation: q,
      token: () => tokens.get(),
      // The projection's depth, which is set before the presenter's own `frame`; a view that is
      // not current reads its presenter's.
      depth: () => (current() ? projections.view.depth : undefined) ?? ownPresenter?.frame?.depth,
      maxTiles: m.maxTilesPerRequest,
      table,
      // The point path's filters, region included, so an artifact's `matched` bit counts members
      // inside the selection.
      filters: () => requestFilters(),
      declarations: m.layers,
      onChange: (state) => {
        if (current()) onArtifacts(state);
      }
    });
    // Layers set before this view existed, including before meta.
    viewChannel.setLayers(layersAsked());

    return {id, replica: built, presenter: ownPresenter, channel: viewChannel};
  }

  async function warm(): Promise<void> {
    const t = await tokens.use();
    const read = options.meta ?? (await client.meta(t));
    if (disposed) return;
    meta = read;
    // A `setCurrentView` before meta names the view to open with, in place of `options.view`.
    if (queuedCurrentView !== null) {
      const wanted = queuedCurrentView;
      queuedCurrentView = null;
      if (meta.views.some((v) => v.id === wanted)) viewId = wanted;
      else onTrace('view-switch', {refused: 1, id: wanted});
    }
    if (!viewId) viewId = meta.views[0]?.id ?? '';
    replaceProjection('meta', meta);
    replaceProjection('view', {...projections.view, id: viewId});
    // One empty control per operand set the bundle publishes.
    if (Object.keys(projections.filters.draft).length === 0) {
      const draft = emptyDraft(meta.filterOperands);
      replaceProjection('filters', {...projections.filters, draft, expr: composeFilters(draft, 'filter'), highlight: composeFilters(draft, 'highlight')});
    }

    layersOn = drawnOnly(layerClosure(meta.layers, layersOn));
    traceUnknownColourLayer();
    views.enter(viewId);

    if (queuedView) {
      const q = queuedView;
      queuedView = null;
      setView(q);
    } else if (lastView) {
      setView(lastView.input);
    }
  }

  function onStatus(status: PresentedStatus, refusal: Refusal | null): void {
    const expired = tokens.isExpiry(refusal);
    replaceProjection('status', {
      status,
      sessionWarm: projections.status.sessionWarm || status === 'shown',
      refusal,
      stale: projections.status.stale,
      expired,
      retrying: status === 'retrying'
    });
    if (status === 'refused' && refusal) region.refuse(refusal);
  }

  /**
   * `status.stale`: the content key the replica last observed differs from the one the presented
   * marks were derived under. A revalidation or an artifact response can observe a new key without
   * a redraw. Not built yet: the counts from a revalidation are not published, so the numbers stay
   * as drawn until the next derive.
   */
  function recomputeStale(): void {
    const observed = views.current?.replica.currentContentKey ?? '';
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
  }

  function onTrace(kind: string, fields: Record<string, number | string>): void {
    // A revalidation observed a content key without redrawing the marks.
    if (kind === 'revalidate') {
      recomputeStale();
      observeArtifactRotation();
    }
    options.instruments?.onTrace?.(kind, fields);
  }

  /**
   * Tell the artifact channel the content key the point path observed. A channel answering from
   * scopes it holds whole makes no request, so this is how it learns that the key moved.
   */
  function observeArtifactRotation(): void {
    const current = views.current;
    const observed = current?.replica.currentContentKey;
    if (observed) current?.channel.observeContentKey(observed);
  }

  function onPresented(p: Presented): void {
    const current = views.current;
    const replica = current?.replica;
    const frame = p.frame;
    // A derive (`p.fetched` set) redrew the marks under the current content key; a fold did not.
    const observed = replica?.currentContentKey ?? '';
    if (p.fetched) contentKeyAtFrame = observed;
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;

    let visible = 0n;
    let matched = 0n;
    let highlighted = 0n;
    let served = 0;
    for (const tile of frame.tiles) {
      if (!tile.counts) continue;
      visible += tile.counts.visible;
      matched += tile.counts.matched;
      highlighted += tile.counts.highlighted;
      served += tile.counts.served;
    }

    // A frame with marks means the session answered, while the request may still be streaming.
    if (!projections.status.sessionWarm && frame.exactDrawn + frame.provisional > 0) replaceProjection('status', {...projections.status, sessionWarm: true});
    // A switch published `loading`; a frame drawn from the view's own bands, with no request
    // behind it, ends that.
    if (awaitingSwitchFrame) {
      awaitingSwitchFrame = false;
      if (projections.status.status === 'loading') replaceProjection('status', {...projections.status, status: 'shown'});
    }
    replaceProjection('view', {
      id: viewId,
      composition: frame,
      depth: frame.depth,
      visible: {value: Number(visible), exact: true},
      matched: {value: Number(matched), exact: true},
      highlighted: {value: Number(highlighted), exact: true},
      highlighting: requestHighlight() !== null,
      served: {shown: served, total: Number(visible), exact: true},
      provisional: frame.provisional
    });
    replaceProjection('marks', {
      bands: frame.exact,
      standIn: frame.standIn,
      count: {shown: frame.exactDrawn, total: Number(visible), exact: true}
    });
    // The channel needs a drawn depth to ask at, so the first frame is when it can first ask.
    if (current && !current.channel.hasView && lastView && current.presenter.view) {
      const v = current.presenter.view;
      current.channel.schedule({target: v.view.target, zoom: v.view.zoom}, v.width, v.height);
    }
    replaceProjection('tiles', {tiles: frame.tiles});
    region.answer({
      marks: {bands: frame.exact, standIn: frame.standIn},
      depth: frame.depth,
      verdict: replica?.lastRegionVerdict ?? null,
      fetched: p.fetched !== null,
      matched: Number(matched),
      narrowed: filtersBesideRegion() !== null
    });
    legend.accumulate(frame, meta?.declaredScalars ?? []);
    colours.refresh();
    checkColourCoverage();

    if (replica) {
      const fetched = p.fetched;
      publishReplica(
        fetched
          ? {held: fetched.plan.wanted - fetched.plan.novel, fetched: fetched.plan.novel}
          : projections.replica.lastPlan
      );
    }
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
    if (p.fetched) observeArtifactRotation();
    if (options.instruments?.onFrame && replica && p.fetched) {
      options.instruments.onFrame({
        plan: {choice: p.plan.choice},
        timings: p.fetched.response?.timings ?? null,
        bytes: p.fetched.plan.bytes,
        held: p.fetched.plan.wanted - p.fetched.plan.novel,
        fetched: p.fetched.plan.novel,
        calibration: p.calibration,
        replica: {bytes: replica.bytes, points: replica.points, bands: replica.bandCount}
      });
    }
  }

  /** `bytes` and `views` count every view's bands; `points` and `bands` the current view's. */
  function publishReplica(lastPlan: ReplicaProjection['lastPlan']): void {
    const replica = views.current?.replica;
    replaceProjection('replica', {
      bytes: replica?.bytes ?? 0,
      points: replica?.points ?? 0,
      bands: replica?.bandCount ?? 0,
      views: replica?.heldViews ?? 0,
      lastPlan
    });
  }

  function onArtifacts(state: ArtifactChannelState): void {
    recomputeStale();
    // The channel is asked for the colour layer as well as the drawn ones. Only the drawn layers'
    // rows reach `served`; the colour layer's reach `colourServed`.
    const drawn = new Set(layersOn);
    const served = state.artifacts.every((a) => drawn.has(a.layer)) ? state.artifacts : state.artifacts.filter((a) => drawn.has(a.layer));
    const coloured = colourLayer();
    const servedOrdinals = new Set<number>();
    for (const a of served) {
      const ordinal = table.ordinalOf(a.layer, a.tesseraId);
      if (ordinal !== 0) servedOrdinals.add(ordinal);
    }
    replaceProjection('artifacts', {
      ...projections.artifacts,
      layer: layersOn[0] ?? null,
      layers: layersOn,
      served,
      colourServed: coloured === null ? [] : state.artifacts.filter((a) => a.layer === coloured),
      lineage: servedLineage(served),
      status: state.status,
      refusal: state.refusal,
      version: state.version,
      held: state.held,
      table,
      servedOrdinals,
      shapes: shapes.shapes,
      colours: colours.current(),
      palette: colours.palette
    });
    checkColourCoverage();
  }

  /**
   * Publish the colour coverage of the bands in view, and fetch the colour-stale ones again. In
   * view is the visible box: the point path also fetches a margin, whose bands name artifacts the
   * channel did not serve for this view.
   */
  function checkColourCoverage(): void {
    const a = projections.artifacts;
    const layers = layersAsked();
    const machinery = views.current;
    if (!machinery || a.status !== 'shown' || layers.length === 0) {
      if (a.coverage.stale !== 0 || a.coverage.current !== 0) replaceProjection('artifacts', {...a, coverage: {current: 0, stale: 0}});
      return;
    }
    const started = clock.now();
    const v = machinery.presenter.view;
    const depth = projections.view.depth;
    const visible = v ? tileRectOfBbox(worldBbox({target: [v.view.target[0], v.view.target[1]], zoom: v.view.zoom, width: v.width, height: v.height}, 1), depth) : null;
    const bands = projections.marks.bands;
    // While a switch settles nothing is asked for: asking reschedules the driver, which would
    // request a view the slider is passing through.
    const {current, stale, toAsk} = coverage.check({bands, layers, table, colours: a.colours, version: a.version, visible, depth, mayAsk: !views.settling});
    if (toAsk.length > 0) {
      machinery.replica.retract(toAsk);
      machinery.presenter.reschedule();
    }
    options.instruments?.onTrace?.('coverage', {ms: clock.now() - started, bands: bands.length, stale, asked: toAsk.length});
    if (a.coverage.current !== current || a.coverage.stale !== stale) {
      replaceProjection('artifacts', {...projections.artifacts, coverage: {current, stale}});
    }
  }

  function toDriverView(input: ViewInput): DriverViewState & {width: number; height: number} {
    const q = frame();
    const [dx0, dy0, dx1, dy1] = input.bbox;
    const [wx0, wy0] = dataToWorldXY(dx0, dy0, q);
    const [wx1, wy1] = dataToWorldXY(dx1, dy1, q);
    const bw = Math.abs(wx1 - wx0) || 1;
    const bh = Math.abs(wy1 - wy0) || 1;
    // Zoom from the tighter axis, so a camera of another aspect over-covers the other axis.
    const zoom = Math.min(MAX_DEPTH, Math.log2(Math.min(input.width / bw, input.height / bh)));
    return {
      target: [(wx0 + wx1) / 2, (wy0 + wy1) / 2, 0],
      zoom,
      width: input.width,
      height: input.height
    };
  }

  /**
   * Make `id` the view the store answers from. Between views that share a frame the camera and the
   * selection carry over: the incoming view draws what it holds, and asks after the settle. Across
   * frames both are dropped, and the map's refit supplies the next `setView`. An id `meta.views`
   * does not list is ignored and traced.
   */
  function setCurrentView(id: string): void {
    if (!meta) {
      queuedCurrentView = id;
      return;
    }
    if (id === viewId) return;
    if (!meta.views.some((v) => v.id === id)) {
      onTrace('view-switch', {refused: 1, id});
      return;
    }

    const from = viewId;
    const kept = sameFrame(quantisationOf(from), quantisationOf(id));
    // A suggestion page answers one view.
    suggestions.reset();

    // Bound before anything below publishes, so a subscriber's `setView` reaches the incoming view.
    const incoming = views.enter(id);
    viewId = id;
    // A `setView` from a subscriber clears this, and its request then answers for the status.
    awaitingSwitchFrame = true;
    // The shared settings reach a view as it becomes current. Set on a held view, they would make it ask.
    incoming.channel.setLayers(layersAsked());
    incoming.presenter.setBudget(budget);
    // The content key, the bands asked for again and the shapes were the outgoing view's.
    contentKeyAtFrame = '';
    coverage.forget();
    shapes.forget('all');

    if (!kept) {
      // The camera and the selection are in the outgoing frame's data coordinates.
      lastView = null;
      region.drop();
    }

    // No marks until the incoming view presents, and the incoming channel's artifacts.
    replaceProjection('view', noFrame(id));
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    onArtifacts(incoming.channel.current);
    replaceProjection('status', {...projections.status, status: 'loading', refusal: null, stale: false});
    publishReplica(projections.replica.lastPlan);

    if (kept && lastView) {
      const v = toDriverView(lastView.input);
      // Draws what the view holds on the next scheduler tick, without a request.
      incoming.presenter.redraw({target: v.target, zoom: v.zoom}, v.width, v.height);
      views.settle(() => {
        if (lastView) setView(lastView.input);
      });
    }

    onTrace('view-switch', {from, to: id, sameFrame: kept ? 1 : 0});
  }

  function setView(input: ViewInput): void {
    lastView = {input};
    // From here the request's own status transitions answer for the status.
    awaitingSwitchFrame = false;
    const current = views.current;
    if (!meta || !current) {
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    current.presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    current.channel.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
  }

  /**
   * The layers a viewport request may name: all but the filter layers, which declare
   * `computed = []` and are applied only as `member_of` clauses. Filtered here so that a host
   * calling `setLayers` directly cannot draw one.
   */
  function drawnOnly(names: readonly string[]): string[] {
    if (!meta) return [...names];
    const filterLayers = new Set(meta.layers.filter(isFilterLayer).map((l) => l.name));
    return names.filter((n) => !filterLayers.has(n));
  }

  /** The layer `colourBy` colours by, where `meta` lists it as one that can colour; else null. */
  function colourLayer(): string | null {
    const colourBy = legend.colourBy;
    if (!meta || !colourBy?.startsWith(CLUSTER_PREFIX)) return null;
    const name = colourBy.slice(CLUSTER_PREFIX.length);
    return colourLayers(meta.layers).some((l) => l.name === name) ? name : null;
  }

  function traceUnknownColourLayer(): void {
    const colourBy = legend.colourBy;
    if (meta && colourBy?.startsWith(CLUSTER_PREFIX) && colourLayer() === null) onTrace('colour-by', {refused: 1, layer: colourBy.slice(CLUSTER_PREFIX.length)});
  }

  /**
   * The layers a viewport request names: the drawn layers with their closure, and the colour
   * layer alone. The colour layer's dependents are not asked for, since nothing of it is drawn.
   */
  function layersAsked(): string[] {
    const coloured = colourLayer();
    return coloured === null || layersOn.includes(coloured) ? layersOn : [...layersOn, coloured];
  }

  /**
   * Point the channel at the layers asked for and publish the artifacts as they now stand. A layer
   * newly asked for is fetched at once, and the bands in view lacking its membership column are
   * refetched by the coverage check. A layer dropped costs nothing: its rows are filtered out.
   */
  function askLayers(): void {
    const current = views.current;
    if (!current) {
      // Before meta: published now, and asked for when the view is built.
      replaceProjection('artifacts', {...projections.artifacts, layer: layersOn[0] ?? null, layers: layersOn});
      return;
    }
    const {channel, presenter} = current;
    const before = channel.current.layers;
    const asked = layersAsked();
    // The channel publishes when its layers move; when they do not, the drawn set may still have.
    if (asked.length === before.length && asked.every((l, i) => l === before[i])) onArtifacts(channel.current);
    else channel.setLayers(asked);
    if (asked.some((l) => !before.includes(l)) && presenter.view) {
      const v = presenter.view;
      channel.refresh(v.view, v.width, v.height);
    }
  }

  /** The filter-position controls and `member_of` clauses, without the region leaf. */
  function filtersBesideRegion(): FilterExpr | null {
    return withMembers(composeFilters(projections.filters.draft, 'filter'), projections.filters.members, 'filter');
  }

  /** One composition for the point path, the artifact channel and the region's count. */
  function requestFilters(): FilterExpr | null {
    const selected = region.selected;
    return withRegion(filtersBesideRegion(), selected ? regionOperand(selected) : null, selected?.outside ?? false);
  }

  /**
   * The `highlight` every request carries: the controls and `member_of` clauses in that position.
   * The region is never here, because its numbers are read off the frame's `matched`.
   */
  function requestHighlight(): FilterExpr | null {
    return withMembers(composeFilters(projections.filters.draft, 'highlight'), projections.filters.members, 'highlight');
  }

  /**
   * The filters or the selection changed, so every view's held bands answer another question. A
   * held view is reset without asking, and refills when it is next current.
   */
  function requery(): void {
    for (const held of views.all()) {
      held.presenter.cancel();
      held.replica.reset();
    }
    contentKeyAtFrame = '';
    if (lastView) setView(lastView.input);
  }

  async function browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage> {
    const asked = await viewed();
    // A caller's `filters: null` asks for the unfiltered counts.
    const filters = 'filters' in req ? (req.filters ?? null) : requestFilters();
    // Not a spread alone: a caller's `view: undefined` would replace the store's view.
    return client.browse(asked.token, {...req, view: req.view ?? asked.view, filters});
  }

  function setMembers(clauses: readonly MemberClause[]): void {
    const members = [...clauses];
    replaceProjection('filters', {...projections.filters, members});
    region.loading(projections.marks);
    requery();
  }

  function setFilters(draft: FilterDraft): void {
    const expr = composeFilters(draft, 'filter');
    replaceProjection('filters', {...projections.filters, draft, expr, highlight: composeFilters(draft, 'highlight')});
    region.loading(projections.marks);
    requery();
  }

  function setLayers(names: string[]): void {
    layersOn = meta ? drawnOnly(layerClosure(meta.layers, names)) : names;
    askLayers();
  }

  function clearSelection(): void {
    replaceProjection('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: null});
  }

  function setColourBy(column: string | null): void {
    const before = colourLayer();
    legend.setColourBy(column);
    traceUnknownColourLayer();
    if (colourLayer() !== before) askLayers();
    // Every declared column is in the held bands, so a column needs no refetch.
    if (column && projections.view.composition) legend.accumulate(projections.view.composition, meta?.declaredScalars ?? []);
  }

  function setBudget(next: number): void {
    if (!Number.isFinite(next) || next <= 0) return;
    budget = next;
    views.current?.presenter.setBudget(next);
    views.current?.presenter.reschedule();
  }

  async function pick(id: bigint): Promise<void> {
    const epoch = clears;
    try {
      const detail = await client.item(await tokens.get(), id);
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {...projections.selection, item: {id, detail}, itemRefusal: null});
    } catch (error) {
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {
        ...projections.selection,
        item: null,
        itemRefusal: refusalOf(error)
      });
    }
  }

  /** The kind a served artifact's layer draws, from the meta; null where it draws none. */
  function shapeKindOf(id: bigint): ShapeKind | null {
    const layer = projections.artifacts.served.find((a) => a.tesseraId === id)?.layer;
    if (layer === undefined) return null;
    return projections.meta?.layers.find((l) => l.name === layer)?.shape ?? null;
  }

  async function openArtifact(id: bigint): Promise<void> {
    const epoch = clears;
    try {
      const asked = await viewed();
      const detail = await client.artifact(asked.token, id, {view: asked.view});
      if (disposed || epoch !== clears) return;
      // A renewal may have changed the principal under the request, and a shape can be theirs.
      if (tokens.current === asked.token) shapes.hold(id, detail.shape);
      replaceProjection('selection', {...projections.selection, artifact: {id, detail}, artifactRefusal: null, item: null, itemRefusal: null});
    } catch (error) {
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {
        ...projections.selection,
        artifact: null,
        artifactRefusal: refusalOf(error)
      });
    }
  }

  function extentOf(artifactId: bigint): [number, number, number, number] | null {
    const artifact = projections.artifacts.served.find((a) => a.tesseraId === artifactId);
    const box = artifact?.box;
    if (!box || !meta) return null;
    // The box is in the wire's 32-bit grid units.
    const [x0, y0] = dataXY(gridToWorld(box[0]), gridToWorld(box[1]));
    const [x1, y1] = dataXY(gridToWorld(box[2]), gridToWorld(box[3]));
    return [x0, y0, x1, y1];
  }

  function dataXY(worldX: number, worldY: number): [number, number] {
    const q = frame();
    return [
      q.xMin + (worldX / WORLD_SIZE) * (q.xMax - q.xMin),
      q.yMin + (worldY / WORLD_SIZE) * (q.yMax - q.yMin)
    ];
  }

  /** How many times the store has been cleared: an item or artifact asked for before a clear is not shown. */
  let clears = 0;

  function clear(): void {
    clears += 1;
    suggestions.reset();
    // Every held view, so none draws the previous principal's marks when it is returned to.
    for (const held of views.all()) {
      held.presenter.cancel();
      held.channel.reset();
      held.replica.reset();
    }
    views.cancelSettle();
    table.clear();
    shapes.forget('all');
    records.forget();
    clearSelection();
    contentKeyAtFrame = '';
    replaceProjection('view', noFrame(viewId));
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    legend.clear();
    replaceProjection('status', {...NO_STATUS});
    region.drop();
    publishReplica(null);
  }

  function refresh(): void {
    // Redraw against the current content key, refetching the artifacts with it.
    views.current?.channel.reset();
    region.loading(projections.marks);
    if (lastView) setView(lastView.input);
  }

  /** Set by `dispose`: an answer that lands afterwards writes nothing. */
  let disposed = false;

  function dispose(): void {
    disposed = true;
    tokens.dispose();
    suggestions.dispose();
    shapes.dispose();
    records.dispose();
    legend.dispose();
    for (const held of views.all()) {
      held.presenter.cancel();
      held.channel.cancel();
    }
    views.cancelSettle();
    client.close();
  }

  const store: Store = {
    get projections() {
      return projections;
    },
    get(name) {
      return projections[name];
    },
    subscribe(nameOrFn: ProjectionName | Listener, fn?: (value: never) => void): () => void {
      if (typeof nameOrFn === 'function') {
        all.add(nameOrFn);
        return () => all.delete(nameOrFn);
      }
      const name = nameOrFn;
      const set = perName.get(name) ?? new Set();
      const wrapped = () => (fn as (v: Projections[typeof name]) => void)(projections[name]);
      set.add(wrapped);
      perName.set(name, set);
      return () => set.delete(wrapped);
    },
    setView,
    browse,
    requestFilters,
    setFilters,
    setMembers,
    suggest: (column, q) => suggestions.suggest(column, q),
    setLayers,
    setColourBy,
    setPalette: (kind) => colours.setPalette(kind),
    setBudget,
    setCurrentView,
    frame: frameOrNull,
    pick,
    describe: (id) => records.describe(id),
    openArtifact,
    needShape: (id) => shapes.need(id),
    clearSelection,
    setScheme: (scheme) => colours.setScheme(scheme),
    select(shape) {
      if (region.select(shape, projections.marks)) requery();
    },
    extentOf,
    dataXY,
    clear,
    refresh,
    dispose
  };

  void ready().catch(() => {});

  return store;
}

/** Re-export for a consumer building a `ViewportResult` in a test. */
export type {ViewportResult};
