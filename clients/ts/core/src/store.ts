import {ArtifactChannel, servedLineage, type ArtifactChannelState, type ServedLineage} from './artifactChannel.js';
import {SessionArtifactTable} from './artifactTable.js';
import type {Composition} from './compose.js';
import {NO_COUNT, NO_MASKED, type Count, type Masked} from './counts.js';
import {dataToWorldXY, MAX_DEPTH, WORLD_SIZE} from './coords.js';
import type {Clock, DriverOptions, ViewState as DriverViewState} from './driver.js';
import {countCodesCached, countCodesInPiece, extendRanks, widenDomain, widenDomainOver, type Domain, type Ranks} from './encoding.js';
import {composeFilters, emptyDraft, type FilterDraft} from './filters.js';
import {Presenter, defaultFrameScheduler, type FrameScheduler, type PresentedStatus, type Refusal} from './presented.js';
import {Replica, type ReplicaOptions} from './replica.js';
import {TesseraClient, TesseraError, type TesseraClientOptions} from './client.js';
import type {DepthChoice} from './budget.js';
import type {Presented} from './presented.js';
import type {
  Artifact,
  ArtifactDetail,
  CategoryValue,
  FilterExpr,
  ItemDetail,
  Meta,
  ViewportResult
} from './types.js';

/**
 * The headless store (design client-components §4): *tell it where I am looking, and it hands you
 * what to draw, from its cache, with its scheduling.* It is `@tesseradb/client`'s main export.
 *
 * Behind it: the session client, the replica, the driver and the presented frame (all in this
 * package), the artifact channel, the encoding accumulators, item and artifact detail, filter
 * composition, and the session artifact table. What it does **not** own is a camera — C2's engine
 * does, and `setView` is how it is told (§4). Rendering is the vis side's: the store hands over a
 * `Composition` by reference and the numbers typed by what they are, never pixels.
 *
 * **Headless by construction.** The frame scheduler and the driver clock are injected, defaulting
 * to `requestAnimationFrame`/`setTimeout`, so the whole store is testable in node against a fake
 * `fetch` (or a fake `TesseraClient`) with a fake scheduler.
 *
 * ⊘ **Selection counting, and the membership column, are step 2 and step 3.** `select(shape)`
 * records the shape and `region` is null; the session artifact table is built and refcounted now
 * but filled only by the artifact channel's served set until the wire carries per-point membership
 * (D12).
 */

export type {Count, Masked} from './counts.js';
export {formatCount, formatMasked} from './counts.js';

/** A data-coordinates bbox and the pixel size it is drawn at — what `setView` takes (§4). */
export type ViewInput = {bbox: [number, number, number, number]; width: number; height: number};

/** A selection shape — recorded now; its counting request is step 2 (§5.11). */
export type SelectionShape =
  | {kind: 'box'; bbox: [number, number, number, number]}
  | {kind: 'lasso'; points: [number, number][]};

/** How to get a viewer token: a fixed string, or a supplier the store renews before expiry. */
export type TokenSupplier = () => Promise<{token: string; expiresAt: number}>;

export type StoreOptions = {
  viewerUrl: string;
  /** A fixed token, or {@link authorise} for one the store renews. Exactly one is required. */
  token?: string;
  authorise?: TokenSupplier;
  /** Which view to answer for; defaults to `meta.views[0]`. */
  view?: string;
  /** Marks-on-screen budget — the input's default, not a ceiling (design, owner 2026-08-25). */
  budget?: number;
  prefetch?: boolean;
  /** Injected for tests; browser defaults otherwise. */
  scheduler?: FrameScheduler;
  clock?: Clock;
  driver?: DriverOptions;
  replica?: Pick<ReplicaOptions, 'cacheBytes' | 'cache' | 'revalidateAfterMs' | 'onPhase'>;
  /** A client already built (a test's fake, or a host that owns `authorise`); else one is made. */
  client?: TesseraClient;
  clientOptions?: Omit<TesseraClientOptions, 'viewerUrl'>;
  /**
   * A demo-only side channel for measurement instruments — **not part of §4**. A conformant
   * client reads projections; the demo's depth/stage/calibration panels read numbers the §4
   * surface deliberately omits (predicted marks, stage timings, the plan's held-vs-fetched split,
   * m_target), so they are forwarded here rather than widening the projection table for them.
   */
  instruments?: {
    onFrame?(info: {plan: {choice: DepthChoice}; timings: import('./types.js').Timings | null; bytes: number; held: number; fetched: number; calibration: {mTarget: number; visibleInView: number | undefined}; replica: {bytes: number; points: number; bands: number}}): void;
    onTrace?(kind: string, fields: Record<string, number>): void;
  };
};

/** The projections table of §4 — each an immutable object, replaced on change. */
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
  /** Set with a refusal that means the session ended (design §5.4's expired row). */
  expired: boolean;
  retrying: boolean;
};

export type ViewProjection = {
  /** The composition on screen and the depth it is drawn at, by reference — the deck side's input. */
  composition: Composition | null;
  depth: number;
  visible: Masked;
  matched: Masked;
  served: Count;
  /** Provisional marks — a screen fact, a plain mark count, never a masked quantity. */
  provisional: number;
};

export type MarksProjection = {
  /** The exact bands, by reference — the slab writes them once; nothing here copies them. */
  bands: Composition['exact'];
  standIn: Composition['standIn'];
  count: Count;
};

export type TilesProjection = {tiles: Composition['tiles']};

export type ArtifactsProjection = {
  layer: string | null;
  served: Artifact[];
  lineage: ServedLineage;
  status: ArtifactChannelState['status'];
  refusal: {code: string; detail: string} | null;
  version: number;
  /** The session artifact table, for a consumer resolving ordinals (§5.10). */
  table: SessionArtifactTable;
};

export type SelectionProjection = {
  item: {id: bigint; detail: ItemDetail} | null;
  itemRefusal: Refusal | null;
  artifact: {id: bigint; detail: ArtifactDetail} | null;
  artifactRefusal: Refusal | null;
};

export type RegionProjection = {
  shape: SelectionShape;
  visible: Masked;
  matched: Masked;
  served: Count;
  depth: number;
};

export type FiltersProjection = {
  draft: FilterDraft;
  expr: FilterExpr | null;
  values: Record<string, CategoryValue[]>;
  valueErrors: Record<string, Refusal>;
};

export type LegendProjection = {
  /** Palette rank per code, per column — assigned by observed frequency, never reordered. */
  ranks: Record<string, Ranks>;
  /** Sticky numeric domains per column — widened as marks arrive, never narrowed. */
  domains: Record<string, Domain>;
  /** Resolved category values per column — the codes drawn, named. */
  categories: Record<string, CategoryValue[]>;
  categoryErrors: Record<string, Refusal>;
  colourBy: string | null;
};

export type ReplicaProjection = {
  bytes: number;
  points: number;
  bands: number;
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
  /** Page a filterable category's value set into `filters.values` — for its picker. */
  loadFilterValues(column: string): Promise<void>;
  setLayers(names: string[]): void;
  setColourBy(column: string | null): void;
  setBudget(budget: number): void;
  pick(id: bigint): Promise<void>;
  openArtifact(id: bigint): Promise<void>;
  select(shape: SelectionShape | null): void;
  /** A data-coordinates bbox for an artifact — what a map's `fitTo` uses. */
  extentOf(artifactId: bigint): [number, number, number, number] | null;
  /** Data coordinates from marks or artifact geometry, derived from world positions. */
  dataXY(worldX: number, worldY: number): [number, number];
  clear(): void;
  refresh(): void;
  dispose(): void;
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

  // The token the store holds, renewed before the first refusal, and whether this store has ever
  // used one — a 401 on a token it *did* use is a swept session (design §5.4), not a bad option.
  let token: string | null = options.token ?? null;
  let expiresAt = Infinity;
  let renewTimer: unknown = null;
  let tokenEverUsed = false;

  let meta: Meta | null = null;
  let viewId = options.view ?? '';
  let budget = options.budget ?? 500_000;
  let colourBy: string | null = null;
  let contentKeyAtFrame = '';
  let selection: SelectionShape | null = null;

  // Replica and the machinery on top of it are built after `meta`, which carries the quantisation.
  let replica: Replica | null = null;
  let presenter: Presenter | null = null;
  let channel: ArtifactChannel | null = null;
  /** The layers `setLayers` last named — held here so a call before meta survives to the channel. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  let queuedView: ViewInput | null = null; // a setView before meta arrives

  const projections: Projections = {
    meta: null,
    status: NO_STATUS,
    view: {composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: {ids: new BigUint64Array(0), positions: new Float32Array(0), scalars: {}} as never, count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, served: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, table},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, values: {}, valueErrors: {}},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, lastPlan: null}
  };

  const all: Set<Listener> = new Set();
  const perName = new Map<ProjectionName, Set<() => void>>();

  function replaceProjection<K extends ProjectionName>(name: K, value: Projections[K]): void {
    projections[name] = value;
    perName.get(name)?.forEach((fn) => fn());
    all.forEach((fn) => fn());
  }

  // ---- the token supplier -------------------------------------------------------------------

  async function ensureToken(): Promise<string> {
    if (token && Date.now() < expiresAt - 5_000) return token;
    if (!options.authorise) {
      if (!token) throw new TesseraError(401, 'bad-credential', 'no token');
      return token;
    }
    const got = await options.authorise();
    token = got.token;
    expiresAt = got.expiresAt * (got.expiresAt < 1e12 ? 1000 : 1); // seconds or ms, tolerant
    armRenewal();
    return token;
  }

  function armRenewal(): void {
    if (renewTimer) clock.cancel(renewTimer);
    if (!options.authorise || !Number.isFinite(expiresAt)) return;
    // Renew a beat before expiry, so a warm client never presents a token the server will refuse.
    const wait = Math.max(0, expiresAt - Date.now() - 30_000);
    renewTimer = clock.after(wait, () => {
      void ensureToken().catch(() => {});
    });
  }

  /** Whether a refusal means the session ended (design §5.4's expired row). */
  function isExpiry(refusal: Refusal | null): boolean {
    if (!refusal) return false;
    if (refusal.code === 'expired-token') return true;
    // A 401 `bad-credential` on a token this store has used is a swept session, indistinguishable
    // from one that never existed — expired either way. Before the store has used a token, it is a
    // bad option, not an expiry.
    return refusal.code === 'bad-credential' && tokenEverUsed;
  }

  // ---- session bring-up ---------------------------------------------------------------------

  async function warm(): Promise<void> {
    const t = await ensureToken();
    tokenEverUsed = true;
    meta = await client.meta(t);
    if (!viewId) viewId = meta.views[0]?.id ?? '';
    replaceProjection('meta', meta);
    // Seed the filter draft from what this bundle publishes as filterable — one control per
    // operand set, all empty. A bundle without an `abstract` simply has no abstract control.
    if (Object.keys(projections.filters.draft).length === 0) {
      const draft = emptyDraft(meta.filterOperands);
      replaceProjection('filters', {...projections.filters, draft, expr: composeFilters(draft)});
    }

    replica = new Replica(
      async (req, signal, background) => {
        const tok = await ensureToken();
        tokenEverUsed = true;
        return client.viewport(
          tok,
          {...req, view: viewId, filters: composeFilters(projections.filters.draft), layers: []},
          signal,
          background
        );
      },
      meta.quantisation,
      {
        view: viewId,
        cacheBytes: options.replica?.cacheBytes,
        cache: options.replica?.cache,
        revalidateAfterMs: options.replica?.revalidateAfterMs,
        onPhase: (kind, ms, n) => {
          options.replica?.onPhase?.(kind, ms, n);
          if (kind === 'store') presenter?.absorbed();
        },
        now: () => clock.now()
      }
    );

    presenter = new Presenter(
      replica,
      {
        kMaxMarks: meta.selection.kMaxMarks,
        maxTilesPerRequest: meta.maxTilesPerRequest,
        thetaTargetMarks: meta.selection.thetaTargetMarks
      },
      clock,
      scheduler,
      {onPresented: (p) => onPresented(p), onStatus, onTrace},
      {budget, ...options.driver},
      options.prefetch ?? true
    );

    channel = new ArtifactChannel(client, {
      clock,
      view: viewId,
      quantisation: meta.quantisation,
      token: () => token,
      // The drawn depth, from the projection the frame handler has just replaced — the presenter's
      // own handle is assigned after it hands the frame over, so it is one frame behind here.
      depth: () => projections.view.depth ?? presenter?.frame?.depth,
      table,
      onChange: onArtifacts
    });
    // A `setLayers` that arrived before meta is honoured now: the channel is what asks, and it
    // did not exist to be told. (Found by the artifacts smoke: the demo chooses its layer before
    // opening the session's store, and the choice was lost on every principal switch.)
    channel.setLayer(layersOn[0] ?? null);

    if (queuedView) {
      const q = queuedView;
      queuedView = null;
      setView(q);
    } else if (lastView) {
      setView(lastView.input);
    }
  }

  // ---- projection updates from the machinery ------------------------------------------------

  function onStatus(status: PresentedStatus, refusal: Refusal | null): void {
    const expired = isExpiry(refusal);
    replaceProjection('status', {
      status,
      sessionWarm: projections.status.sessionWarm || status === 'shown',
      refusal,
      stale: projections.status.stale,
      expired,
      retrying: status === 'retrying'
    });
    if (status === 'refused' && !refusal) return;
  }

  /**
   * Recompute staleness against the content key. `status.stale` is true when the content key the
   * latest response observed differs from the one the presented marks were derived under — the
   * built replica's own `currentContentKey`, deliberately not `x-tessera-stale` (design §4). A
   * revalidation or an artifact response can observe a bump without a redraw, which is exactly the
   * case a client wired to the geometry stamp would miss; `onTrace('revalidate')` is the signal.
   *
   * ⊘ Eager number refresh is not built here: the design has the numbers refresh from the
   * revalidation response's counts while the marks stay stale-marked until `refresh()`; the
   * replica's counts-only response is not surfaced to the store, so the numbers hold until the
   * next derive. Marking stale — the load-bearing half — is built; the eager refresh is not.
   */
  function recomputeStale(): void {
    const observed = replica?.currentContentKey ?? '';
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
  }

  function onTrace(kind: string, fields: Record<string, number>): void {
    // A revalidation observed a (possibly new) content key without redrawing the marks.
    if (kind === 'revalidate') recomputeStale();
    options.instruments?.onTrace?.(kind, fields);
  }

  function onPresented(p: Presented): void {
    const frame = p.frame;
    // A derive redrew the marks under the current content key; a fold did not, so it may reveal a
    // bump observed since. `p.fetched` is non-null exactly for a derive.
    const observed = replica?.currentContentKey ?? '';
    if (p.fetched) contentKeyAtFrame = observed;
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;

    let visible = 0n;
    let matched = 0n;
    let served = 0;
    for (const tile of frame.tiles) {
      if (!tile.counts) continue;
      visible += tile.counts.visible;
      matched += tile.counts.matched;
      served += tile.counts.served;
    }

    replaceProjection('view', {
      composition: frame,
      depth: frame.depth,
      visible: {value: Number(visible), exact: true},
      matched: {value: Number(matched), exact: true},
      served: {shown: served, total: Number(visible), exact: true},
      provisional: frame.provisional
    });
    replaceProjection('marks', {
      bands: frame.exact,
      standIn: frame.standIn,
      count: {shown: frame.exactDrawn, total: Number(visible), exact: true}
    });
    // The channel asks at a depth the drawn frame supplies, so a view noted before the first frame
    // was refused (no depth) and nothing else re-asks until the camera moves. The first derive is
    // that moment: ask now, or the session's opening view shows its points and no artifacts.
    if (channel && !channel.hasView && lastView && presenter?.view) {
      const v = presenter.view;
      channel.schedule({target: v.view.target, zoom: v.view.zoom}, v.width, v.height);
    }
    replaceProjection('tiles', {tiles: frame.tiles});
    accumulateEncoding(frame);

    if (replica) {
      const fetched = p.fetched;
      replaceProjection('replica', {
        bytes: replica.bytes,
        points: replica.points,
        bands: replica.bandCount,
        lastPlan: fetched
          ? {held: fetched.plan.wanted - fetched.plan.novel, fetched: fetched.plan.novel}
          : projections.replica.lastPlan
      });
    }
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
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

  function onArtifacts(state: ArtifactChannelState): void {
    recomputeStale();
    replaceProjection('artifacts', {
      layer: state.layer,
      served: state.artifacts,
      lineage: servedLineage(state.artifacts),
      status: state.status,
      refusal: state.refusal,
      version: state.version,
      table
    });
  }

  // ---- the encoding accumulators (in the store, §4) -----------------------------------------

  function accumulateEncoding(frame: Composition): void {
    if (!colourBy || !meta) return;
    const column = meta.declaredScalars.find((c) => c.name === colourBy);
    if (!column) return;
    if (column.category) {
      const counts = new Map<number, number>();
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) for (const [code, n] of countCodesCached(values)) counts.set(code, (counts.get(code) ?? 0) + n);
      }
      // The stand-in pieces bootstrap the palette while this depth's own bands stream in.
      if (counts.size === 0) for (const piece of frame.standIn) countCodesInPiece(counts, piece, colourBy);
      if (counts.size === 0) return;
      const ranks = extendRanks(projections.legend.ranks[colourBy] ?? {}, counts);
      if (ranks !== projections.legend.ranks[colourBy]) {
        replaceProjection('legend', {...projections.legend, ranks: {...projections.legend.ranks, [colourBy]: ranks}});
        void resolveCategoryCodes(colourBy, counts);
      }
    } else {
      let domain: Domain | null = projections.legend.domains[colourBy] ?? null;
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) domain = widenDomain(domain, values);
      }
      for (const piece of frame.standIn) {
        const values = piece.band.scalars[colourBy];
        if (values) domain = widenDomainOver(domain, values, piece.indices, piece.limit);
      }
      if (domain && domain !== projections.legend.domains[colourBy]) {
        replaceProjection('legend', {...projections.legend, domains: {...projections.legend.domains, [colourBy]: domain}});
      }
    }
  }

  async function resolveCategoryCodes(column: string, counts: Map<number, number>): Promise<void> {
    if (!token || !meta) return;
    if (projections.legend.categoryErrors[column]) return;
    const held = new Set((projections.legend.categories[column] ?? []).map((v) => v.code));
    const wanted = [...counts.keys()].filter((code) => !held.has(code));
    if (wanted.length === 0) return;
    try {
      const resolved = await client.categories(token, column, {codes: wanted});
      const byCode = new Map((projections.legend.categories[column] ?? []).map((v) => [v.code, v]));
      for (const v of resolved) byCode.set(v.code, v);
      replaceProjection('legend', {
        ...projections.legend,
        categories: {...projections.legend.categories, [column]: [...byCode.values()]}
      });
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('legend', {
        ...projections.legend,
        categoryErrors: {...projections.legend.categoryErrors, [column]: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}}
      });
    }
  }

  // ---- setView's conversion (§4) ------------------------------------------------------------

  function toDriverView(input: ViewInput): DriverViewState & {width: number; height: number} {
    const q = meta!.quantisation;
    const [dx0, dy0, dx1, dy1] = input.bbox;
    const [wx0, wy0] = dataToWorldXY(dx0, dy0, q);
    const [wx1, wy1] = dataToWorldXY(dx1, dy1, q);
    const bw = Math.abs(wx1 - wx0) || 1;
    const bh = Math.abs(wy1 - wy0) || 1;
    // Zoom from the tighter axis, so a camera whose aspect differs over-covers the other axis,
    // which is safe (§4).
    const zoom = Math.min(MAX_DEPTH, Math.log2(Math.min(input.width / bw, input.height / bh)));
    return {
      target: [(wx0 + wx1) / 2, (wy0 + wy1) / 2, 0],
      zoom,
      width: input.width,
      height: input.height
    };
  }

  // ---- verbs --------------------------------------------------------------------------------

  function setView(input: ViewInput): void {
    lastView = {input};
    if (!meta || !presenter) {
      // A setView before meta has arrived is queued (§4).
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    channel?.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
  }

  function setFilters(draft: FilterDraft): void {
    const expr = composeFilters(draft);
    replaceProjection('filters', {...projections.filters, draft, expr});
    // A filter narrows what is served without changing the identity key, so bands held under one
    // filter are renderable under another — the client that changed the question is the only party
    // that knows the held answers are to a different one (§4; `main.ts:440`'s manual reset).
    presenter?.cancel();
    replica?.reset();
    contentKeyAtFrame = '';
    if (lastView) setView(lastView.input);
  }

  async function loadFilterValues(column: string): Promise<void> {
    if (!token) return;
    if (projections.filters.values[column] || projections.filters.valueErrors[column]) return;
    try {
      const values = await client.categories(token, column);
      values.sort((a, b) => a.key.localeCompare(b.key));
      replaceProjection('filters', {...projections.filters, values: {...projections.filters.values, [column]: values}});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('filters', {
        ...projections.filters,
        valueErrors: {...projections.filters.valueErrors, [column]: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}}
      });
    }
  }

  function setLayers(names: string[]): void {
    // Usually one (owner 2026-08-25); the channel draws one at a time.
    layersOn = names;
    if (!channel) {
      // Before meta: record the intent where a reader sees it; the channel adopts it at meta.
      replaceProjection('artifacts', {...projections.artifacts, layer: names[0] ?? null});
      return;
    }
    channel.setLayer(names[0] ?? null);
    if (lastView && presenter?.view) {
      const v = presenter.view;
      channel?.refresh(v.view, v.width, v.height);
    }
  }

  function setColourBy(column: string | null): void {
    colourBy = column;
    replaceProjection('legend', {...projections.legend, colourBy: column});
    // No refetch: every declared column is already in the held response, so this is an accumulator
    // pass over what is drawn (§4). The vis side rebuilds its layers; the mark count cannot move.
    if (column && projections.view.composition) accumulateEncoding(projections.view.composition);
  }

  function setBudget(next: number): void {
    if (!Number.isFinite(next) || next <= 0) return;
    budget = next;
    presenter?.setBudget(next);
    presenter?.reschedule();
  }

  async function pick(id: bigint): Promise<void> {
    if (!token) return;
    try {
      const detail = await client.item(token, id);
      replaceProjection('selection', {...projections.selection, item: {id, detail}, itemRefusal: null});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('selection', {
        ...projections.selection,
        item: null,
        itemRefusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}
      });
    }
  }

  async function openArtifact(id: bigint): Promise<void> {
    if (!token) return;
    try {
      const detail = await client.artifact(token, id, {view: viewId});
      replaceProjection('selection', {...projections.selection, artifact: {id, detail}, artifactRefusal: null, item: null, itemRefusal: null});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('selection', {
        ...projections.selection,
        artifact: null,
        artifactRefusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}
      });
    }
  }

  function select(shape: SelectionShape | null): void {
    // ⊘ The counting request is step 2 (§5.11). Recorded here; `region` stays null until then.
    selection = shape;
    replaceProjection('region', null);
  }

  function extentOf(artifactId: bigint): [number, number, number, number] | null {
    const artifact = projections.artifacts.served.find((a) => a.tesseraId === artifactId);
    const box = artifact?.box;
    if (!box || !meta) return null;
    // The box is in grid (cell) units; convert to data coordinates through the quantisation.
    const q = meta.quantisation;
    const toData = (cell: number, min: number, max: number) => min + (cell / 65536) * (max - min);
    return [
      toData(box[0], q.xMin, q.xMax),
      toData(box[1], q.yMin, q.yMax),
      toData(box[2], q.xMin, q.xMax),
      toData(box[3], q.yMin, q.yMax)
    ];
  }

  function dataXY(worldX: number, worldY: number): [number, number] {
    const q = meta!.quantisation;
    return [
      q.xMin + (worldX / WORLD_SIZE) * (q.xMax - q.xMin),
      q.yMin + (worldY / WORLD_SIZE) * (q.yMax - q.yMin)
    ];
  }

  function clear(): void {
    presenter?.cancel();
    channel?.reset();
    replica?.reset();
    table.clear();
    contentKeyAtFrame = '';
    replaceProjection('view', {composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], count: NO_COUNT});
    replaceProjection('legend', {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy});
    replaceProjection('status', {...NO_STATUS});
  }

  function refresh(): void {
    // Redraw the marks against the refreshed content key — a held layer set goes with it (§4).
    channel?.reset();
    if (lastView) setView(lastView.input);
  }

  function dispose(): void {
    presenter?.cancel();
    channel?.cancel();
    if (renewTimer) clock.cancel(renewTimer);
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
    setFilters,
    loadFilterValues,
    setLayers,
    setColourBy,
    setBudget,
    pick,
    openArtifact,
    select,
    extentOf,
    dataXY,
    clear,
    refresh,
    dispose
  };

  void warm().catch((error) => {
    const e = error as {code?: string; detail?: string; message?: string; status?: number};
    const refusal = {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)};
    replaceProjection('status', {...projections.status, status: 'refused', refusal, expired: isExpiry(refusal)});
  });

  return store;
}

/** Re-export for a consumer building a `ViewportResult` in a test. */
export type {ViewportResult};
