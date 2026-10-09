import type {MosaicaClient} from './client.js';
import {rectArea, type TileRect} from './rects.js';
import {tileRectOfBbox} from './budget.js';
import {MAX_DEPTH, WORLD_SIZE, mortonOfTile, rectToRequestBbox} from './coords.js';
import {gridOfData} from './projection.js';
import {worldBbox, type Viewport} from './prefetch.js';
import type {ViewState} from './driver.js';
import type {AggregateCut, Artifact, FilterExpr, Layer, MapProjection, Quantisation, ViewportArtifactsFrame} from './types.js';
import {SessionArtifactTable, type ArtifactRef} from './artifactTable.js';
import {refusalOf} from './presented.js';

/**
 * The annotation channel: which artifacts the current view is served, and each one's masked count.
 *
 * It is a store of tiles, asked at the depth {@link artifactDepth} gives for the camera's zoom and
 * at most {@link ARTIFACT_TILES_PER_REQUEST} at once. `POST /v1/artifacts/viewport` answers one
 * frame per tile, and the frame of a flat or levelled layer's tile depends only on the tile, the
 * layer, the level, the filter and the response's keys: an artifact's figures are over its whole
 * visible membership, the same in every tile. So a tile is held once fetched, keyed by (layer,
 * level, depth, tile) under the identity and content keys, and a settled view asks only for the
 * tiles it does not hold, the centre first. What the view shows is the union of its tiles' artifacts.
 *
 * A treed layer (`nested`, `dag`) is cut over every requested tile together, so its frame answers
 * one view and is not held by tile: a view naming one asks for every tile it covers.
 *
 * At idle the channel asks for the ring of tiles around the view and for the view at the parent
 * depth, so a pan or a step out draws from held tiles. Held tiles are capped, the least recently
 * shown leaving first. Everything is dropped when the identity key or the content key changes, and
 * on reset.
 *
 * An artifact below its layer's existence criterion, one on a layer this principal cannot reach,
 * one suppressed and one that never existed all arrive as nothing, and the channel does not tell
 * them apart. The debounce clock is injected so the channel runs in node with a fake.
 */

/** The session's held artifacts and the channel's state, from which a projection is built. @internal */
export type ArtifactChannelState = {
  /** The first layer asked for, for a reader of one layer. */
  layer: string | null;
  /** Every layer asked for, as the store named them. */
  layers: string[];
  /** The artifacts of the view's tiles, each once. */
  artifacts: Artifact[];
  status: 'idle' | 'loading' | 'shown' | 'refused';
  refusal: {code: string; detail: string} | null;
  /** Incremented whenever `artifacts` is replaced. */
  version: number;
  /** How many distinct artifacts the held tiles carry. For instrumentation. */
  held: number;
};

export type ArtifactChannelClock = {
  after(ms: number, fire: () => void): unknown;
  cancel(handle: unknown): void;
};

function defaultClock(): ArtifactChannelClock {
  return {
    after: (ms, fire) => setTimeout(fire, ms),
    cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)
  };
}

/** How long reads by identifier wait after one fails, at first and at most. */
const LOOK_RETRY_MS = 1000;
const LOOK_RETRY_MAX_MS = 30_000;

/** How long the view must be still before the artifact request goes out. */
const SETTLE_MS = 200;

/**
 * How long the view must be quiet before the ring and the parent depth are fetched. Longer than
 * the settle debounce, so a pause between drags does not start one.
 *
 * @internal
 */
export const PREFETCH_IDLE_MS = 1500;

/**
 * How a layer takes part in fetching. A levelled layer (one that declares levels) and a flat one
 * are answered tile by tile and held by tile. A treed layer (parent-linked, no levels) is cut over
 * the whole request, so it is asked for with every tile of the view. A layer with no declaration is
 * treated as treed.
 *
 * @internal
 */
export function scopeKindOf(layer: Pick<Layer, 'hierarchy' | 'levels'>): 'levelled' | 'flat' | 'treed' {
  if (layer.levels.length > 0) return 'levelled';
  if (layer.hierarchy.kind === 'flat') return 'flat';
  return 'treed';
}

/**
 * The whole map zoom a camera at `zoom` is read as: rounded down, after a zoom within a millionth of
 * a whole number is taken as that number, since a camera framed at a whole zoom comes back from the
 * data-to-world round trip a little either side of it.
 *
 * @internal
 */
export function wholeZoom(zoom: number): number {
  return Math.floor(zoom + 1e-6);
}

/**
 * The levels the server answers a request naming no `levels` at, for one layer at one zoom,
 * mirroring the server's default. Where no level declares a zoom range every level answers;
 * otherwise a level answers where its range covers the zoom, inclusive, and a level with no range
 * answers at every zoom.
 *
 * @internal
 */
export function declaredLevelsAt(layer: Pick<Layer, 'levels'>, zoom: number): number[] {
  // A camera zoomed out past 0 shows what zoom 0 declares.
  zoom = Math.max(0, wholeZoom(zoom));
  const declared = layer.levels;
  if (declared.length === 0) return [];
  if (!declared.some((d) => d.zoom !== null)) return declared.map((d) => d.level);
  return declared.filter((d) => d.zoom === null || (d.zoom[0] <= zoom && zoom <= d.zoom[1])).map((d) => d.level);
}

/**
 * The `levels` a request over `layers` names: the union of each levelled layer's declared levels
 * at the camera zoom, or `undefined` where no named layer declares levels.
 *
 * The server's default keys on the request's `zoom`, which is the tile depth, up to two deeper than
 * the camera, so it would select levels declared for a closer zoom. The server applies one list to
 * every named layer and ignores a level a layer does not declare, so the union is safe.
 *
 * @internal
 */
export function requestLevels(declarations: ReadonlyMap<string, Pick<Layer, 'hierarchy' | 'levels'>> | readonly Pick<Layer, 'name' | 'hierarchy' | 'levels'>[], layers: readonly string[], zoom: number): number[] | undefined {
  const lookup = (name: string) => (declarations instanceof Map ? declarations.get(name) : (declarations as readonly Pick<Layer, 'name' | 'hierarchy' | 'levels'>[]).find((l) => l.name === name));
  const out = new Set<number>();
  let any = false;
  for (const name of layers) {
    const decl = lookup(name);
    if (!decl || scopeKindOf(decl) !== 'levelled') continue;
    any = true;
    for (const level of declaredLevelsAt(decl, zoom)) out.add(level);
  }
  return any ? [...out].sort((a, b) => a - b) : undefined;
}

/** `(layer, tessera_id)`: ids are unique within a layer. */
function keyOf(a: {layer: string; tesseraId: bigint}): string {
  return `${a.layer}\u0000${a.tesseraId}`;
}

/** A held tile's key: the filter its bits answer, its depth and its prefix. */
function tileKey(filter: string, depth: number, tile: bigint): string {
  return `${filter}\u0001${depth}\u0001${tile}`;
}

/** A (layer, level) a held tile answers for. A flat layer's one level is 0. */
function pairKey(layer: string, level: number): string {
  return `${layer}\u0000${level}`;
}

/** One held tile: the rows its frame carried and the (layer, level) pairs it answered. */
type HeldTile = {rows: Artifact[]; answered: Set<string>; usedAt: number; paletteSize: number | undefined};

/** What a view needs: its tiles, centre first, and the pairs and layers it is asked over. */
type Want = {
  depth: number;
  /** The filter the request sends, and the same as text, which keys the tiles it brings. */
  expression: FilterExpr | null;
  filter: string;
  tiles: bigint[];
  /** The (layer, level) pairs of the flat and levelled layers asked for. */
  pairs: Set<string>;
  /** The flat and levelled layers asked for. */
  held: string[];
  /** The treed layers asked for, and those with no declaration. */
  walked: string[];
  levels: number[] | undefined;
};

/**
 * The most tiles one artifacts request names. A tile at {@link artifactDepth}'s depth is at least
 * 128 CSS pixels across, 128 at a whole-number zoom, and a 3840 × 2160 viewport, the largest screen
 * in common use drawn at one device pixel per CSS pixel, touches at most 31 × 18 = 558 of them. So
 * any common screen is asked for at map zoom + 2, and only a larger one steps to a coarser depth.
 *
 * @internal
 */
export const ARTIFACT_TILES_PER_REQUEST = 558;

/**
 * The tile depth artifacts are asked at for a camera at `zoom` over `bbox`: map zoom + 2, the zoom
 * read by {@link wholeZoom} as the levels' zoom ranges are, within 0 to {@link MAX_DEPTH}. Where
 * `bbox` touches more than `cap` tiles at that depth it is asked at the next coarser depth, until it
 * fits. It does not depend on the depth the points are drawn at.
 *
 * Each level of a layer shows at most `perTile` artifacts in a tile, so the tiles asked for times
 * `perTile` bounds the artifacts a level draws.
 *
 * @internal
 */
export function artifactDepth(bbox: [number, number, number, number], zoom: number, cap = ARTIFACT_TILES_PER_REQUEST): number {
  let depth = Math.max(0, Math.min(MAX_DEPTH, wholeZoom(zoom) + 2));
  while (depth > 0 && rectArea(tileRectOfBbox(bbox, depth)) > cap) depth -= 1;
  return depth;
}

/** The noted view: the visible box in world space, the depth to ask at and the camera. */
type NotedView = {bbox: [number, number, number, number]; depth: number; zoom: number; target: [number, number]};

export type ArtifactChannelOptions = {
  view: string;
  quantisation: Quantisation;
  /** The view's projection, to place an artifact read by identifier. */
  projection?: MapProjection;
  /** The token to ask with; a rejection is the ask's refusal. */
  token(): Promise<string>;
  /**
   * The deployment's `max_tiles_per_request`. Where it is below {@link ARTIFACT_TILES_PER_REQUEST}
   * it is the cap instead.
   */
  maxTiles?: number;
  /**
   * The request's `per_tile`: the most artifacts one level shows in one tile. `null` asks for
   * nothing: the channel reports a refusal saying what to set.
   */
  perTile: number | null;
  /** The treed frame's `budget`. Omitted, the request names none and the cut is unbounded. */
  budget?: number;
  /** The request's `palette_size`, so each artifact carries its slot. Omitted, the request names none. */
  paletteSize?: number;
  /** The most tiles held. */
  heldTiles: number;
  /** Whether to fetch the ring and the parent depth at idle. */
  prefetch: boolean;
  onChange(state: ArtifactChannelState): void;
  clock?: ArtifactChannelClock;
  settleMs?: number;
  /** How long the view must be quiet before a prefetch goes out. See {@link PREFETCH_IDLE_MS}. */
  idleMs?: number;
  /** The session table this channel names artifacts in. */
  table?: SessionArtifactTable;
  /** The filter the view is under, or null: the expression the point path sends. */
  filters?: () => FilterExpr | null;
  /**
   * `/v1/meta`'s layer declarations, which classify each layer and carry its declared levels.
   * Without them every layer is treated as treed, so nothing is held by tile.
   */
  declarations?: readonly Layer[];
  /**
   * Called with each response's identity key, and the token it was asked under, before anything in
   * the response is held. False drops the response.
   */
  admit?(identityKey: string, token: string): boolean;
};

/** @internal */
export class ArtifactChannel {
  private inFlight: AbortController | null = null;
  private prefetching: AbortController | null = null;
  private timer: unknown = null;
  private idleTimer: unknown = null;
  private view: NotedView | null = null;
  /** The treed frame's `budget`; `undefined` asks for the finest cut. */
  private budget: number | undefined;
  /** The request's `palette_size`; `undefined` asks for no slots. */
  private paletteSize: number | undefined;
  private readonly clock: ArtifactChannelClock;
  private readonly settleMs: number;
  private readonly idleMs: number;
  private readonly table: SessionArtifactTable | null;
  private readonly declarations: Map<string, Layer>;
  /** The held tiles, by {@link tileKey}. */
  private tiles = new Map<string, HeldTile>();
  /** Incremented on every use of a tile, which orders eviction. */
  private uses = 0;
  /**
   * The treed layers' rows for the last view that asked for them, with the rows of their tiles: a
   * treed cut answers one request, so these are replaced by the next.
   */
  private walkedRows: Artifact[] = [];
  /** Per artifact the held rows name: its ordinal on the table and how many holders name it. */
  private named = new Map<string, {ordinal: number; holders: number}>();
  /** The table generation {@link named} was filled under. */
  private namedIn = 0;
  /** The keys the tiles were filled under; they are dropped when either changes. */
  private heldUnder: {identityKey: string; contentKey: string} | null = null;
  /** The tags read by identifier under {@link heldUnder}'s keys, asked once each. */
  private looked = new Set<string>();
  /** Set while reads by identifier wait out a failure, and how long the next wait is. */
  private lookPaused: unknown = null;
  private lookWait = LOOK_RETRY_MS;
  private state: ArtifactChannelState = {layer: null, layers: [], artifacts: [], status: 'idle', refusal: null, version: 0, held: 0};

  constructor(
    private readonly client: MosaicaClient,
    private readonly opts: ArtifactChannelOptions
  ) {
    this.clock = opts.clock ?? defaultClock();
    this.settleMs = opts.settleMs ?? SETTLE_MS;
    this.idleMs = opts.idleMs ?? PREFETCH_IDLE_MS;
    this.table = opts.table ?? null;
    this.declarations = new Map((opts.declarations ?? []).map((l) => [l.name, l]));
    this.budget = opts.budget;
    this.paletteSize = opts.paletteSize;
  }

  /**
   * The cut the treed frame is drawn at for the noted view: its tile depth, the box naming exactly
   * its tiles in the view's coordinates, and the budget. `null` before a view is noted.
   */
  drawnCut(): AggregateCut | null {
    const view = this.view;
    if (!view) return null;
    const bbox = rectToRequestBbox(tileRectOfBbox(view.bbox, view.depth), view.depth, this.opts.quantisation);
    return {zoom: view.depth, bbox, ...(this.budget === undefined ? {} : {budget: this.budget})};
  }

  /** Cut the treed layers to `budget`, `undefined` for the finest cut, and ask again where one is asked for. */
  setBudget(budget: number | undefined): void {
    if (budget === this.budget) return;
    this.budget = budget;
    const walked = this.state.layers.some((name) => {
      const decl = this.declarations.get(name);
      return !decl || scopeKindOf(decl) === 'treed';
    });
    if (this.view && walked) {
      this.cancelPrefetch();
      if (this.timer) this.clock.cancel(this.timer);
      this.timer = null;
      void this.request();
    }
  }

  /**
   * Ask for slots in a palette of `size` colours, and ask again for the noted view. What is held
   * stays drawn, each artifact in the palette its slot was served for, until a frame of the new
   * size replaces it; a held tile of another size counts as missing.
   */
  setPaletteSize(size: number | undefined): void {
    if (size === this.paletteSize) return;
    this.paletteSize = size;
    this.looked.clear();
    if (this.view && this.state.layers.length > 0) {
      if (this.timer) this.clock.cancel(this.timer);
      this.timer = null;
      void this.request();
    }
  }

  get current(): ArtifactChannelState {
    return this.state;
  }

  /** How many tiles are held. */
  get heldTiles(): number {
    return this.tiles.size;
  }

  /** Points the channel at one layer, or none; see {@link setLayers}. */
  setLayer(layer: string | null): void {
    this.setLayers(layer ? [layer] : []);
  }

  /**
   * Points the channel at the layers the store asks for: the drawn layers with their closure, and
   * the layer the points are coloured by.
   */
  setLayers(layers: readonly string[]): void {
    const next = [...layers];
    if (next.length === this.state.layers.length && next.every((l, i) => l === this.state.layers[i])) return;
    this.state = {...this.state, layer: next[0] ?? null, layers: next};
    this.emit();
  }

  private emit(): void {
    this.opts.onChange(this.state);
  }

  /**
   * Notes the view and asks once it has been still for `settleMs`. A request per pointer move
   * would put the server's counting inside the gesture.
   */
  schedule(view: ViewState, width: number, height: number): void {
    this.cancelPrefetch();
    this.noteView(view, width, height);
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = this.clock.after(this.settleMs, () => {
      this.timer = null;
      void this.request();
    });
  }

  /**
   * Asks now, for this view, as a layer toggle needs.
   */
  refresh(view: ViewState, width: number, height: number): void {
    this.cancelPrefetch();
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = null;
    this.noteView(view, width, height);
    void this.request();
  }

  /** The most tiles one request names. */
  private get cap(): number {
    return Math.min(ARTIFACT_TILES_PER_REQUEST, this.opts.maxTiles ?? Infinity);
  }

  /** What to ask for: the visible box, with no margin since the ring is fetched at idle. */
  private noteView(view: ViewState, width: number, height: number): void {
    const viewport: Viewport = {target: [view.target[0], view.target[1]], zoom: view.zoom, width, height};
    const bbox = worldBbox(viewport, 1);
    this.view = {bbox, depth: artifactDepth(bbox, view.zoom, this.cap), zoom: view.zoom, target: [view.target[0], view.target[1]]};
  }

  /**
   * Drop what is held and the noted view, and abandon anything in flight: a new principal, or a
   * dataset switch. The next {@link schedule} or {@link refresh} asks again.
   */
  reset(): void {
    this.cancel();
    this.dropHeld();
    this.view = null;
    this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1, held: 0};
    this.emit();
  }

  cancel(): void {
    this.cancelPrefetch();
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = null;
    this.inFlight?.abort();
    this.inFlight = null;
  }

  private cancelPrefetch(): void {
    if (this.idleTimer) this.clock.cancel(this.idleTimer);
    this.idleTimer = null;
    this.prefetching?.abort();
    this.prefetching = null;
  }

  /** Drops every held tile and row, releasing every ordinal they named. */
  private dropHeld(): void {
    if (this.table && this.table.generation === this.namedIn && this.named.size > 0) {
      this.table.release(Uint32Array.from([...this.named.values()].map((n) => n.ordinal)));
    }
    this.named.clear();
    this.tiles.clear();
    this.walkedRows = [];
    this.heldUnder = null;
    this.looked.clear();
    if (this.lookPaused !== null) this.clock.cancel(this.lookPaused);
    this.lookPaused = null;
    this.lookWait = LOOK_RETRY_MS;
  }

  /**
   * A content key another channel observed; the point path carries one on every response. On a
   * change what is held is dropped and the noted view is asked again.
   */
  observeContentKey(contentKey: string): void {
    if (!contentKey || !this.heldUnder || this.heldUnder.contentKey === contentKey) return;
    this.dropHeld();
    if (this.view && this.state.layers.length > 0) {
      void this.request();
      return;
    }
    this.state = {...this.state, held: 0};
    this.emit();
  }

  /** Takes a response's keys, dropping what is held under others. */
  private keysOf(identityKey: string, contentKey: string): void {
    if (this.heldUnder && this.heldUnder.identityKey === identityKey && this.heldUnder.contentKey === contentKey) return;
    if (this.heldUnder) this.dropHeld();
    this.heldUnder = {identityKey, contentKey};
    if (this.table) this.namedIn = this.table.generation;
  }

  /**
   * Names a frame's rows on the session table, one reference per distinct artifact held. Every row
   * is taken, so an artifact already named learns this frame's parent links, centroid and slot,
   * the slot under `paletteSize`, the size the frame was asked with; the reference a held one
   * already had is given back at once.
   */
  private name(rows: readonly Artifact[], paletteSize: number | undefined): void {
    const table = this.table;
    const ref = (a: Artifact): ArtifactRef => ({
      tesseraId: a.tesseraId,
      layer: a.layer,
      parentIds: a.parentIds,
      centroid: a.centroid,
      rung: a.rung,
      ...(paletteSize === undefined ? {} : {slot: {slot: a.slot, paletteSize}})
    });
    const ordinals = table ? table.take(rows.map(ref)) : new Uint32Array(rows.length);
    const extra: number[] = [];
    rows.forEach((a, i) => {
      const held = this.named.get(keyOf(a));
      if (held) {
        held.holders += 1;
        extra.push(ordinals[i]!);
      } else {
        this.named.set(keyOf(a), {ordinal: ordinals[i]!, holders: 1});
      }
    });
    if (table && extra.length > 0) table.release(Uint32Array.from(extra));
  }

  /** Gives back one hold on each of `rows`' artifacts, freeing those no holder names. */
  private unname(rows: readonly Artifact[]): void {
    const freed: number[] = [];
    for (const a of rows) {
      const held = this.named.get(keyOf(a));
      if (!held) continue;
      held.holders -= 1;
      if (held.holders > 0) continue;
      this.named.delete(keyOf(a));
      freed.push(held.ordinal);
    }
    if (this.table && this.table.generation === this.namedIn && freed.length > 0) this.table.release(Uint32Array.from(freed));
  }

  /** What the noted view needs, at `view`'s depth, or at a shallower one for the parent prefetch. */
  private want(view: NotedView, depth = view.depth, zoom = view.zoom): Want {
    const filters = this.opts.filters?.() ?? null;
    const held: string[] = [];
    const walked: string[] = [];
    const pairs = new Set<string>();
    for (const layer of this.state.layers) {
      const decl = this.declarations.get(layer);
      const kind = decl ? scopeKindOf(decl) : 'treed';
      if (kind === 'treed') {
        walked.push(layer);
        continue;
      }
      held.push(layer);
      for (const level of kind === 'flat' ? [0] : declaredLevelsAt(decl!, zoom)) pairs.add(pairKey(layer, level));
    }
    const rect = tileRectOfBbox(view.bbox, depth);
    return {
      depth,
      expression: filters,
      filter: filters === null ? '' : JSON.stringify(filters),
      tiles: this.centreFirst(rect, depth, view.target),
      pairs,
      held,
      walked,
      levels: requestLevels(this.declarations, this.state.layers, zoom)
    };
  }

  /** The tiles of `rect`, nearest the camera's centre first. */
  private centreFirst(rect: TileRect, depth: number, target: [number, number]): bigint[] {
    const span = WORLD_SIZE / 2 ** depth;
    const cx = target[0] / span - 0.5;
    const cy = target[1] / span - 0.5;
    const out: {tile: bigint; d: number}[] = [];
    for (let y = rect.y0; y <= rect.y1; y++) {
      for (let x = rect.x0; x <= rect.x1; x++) out.push({tile: mortonOfTile(x, y, depth), d: (x - cx) ** 2 + (y - cy) ** 2});
    }
    return out.sort((a, b) => a.d - b.d).map((t) => t.tile);
  }

  /** The tiles of `want` not held for every pair it asks for. */
  private missing(want: Want): bigint[] {
    if (want.pairs.size === 0) return [];
    return want.tiles.filter((tile) => {
      const held = this.tiles.get(tileKey(want.filter, want.depth, tile));
      if (!held || held.paletteSize !== this.paletteSize) return true;
      for (const pair of want.pairs) if (!held.answered.has(pair)) return true;
      return false;
    });
  }

  /**
   * The artifacts of the noted view: each held tile's rows of the pairs asked, and the treed rows,
   * each artifact once. One served in several tiles has the same figures in each; its `matched`
   * and `highlighted` are true where any tile's are, and its parents are those any tile served.
   */
  private compose(want: Want): Artifact[] {
    const at = new Map<string, number>();
    const out: Artifact[] = [];
    const add = (a: Artifact) => {
      const i = at.get(keyOf(a));
      if (i === undefined) {
        at.set(keyOf(a), out.length);
        out.push(a);
        return;
      }
      out[i] = merged(out[i]!, a);
    };
    const walked = new Set(want.walked);
    for (const a of this.walkedRows) if (walked.has(a.layer)) add(a);
    for (const tile of want.tiles) {
      const held = this.tiles.get(tileKey(want.filter, want.depth, tile));
      if (!held) continue;
      held.usedAt = ++this.uses;
      for (const a of held.rows) if (want.pairs.has(pairKey(a.layer, a.rung)) && held.answered.has(pairKey(a.layer, a.rung))) add(a);
    }
    return out;
  }

  /** Whether any tile of `want` is held, or the treed rows answer it. */
  private holdsAny(want: Want): boolean {
    if (want.walked.length > 0 && this.walkedRows.length > 0) return true;
    return want.tiles.some((tile) => this.tiles.has(tileKey(want.filter, want.depth, tile)));
  }

  private show(want: Want, status: ArtifactChannelState['status']): void {
    this.state = {...this.state, artifacts: this.compose(want), status, refusal: null, version: this.state.version + 1, held: this.named.size};
    this.emit();
  }

  /** Holds one tile's frame, answering `pairs`, in place of what was held for it. */
  private holdTile(want: Want, tile: bigint, rows: Artifact[], paletteSize: number | undefined): void {
    const key = tileKey(want.filter, want.depth, tile);
    const before = this.tiles.get(key);
    const kept = rows.filter((a) => want.pairs.has(pairKey(a.layer, a.rung)));
    this.name(kept, paletteSize);
    if (before) this.unname(before.rows);
    this.tiles.set(key, {rows: kept, answered: new Set(want.pairs), usedAt: ++this.uses, paletteSize});
  }

  /** Evicts the least recently used tiles past the cap, never one of `protect`'s. */
  private evict(protect: Want | null): void {
    const excess = this.tiles.size - this.opts.heldTiles;
    if (excess <= 0) return;
    const kept = new Set(protect ? protect.tiles.map((t) => tileKey(protect.filter, protect.depth, t)) : []);
    const oldest = [...this.tiles.entries()].filter(([key]) => !kept.has(key)).sort((a, b) => a[1].usedAt - b[1].usedAt);
    for (const [key, held] of oldest.slice(0, excess)) {
      this.tiles.delete(key);
      this.unname(held.rows);
    }
  }

  /**
   * One request over `tiles`, holding each frame as it lands. `walked` asks for the treed layers
   * too, whose rows replace {@link walkedRows}. Returns false where the answer was not admitted.
   */
  private async ask(want: Want, tiles: bigint[], walked: boolean, signal: AbortSignal, onFrame: () => void): Promise<boolean> {
    const token = await this.opts.token();
    if (signal.aborted) return false;
    let admitted = true;
    let stale = false;
    let first = true;
    let landed = 0;
    const walkedLayers = new Set(want.walked);
    const fresh: Artifact[] = [];
    const paletteSize = this.paletteSize;
    await this.client.viewportArtifacts(
      token,
      {
        view: this.opts.view,
        zoom: want.depth,
        tiles,
        layers: walked ? [...want.held, ...want.walked] : want.held,
        ...(want.levels === undefined ? {} : {levels: want.levels}),
        perTile: this.opts.perTile!,
        ...(want.expression === null ? {} : {filters: want.expression}),
        ...(walked && this.budget !== undefined ? {budget: this.budget} : {}),
        ...(paletteSize === undefined ? {} : {paletteSize})
      },
      {
        signal,
        onTile: (frame: ViewportArtifactsFrame, keys) => {
          if (signal.aborted) return;
          if (first) {
            first = false;
            admitted = this.opts.admit?.(keys.identityKey, token) ?? true;
            if (admitted) this.keysOf(keys.identityKey, keys.contentKey);
          }
          if (!admitted) return;
          // What is held may have moved to other keys since the first frame, as a suppression
          // moves the content key; a frame of the old keys is not filed under the new.
          const held = this.heldUnder;
          if (!held || held.identityKey !== keys.identityKey || held.contentKey !== keys.contentKey) {
            stale = true;
            return;
          }
          const own = frame.artifacts.filter((a) => walkedLayers.has(a.layer));
          if (walked && own.length > 0) {
            this.name(own, paletteSize);
            fresh.push(...own);
          }
          if (!frame.treed && frame.tile !== null) this.holdTile(want, frame.tile, frame.artifacts, paletteSize);
          // Drawn at the first frame and then at every doubling, so a wide view redraws a few times
          // and not once per tile.
          landed += 1;
          if ((landed & (landed - 1)) === 0) onFrame();
        }
      }
    );
    if (!admitted || stale || signal.aborted) {
      this.unname(fresh);
      return false;
    }
    if (walked) {
      this.unname(this.walkedRows);
      this.walkedRows = fresh;
    }
    return true;
  }

  private async request(): Promise<void> {
    // A prefetch is idle work under what was held; this request may hold something else.
    this.cancelPrefetch();
    const view = this.view;
    if (!view) return;
    this.inFlight?.abort();
    this.inFlight = null;
    if (this.state.layers.length === 0) {
      // No layer: the served set is cleared and the tiles kept, so turning a layer back on draws
      // without a refetch.
      this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1};
      this.emit();
      return;
    }
    if (this.opts.perTile === null) {
      this.state = {
        ...this.state,
        artifacts: [],
        status: 'refused',
        refusal: {code: 'per-tile', detail: "no number of artifacts per tile was given: set createStore's artifacts.perTile, at most meta.selection.maxArtifactsPerTile"},
        version: this.state.version + 1
      };
      this.emit();
      return;
    }
    const want = this.want(view);
    const walked = want.walked.length > 0;
    // A treed layer is cut over the whole view, so it asks for every tile; otherwise only those
    // not held.
    const tiles = walked ? want.tiles : this.missing(want);
    if (tiles.length === 0) {
      this.show(want, 'shown');
      this.schedulePrefetch();
      return;
    }
    const signal = new AbortController();
    this.inFlight = signal;
    // What is held for the view is drawn now; with nothing held, the last view's set stays until the
    // first tile lands.
    if (this.holdsAny(want)) this.show(want, 'loading');
    else {
      this.state = {...this.state, status: 'loading'};
      this.emit();
    }
    try {
      let done = await this.ask(want, tiles, walked, signal.signal, () => this.show(want, 'loading'));
      // A response under new keys drops what was held, so tiles of the view it did not carry are
      // asked for once more.
      const rest = done && !walked ? this.missing(want) : [];
      if (rest.length > 0 && this.inFlight === signal) done = await this.ask(want, rest, false, signal.signal, () => this.show(want, 'loading'));
      if (this.inFlight !== signal) return;
      this.inFlight = null;
      if (!done) return;
      this.evict(want);
      this.show(want, 'shown');
      this.schedulePrefetch();
    } catch (error) {
      if (signal.signal.aborted || this.inFlight !== signal) return;
      this.inFlight = null;
      // A refusal drops the served set, which answered a superseded request. The tiles are kept: a
      // failed request says nothing about whether they are still true.
      this.state = {...this.state, artifacts: [], status: 'refused', refusal: refusalOf(error), version: this.state.version + 1};
      this.emit();
    }
  }

  /** What the idle prefetch asks for next: the ring of tiles round the view, then the parent depth. */
  private prefetchNext(): {want: Want; tiles: bigint[]} | null {
    const view = this.view;
    if (!view) return null;
    const here = this.want(view);
    if (here.pairs.size === 0) return null;
    const rect = tileRectOfBbox(view.bbox, view.depth);
    const edge = 2 ** view.depth - 1;
    const ring: TileRect = {x0: Math.max(0, rect.x0 - 1), y0: Math.max(0, rect.y0 - 1), x1: Math.min(edge, rect.x1 + 1), y1: Math.min(edge, rect.y1 + 1)};
    const around = this.missing({...here, tiles: this.centreFirst(ring, view.depth, view.target)});
    const limit = this.cap;
    if (around.length > 0) return {want: here, tiles: around.slice(0, limit)};
    if (view.depth === 0) return null;
    const parent = this.want(view, view.depth - 1, Math.max(0, view.zoom - 1));
    const up = this.missing(parent);
    return up.length > 0 ? {want: parent, tiles: up.slice(0, limit)} : null;
  }

  /** Arms the idle prefetch where there is something to fetch. Interaction disarms it. */
  private schedulePrefetch(): void {
    if (!this.opts.prefetch) return;
    if (this.idleTimer) this.clock.cancel(this.idleTimer);
    this.idleTimer = null;
    if (this.prefetchNext() === null) return;
    this.idleTimer = this.clock.after(this.idleMs, () => {
      this.idleTimer = null;
      void this.prefetch();
    });
  }

  /** One prefetch request; the tiles it brings are held and change nothing drawn. */
  private async prefetch(): Promise<void> {
    if (this.inFlight || this.opts.perTile === null) return;
    const next = this.prefetchNext();
    if (!next) return;
    const signal = new AbortController();
    this.prefetching = signal;
    try {
      const done = await this.ask(next.want, next.tiles, false, signal.signal, () => {});
      if (this.prefetching !== signal) return;
      this.prefetching = null;
      if (!done) return;
      this.evict(this.view ? this.want(this.view) : null);
      this.state = {...this.state, held: this.named.size};
      this.emit();
      this.schedulePrefetch();
    } catch {
      if (this.prefetching === signal) this.prefetching = null;
    }
  }

  /**
   * Reads by identifier the artifacts that `ordinals` name and no held tile carries, such as a
   * point's tag past a tile's quota, so the table learns each one's level, parents, centroid and slot.
   * Each is asked once under the current keys. The references taken are given back at once.
   */
  async lookUp(ordinals: Iterable<number>): Promise<void> {
    const table = this.table;
    if (!table || !this.heldUnder || this.lookPaused !== null) return;
    const byLayer = new Map<string, bigint[]>();
    for (const ordinal of ordinals) {
      const entry = table.entry(ordinal);
      if (!entry) continue;
      const key = keyOf(entry);
      if (this.named.has(key) || this.looked.has(key)) continue;
      this.looked.add(key);
      const ids = byLayer.get(entry.layer) ?? [];
      ids.push(entry.tesseraId);
      byLayer.set(entry.layer, ids);
    }
    if (byLayer.size === 0) return;
    const generation = table.generation;
    const keys = this.heldUnder;
    const q = this.opts.quantisation;
    const projection = this.opts.projection ?? 'none';
    const paletteSize = this.paletteSize;
    for (const [layer, ids] of byLayer) {
      try {
        const token = await this.opts.token();
        const read = await this.client.artifacts(token, {
          view: this.opts.view,
          layer,
          ids,
          fields: paletteSize === undefined ? ['level', 'parents', 'centroid'] : ['level', 'parents', 'centroid', 'slot'],
          ...(paletteSize === undefined ? {} : {paletteSize})
        });
        const refs: ArtifactRef[] = [];
        for await (const page of read) {
          const id = page.getChild('tessera_id')!;
          const level = page.getChild('level')!;
          const parents = page.getChild('parents')!;
          const cx = page.getChild('centroid_x')!;
          const cy = page.getChild('centroid_y')!;
          const slot = page.getChild('slot');
          for (let i = 0; i < page.numRows; i++) {
            const x = cx.get(i) as number | null;
            const y = cy.get(i) as number | null;
            refs.push({
              tesseraId: BigInt(id.get(i) as bigint),
              layer,
              parentIds: Array.from((parents.get(i) as Iterable<bigint> | null) ?? [], (p) => BigInt(p)),
              centroid: x === null || y === null ? null : gridOfData(x, y, projection, q),
              rung: Number(level.get(i)),
              ...(slot && paletteSize !== undefined ? {slot: {slot: slot.get(i) === null ? null : Number(slot.get(i)), paletteSize}} : {})
            });
          }
        }
        this.lookWait = LOOK_RETRY_MS;
        if (table.generation !== generation || this.heldUnder !== keys || this.paletteSize !== paletteSize || refs.length === 0) return;
        table.release(table.take(refs));
      } catch {
        // A tag left unread keeps the neutral colour. It is asked again by the next check once
        // the wait is out, each wait twice the last, so a shed read is not repeated at once.
        if (this.heldUnder !== keys) return;
        for (const id of ids) this.looked.delete(keyOf({layer, tesseraId: id}));
        if (this.lookPaused === null) {
          this.lookPaused = this.clock.after(this.lookWait, () => (this.lookPaused = null));
          this.lookWait = Math.min(this.lookWait * 2, LOOK_RETRY_MAX_MS);
        }
      }
    }
  }
}

/** Two rows of one artifact from two tiles: the same figures, the bits and parents of either. */
function merged(a: Artifact, b: Artifact): Artifact {
  const or = (x: boolean | null, y: boolean | null) => (x === null && y === null ? null : Boolean(x) || Boolean(y));
  const matched = or(a.matched, b.matched);
  const highlighted = or(a.highlighted, b.highlighted);
  const extra = b.parentIds.filter((p) => !a.parentIds.includes(p));
  if (matched === a.matched && highlighted === a.highlighted && extra.length === 0) return a;
  const parentIds = extra.length === 0 ? a.parentIds : [...a.parentIds, ...extra].sort((x, y) => (x < y ? -1 : x > y ? 1 : 0));
  return {...a, matched, highlighted, parentIds};
}

/**
 * The hierarchy among the artifacts a view is served, built from their `parentIds`, as the store's
 * `artifacts` projection holds it in `lineage`. A parent is named only where it was served beside
 * the child, so an artifact whose parent was withheld has no parent here and is a root. On a `dag`
 * layer a child is listed under every served parent. The lineage changes as the map moves and as
 * the artifact budget cuts the hierarchy.
 *
 * @category Projections
 */
export type ServedLineage = {
  /** Every served artifact, by `tesseraId`. */
  byId: Map<bigint, Artifact>;
  /**
   * A parent's served children, by the parent's `tesseraId`; on a `dag` layer one child may be
   * under several. Absent where none was served.
   */
  childrenOf: Map<bigint, Artifact[]>;
  /** The served artifacts with no served parent, where a walk of the hierarchy starts. */
  roots: Artifact[];
  /** Whether any parent link resolved. `false` both for a flat layer and for a tree cut to one level. */
  linked: boolean;
};

/** @internal */
export function servedLineage(artifacts: readonly Artifact[]): ServedLineage {
  const byId = new Map(artifacts.map((a) => [a.tesseraId, a]));
  const childrenOf = new Map<bigint, Artifact[]>();
  const roots: Artifact[] = [];
  for (const artifact of artifacts) {
    let linked = false;
    for (const parentId of artifact.parentIds) {
      const parent = byId.get(parentId);
      if (!parent) continue;
      linked = true;
      const siblings = childrenOf.get(parent.tesseraId);
      if (siblings) siblings.push(artifact);
      else childrenOf.set(parent.tesseraId, [artifact]);
    }
    if (!linked) roots.push(artifact);
  }
  return {byId, childrenOf, roots, linked: childrenOf.size > 0};
}

/**
 * The `tesseraId`s of `root` and of every artifact served beneath it, following the parent links
 * in `lineage`. `root` is included even where `lineage` does not hold it. A `dag` layer's cycles
 * end the walk.
 *
 * @category Layers and views
 */
export function subtreeOf(lineage: ServedLineage, root: bigint): Set<bigint> {
  const seen = new Set<bigint>();
  const stack = [root];
  while (stack.length > 0) {
    const id = stack.pop()!;
    if (seen.has(id)) continue;
    seen.add(id);
    for (const child of lineage.childrenOf.get(id) ?? []) stack.push(child.tesseraId);
  }
  return seen;
}
