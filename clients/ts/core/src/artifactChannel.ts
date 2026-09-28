import type {TesseraClient} from './client.js';
import {rectArea} from './rects.js';
import {tileRectOfBbox} from './budget.js';
import {GRID32, rectToRequestBbox} from './coords.js';
import {worldBbox, type Viewport} from './prefetch.js';
import type {ViewState} from './driver.js';
import type {Artifact, ArtifactIdentity, FilterExpr, Layer, Quantisation, ViewportResponse} from './types.js';
import {SessionArtifactTable, type ArtifactRef} from './artifactTable.js';
import {artifactBudgetFor} from './artifactBudget.js';
import {refusalOf} from './presented.js';

/**
 * The annotation channel: which artifacts the current view is served, and each one's masked count.
 *
 * The channel issues its own request per settled view, with `k = 0`: the tile list and the
 * artifacts frame, and no points. Reading artifacts off the point path's responses would be wrong:
 * the replica leaves held tiles out of its requests, a tile left out contributes no artifacts, and
 * clusters would disappear as the cache warmed. The point requests send `layers: []`.
 *
 * The served set is replaced per view. A cluster is served because a member this principal can see
 * falls inside the requested tiles, so a merged served set would show clusters for ground the user
 * has panned away from. The payloads are kept across views in a store: an artifact's key, count,
 * geometry, content and parents depend only on the artifact, the authorised set and the
 * generation, so an artifact seen again is not renamed, recoloured or re-uploaded. `matched`, and
 * `rung` on a treed layer, depend on the request and are taken from each response. The store is
 * dropped when the identity key or the content key changes, and on reset.
 *
 * Fetching:
 *
 * - A scope is a layer at one level over the whole extent. A scope is held whole once an
 *   unfiltered response answered a request covering the whole extent. Only levelled and flat
 *   layers qualify. A treed layer's per-view answer is not a clip of its whole-extent answer,
 *   because `prune_children` and the budget reshape the cut per request.
 * - An unfiltered settled view whose scopes are all held whole is answered locally, by picking the
 *   held artifacts in view.
 * - A filtered view always asks the server, since `matched` is per request and cannot be computed
 *   from held points. Where every scope is held whole it asks for identity rows and resolves them
 *   against the store; a row the store cannot resolve makes it ask once more for full rows.
 * - After a view is served at scopes not held whole, the channel fetches one such scope whole per
 *   idle window. Only the store's drop rules and the declared levels limit this.
 * - Held-whole marks are dropped with the store, so the local pick does not serve an old
 *   generation.
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
  artifacts: Artifact[];
  status: 'idle' | 'loading' | 'shown' | 'refused';
  refusal: {code: string; detail: string} | null;
  /** Incremented whenever `artifacts` is replaced. */
  version: number;
  /**
   * How many payloads the session holds, served now or earlier under the same keys. For
   * instrumentation; what is drawn is `artifacts`.
   */
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

/** How long the view must be still before the artifact request goes out. */
const SETTLE_MS = 200;

/**
 * How long the view must be quiet before a whole-scope fetch goes out. Longer than the settle
 * debounce, so a pause between drags does not start one.
 *
 * @internal
 */
export const PROMOTE_IDLE_MS = 1500;

/**
 * How a layer takes part in fetching. A levelled layer (one that declares levels) or a flat one
 * can be held whole, since the budget does not act on either. A treed layer (parent-linked, no
 * levels) cannot: `prune_children` and the budget reshape its cut per request. Anything else is
 * treated as treed, which only ever asks.
 *
 * @internal
 */
export function scopeKindOf(layer: Pick<Layer, 'hierarchy' | 'levels'>): 'levelled' | 'flat' | 'treed' {
  if (layer.levels.length > 0) return 'levelled';
  if (layer.hierarchy.kind === 'flat') return 'flat';
  return 'treed';
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
  zoom = Math.floor(zoom);
  const declared = layer.levels;
  if (declared.length === 0) return [];
  if (!declared.some((d) => d.zoom !== null)) return declared.map((d) => d.level);
  return declared.filter((d) => d.zoom === null || (d.zoom[0] <= zoom && zoom <= d.zoom[1])).map((d) => d.level);
}

/**
 * The `levels` a request over `layers` names: the union of each levelled layer's declared levels
 * at the camera zoom, or `undefined` where no named layer declares levels.
 *
 * The server's default keys on the request's `zoom`, which is the tile depth the mark budget chose
 * and can be several levels deeper than the camera. On a sparse corpus that selects only the
 * deepest level, and most points then carry no membership. The server applies one list to every
 * named layer and ignores a level a layer does not declare, so the union is safe.
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

/**
 * Whether an artifact is in a locally served view: its box intersects the viewport, or, without a
 * box, its centroid is inside it. One with no geometry is in view, since nothing excludes it.
 * `box` is `[minX, minY, maxX, maxY]` in wire grid units, closed; the viewport is in the same
 * units.
 *
 * @internal
 */
export function artifactInView(
  a: Pick<Artifact, 'box' | 'centroid'>,
  view: {x0: number; y0: number; x1: number; y1: number}
): boolean {
  if (a.box) return a.box[0] <= view.x1 && a.box[2] >= view.x0 && a.box[1] <= view.y1 && a.box[3] >= view.y0;
  if (a.centroid) {
    return a.centroid[0] >= view.x0 && a.centroid[0] <= view.x1 && a.centroid[1] >= view.y0 && a.centroid[1] <= view.y1;
  }
  return true;
}

/** One held payload: the artifact as served, and the ordinal it was named under. */
type HeldArtifact = {artifact: Artifact; ordinal: number};

/** The store's key, `(layer, tessera_id)`: ids are unique within a layer. */
function keyOf(a: {layer: string; tesseraId: bigint}): string {
  return `${a.layer}\u0000${a.tesseraId}`;
}

/** The payload as held: without `matched`, which answers a request's filter. */
function withoutBit(a: Artifact): Artifact {
  return a.matched === null ? a : {...a, matched: null};
}

export type ArtifactChannelOptions = {
  view: string;
  quantisation: Quantisation;
  /** The token to ask with; a rejection is the ask's refusal. */
  token(): Promise<string>;
  /** The depth the map is drawn at, or undefined before the first frame. */
  depth(): number | undefined;
  /**
   * The deployment's `max_tiles_per_request`. The ask's depth is lowered until its tile rectangle
   * fits, since the drawn depth can pair with a wider view than it was drawn for.
   */
  maxTiles?: number;
  onChange(state: ArtifactChannelState): void;
  clock?: ArtifactChannelClock;
  settleMs?: number;
  /** The session table this channel names artifacts in. */
  table?: SessionArtifactTable;
  /**
   * The filter the view is under, or null: the expression the point path sends. A filtered request
   * always goes to the network. Absent, the channel sends no filter.
   */
  filters?: () => FilterExpr | null;
  /**
   * `/v1/meta`'s layer declarations, which classify each layer and carry its declared levels.
   * Without them every layer is treated as treed, so nothing is held whole.
   */
  declarations?: readonly Layer[];
  /** How long the view must be quiet before a promotion goes out. See {@link PROMOTE_IDLE_MS}. */
  promoteIdleMs?: number;
  /**
   * Called with each response's identity key, and the token it was asked under, before anything in
   * the response is held. False drops the response.
   */
  admit?(identityKey: string, token: string): boolean;
};

/** @internal */
export class ArtifactChannel {
  private inFlight: AbortController | null = null;
  private timer: unknown = null;
  private view: {bbox: [number, number, number, number]; depth: number; zoom: number} | null = null;
  private readonly clock: ArtifactChannelClock;
  private readonly settleMs: number;
  private readonly table: SessionArtifactTable | null;
  /**
   * The payload store: one entry per artifact served under the current keys, with the ordinal it
   * was named under. The ordinal is taken when the payload enters and released when it leaves, so
   * an artifact seen again keeps its colour.
   *
   * The store has no cap. A cap in artifacts would mean nothing, since one artifact ranges from
   * tens of bytes to a hull of any size. The store holds only what responses carried and is dropped
   * on a key change or reset, so it is bounded by one layer's population under one content key.
   */
  private held = new Map<string, HeldArtifact>();
  /** The keys the store was filled under; it is dropped when either changes. */
  private heldUnder: {identityKey: string; contentKey: string} | null = null;
  /** The layer declarations by name; empty when the caller supplied none. */
  private readonly declarations: Map<string, Layer>;
  /**
   * Per layer, the levels held whole under {@link heldUnder}'s keys; a flat layer's one scope is
   * level 0. Cleared only with the store.
   */
  private wholeLevels = new Map<string, Set<number>>();
  private promoteTimer: unknown = null;
  private promoting: AbortController | null = null;
  private readonly promoteIdleMs: number;
  private state: ArtifactChannelState = {
    layer: null,
    layers: [],
    artifacts: [],
    status: 'idle',
    refusal: null,
    version: 0,
    held: 0
  };

  constructor(
    private readonly client: TesseraClient,
    private readonly opts: ArtifactChannelOptions
  ) {
    this.clock = opts.clock ?? defaultClock();
    this.settleMs = opts.settleMs ?? SETTLE_MS;
    this.table = opts.table ?? null;
    this.declarations = new Map((opts.declarations ?? []).map((l) => [l.name, l]));
    this.promoteIdleMs = opts.promoteIdleMs ?? PROMOTE_IDLE_MS;
  }

  get current(): ArtifactChannelState {
    return this.state;
  }

  /** Whether a view has been noted; false until a frame has been drawn. */
  get hasView(): boolean {
    return this.view !== null;
  }

  /** Points the channel at one layer, or none; see {@link setLayers}. */
  setLayer(layer: string | null): void {
    this.setLayers(layer ? [layer] : []);
  }

  /**
   * Points the channel at the layers the store asks for: the drawn layers with their closure, and
   * the layer the points are coloured by. Each is named in the request and costs its own pass.
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
    // A promotion is idle work, and the view is no longer idle.
    this.cancelPromotion();
    if (!this.noteView(view, width, height)) return;
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = this.clock.after(this.settleMs, () => {
      this.timer = null;
      void this.request();
    });
  }

  /**
   * Asks now, for this view, as a layer toggle needs. It takes the view because the noted one is
   * absent until a frame has been drawn.
   */
  refresh(view: ViewState, width: number, height: number): void {
    this.cancelPromotion();
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = null;
    if (!this.noteView(view, width, height)) return;
    void this.request();
  }

  /** What to ask for: the visible box, at the depth the map is drawn at. False when there is none. */
  private noteView(view: ViewState, width: number, height: number): boolean {
    // The depth the map is drawn at: which artifacts are served depends on the tiles requested, so
    // another depth would answer for ground the marks do not cover.
    const depth = this.opts.depth();
    if (depth === undefined) return false;
    // The visible box, with no margin. The point path fetches a wider ring, and clusters for it
    // would be off screen.
    const viewport: Viewport = {
      target: [view.target[0], view.target[1]],
      zoom: view.zoom,
      width,
      height
    };
    const bbox = worldBbox(viewport, 1);
    let asked = depth;
    if (this.opts.maxTiles !== undefined) {
      while (asked > 0 && rectArea(tileRectOfBbox(bbox, asked)) > this.opts.maxTiles) asked -= 1;
    }
    this.view = {bbox, depth: asked, zoom: view.zoom};
    return true;
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
    this.cancelPromotion();
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = null;
    this.inFlight?.abort();
    this.inFlight = null;
  }

  /** Cancels a pending promotion and abandons one in flight. */
  private cancelPromotion(): void {
    if (this.promoteTimer) this.clock.cancel(this.promoteTimer);
    this.promoteTimer = null;
    this.promoting?.abort();
    this.promoting = null;
  }

  /** Drops the store and its held-whole marks, releasing every ordinal it held. */
  private dropHeld(): void {
    if (this.table && this.held.size > 0) {
      const ordinals = new Uint32Array(this.held.size);
      let i = 0;
      for (const entry of this.held.values()) ordinals[i++] = entry.ordinal;
      this.table.release(ordinals);
    }
    this.held.clear();
    this.heldUnder = null;
    this.wholeLevels.clear();
  }

  /** Whether `(layer, level)` is held whole under the store's current keys. */
  isHeldWhole(layer: string, level: number): boolean {
    return this.wholeLevels.get(layer)?.has(level) ?? false;
  }

  /**
   * A content key another channel observed; the point path carries one on every response. While
   * the local pick answers views without a request, this is how the channel learns of a new
   * generation. On a change the store and marks are dropped and the noted view is asked again.
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

  /**
   * Takes a response's served set into the store and the session table, and returns the artifacts
   * to draw: each held payload with this response's `matched` and `rung`. The held object itself
   * is returned where those agree, so a consumer memoising on it does no work.
   */
  private hold(artifacts: readonly Artifact[], identityKey: string, contentKey: string): Artifact[] {
    // A new content key means what is held may be out of date. A new identity key is another
    // principal's answer, which must not be shown.
    if (this.heldUnder && (this.heldUnder.identityKey !== identityKey || this.heldUnder.contentKey !== contentKey)) {
      this.dropHeld();
    }
    this.heldUnder = {identityKey, contentKey};

    // Named in one batch: `take` names every entry before it sets links, so a child can link to a
    // parent that is new in the same response.
    const novel = artifacts.filter((a) => !this.held.has(keyOf(a)));
    const ordinals = this.table
      ? this.table.take(
          novel.map((a) => ({
            tesseraId: a.tesseraId,
            layer: a.layer,
            parentIds: a.parentIds,
            centroid: a.centroid,
            rung: a.rung
          }))
        )
      : new Uint32Array(novel.length);
    for (let i = 0; i < novel.length; i++) {
      const a = novel[i]!;
      this.held.set(keyOf(a), {artifact: withoutBit(a), ordinal: ordinals[i]!});
    }

    return artifacts.map((a) => {
      const {artifact} = this.held.get(keyOf(a))!;
      // On a treed layer `rung` is the artifact's depth in this response's cut, so a re-served
      // artifact takes this response's value.
      return a.matched === artifact.matched && a.rung === artifact.rung ? artifact : {...artifact, rung: a.rung, matched: a.matched};
    });
  }

  /**
   * Resolves identity rows against the store by `(layer, tessera_id)`, taking `rung`, `matched`
   * and `highlighted` from each row. Null where a row does not resolve or the response's keys are
   * not the store's; the caller then asks again for full rows.
   */
  private resolveIdentity(rows: readonly ArtifactIdentity[], response: ViewportResponse): Artifact[] | null {
    if (!this.heldUnder || this.heldUnder.identityKey !== response.identityKey || this.heldUnder.contentKey !== response.contentKey) {
      return null;
    }
    const out: Artifact[] = [];
    for (const row of rows) {
      const held = this.held.get(keyOf(row));
      if (!held) return null;
      const {artifact} = held;
      const same = row.matched === artifact.matched && row.highlighted === artifact.highlighted && row.rung === artifact.rung;
      out.push(same ? artifact : {...artifact, rung: row.rung, matched: row.matched, highlighted: row.highlighted});
    }
    return out;
  }

  /** Whether the view's request covers every tile at its depth. */
  private static isWholeExtent(view: {bbox: [number, number, number, number]; depth: number; zoom: number}): boolean {
    const r = tileRectOfBbox(view.bbox, view.depth);
    const edge = 2 ** view.depth - 1;
    return r.x0 === 0 && r.y0 === 0 && r.x1 === edge && r.y1 === edge;
  }

  /**
   * The levels a view's request is answered at for one layer, or null where the layer cannot be
   * held whole: a treed layer, or one with no declaration. A flat layer is level 0; a levelled one
   * follows its declared levels at the camera zoom, as {@link requestLevels} does.
   */
  private scopeLevels(layer: string, zoom: number): number[] | null {
    const decl = this.declarations.get(layer);
    if (!decl) return null;
    const kind = scopeKindOf(decl);
    if (kind === 'treed') return null;
    return kind === 'flat' ? [0] : declaredLevelsAt(decl, zoom);
  }

  /** Whether every scope the view touches is held whole, the condition for answering locally. */
  private servesWhole(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number; zoom: number}): boolean {
    if (!this.heldUnder) return false;
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.zoom);
      if (!levels) return false;
      for (const level of levels) if (!this.isHeldWhole(layer, level)) return false;
    }
    return true;
  }

  /**
   * The served set picked from the store: artifacts at an answered level whose geometry meets the
   * tile-aligned box the request would have asked over. This can draw the edge of a shape whose
   * visible members are off screen; its geometry covers the whole visible membership.
   */
  private pickLocal(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number; zoom: number}): Artifact[] {
    const r = tileRectOfBbox(view.bbox, view.depth);
    const span = GRID32 / 2 ** view.depth;
    const box = {x0: r.x0 * span, y0: r.y0 * span, x1: (r.x1 + 1) * span, y1: (r.y1 + 1) * span};
    const wanted = new Map<string, Set<number>>();
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.zoom);
      if (levels) wanted.set(layer, new Set(levels));
    }
    const out: Artifact[] = [];
    for (const {artifact} of this.held.values()) {
      if (!wanted.get(artifact.layer)?.has(artifact.rung)) continue;
      if (artifactInView(artifact, box)) out.push(artifact);
    }
    return out;
  }

  /** Marks every scope a whole-extent response answered as held whole. */
  private markWholeExtent(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number; zoom: number}): void {
    if (!ArtifactChannel.isWholeExtent(view)) return;
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.zoom);
      if (!levels) continue;
      for (const level of levels) this.markWhole(layer, level);
    }
  }

  /** Marks `(layer, level)` held whole under the store's current keys. */
  private markWhole(layer: string, level: number): void {
    let levels = this.wholeLevels.get(layer);
    if (!levels) {
      levels = new Set();
      this.wholeLevels.set(layer, levels);
    }
    levels.add(level);
  }

  /** The scopes the declared levels name for the current view that are not yet held whole. */
  private promotionCandidates(): {layer: string; level: number; flat: boolean}[] {
    if (!this.view) return [];
    const out: {layer: string; level: number; flat: boolean}[] = [];
    for (const layer of this.state.layers) {
      const decl = this.declarations.get(layer);
      if (!decl) continue;
      const kind = scopeKindOf(decl);
      if (kind === 'treed') continue;
      const levels = kind === 'flat' ? [0] : declaredLevelsAt(decl, this.view.zoom);
      for (const level of levels) {
        if (this.isHeldWhole(layer, level)) continue;
        out.push({layer, level, flat: kind === 'flat'});
      }
    }
    return out;
  }

  /** Arms the idle promotion where there is something to promote. Interaction disarms it. */
  private schedulePromotion(): void {
    if (this.promoteTimer) this.clock.cancel(this.promoteTimer);
    this.promoteTimer = null;
    if (this.promotionCandidates().length === 0) return;
    this.promoteTimer = this.clock.after(this.promoteIdleMs, () => {
      this.promoteTimer = null;
      void this.promote();
    });
  }

  /**
   * Fetches one scope whole: the whole extent, the level named (omitted for a flat layer), no
   * filter and no `artifact_budget`, which does not act on levelled or flat layers. The response
   * fills the store and the marks and leaves the served set alone, so it does not redraw the map. A
   * response under keys other than the store's is discarded; a key change is for the per-view path
   * to observe.
   */
  private async promote(): Promise<void> {
    if (this.inFlight) return;
    const candidate = this.promotionCandidates()[0];
    if (!candidate) return;
    const signal = new AbortController();
    this.promoting?.abort();
    this.promoting = signal;
    try {
      const token = await this.opts.token();
      if (this.promoting !== signal) return;
      const response = await this.client.viewport(
        token,
        {
          view: this.opts.view,
          zoom: 0,
          bbox: rectToRequestBbox({x0: 0, y0: 0, x1: 0, y1: 0}, 0, this.opts.quantisation),
          k: 0,
          layers: [candidate.layer],
          ...(candidate.flat ? {} : {levels: [candidate.level]}),
          // Centroid and box only, as the per-view request asks; the one hull drawn is fetched by id.
          computed: ['centroid', 'box']
        },
        signal.signal
      );
      if (this.promoting !== signal) return;
      this.promoting = null;
      if (this.opts.admit?.(response.identityKey, token) === false) return;
      if (
        this.heldUnder &&
        (this.heldUnder.identityKey !== response.identityKey || this.heldUnder.contentKey !== response.contentKey)
      ) {
        return;
      }
      this.hold(response.result.artifacts, response.identityKey, response.contentKey);
      this.markWhole(candidate.layer, candidate.level);
      this.state = {...this.state, held: this.held.size};
      this.emit();
      // One scope per idle window; the next waits for its own.
      this.schedulePromotion();
    } catch {
      if (this.promoting === signal) this.promoting = null;
      // The per-view path is unaffected, and the next served view arms another promotion.
    }
  }

  private async request(): Promise<void> {
    const view = this.view;
    const layers = this.state.layers;
    if (!view) return;
    this.inFlight?.abort();
    if (layers.length === 0) {
      this.inFlight = null;
      // No layer: the served set is cleared and the store kept, so turning a layer back on draws
      // without a refetch.
      this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1};
      this.emit();
      return;
    }

    // A filter always asks the server. The held points are a sample of the matches, so a bit
    // derived from them would be false for the small clusters a filter is used to find.
    const filterExpr = this.opts.filters?.() ?? null;
    if (!filterExpr && this.servesWhole(layers, view)) {
      this.inFlight = null;
      this.state = {
        ...this.state,
        artifacts: this.pickLocal(layers, view),
        status: 'shown',
        refusal: null,
        version: this.state.version + 1,
        held: this.held.size
      };
      this.emit();
      return;
    }

    // A filtered view over scopes all held whole asks for identity rows: the same rows, with only
    // the columns a filter moves.
    const identityAsk = filterExpr !== null && this.servesWhole(layers, view);

    const signal = new AbortController();
    this.inFlight = signal;
    this.state = {...this.state, status: 'loading'};
    this.emit();
    const ask = (token: string, rows: 'identity' | null) =>
      this.client.viewport(
        token,
        {
          view: this.opts.view,
          zoom: view.depth,
          bbox: rectToRequestBbox(tileRectOfBbox(view.bbox, view.depth), view.depth, this.opts.quantisation),
          // No points: this channel draws none.
          k: 0,
          layers,
          // Levels from the camera zoom; see `requestLevels`.
          ...(requestLevels(this.declarations, layers, view.zoom) !== undefined ? {levels: requestLevels(this.declarations, layers, view.zoom)} : {}),
          // Centroid and box, not the hull. A hull is derived per artifact per request, and a
          // settled view carries a couple of hundred artifacts while the map draws one hull; that
          // one is fetched by id (`needHull` in store.ts).
          computed: ['centroid', 'box'],
          // A budgeted cut: coarse ancestors at the overview, refined as the zoom deepens.
          artifactBudget: artifactBudgetFor(view.zoom),
          ...(filterExpr ? {filters: filterExpr} : {}),
          ...(rows ? {artifactRows: rows} : {})
        },
        signal.signal
      );
    try {
      const token = await this.opts.token();
      if (this.inFlight !== signal) return;
      let response = await ask(token, identityAsk ? 'identity' : null);
      if (this.inFlight !== signal) return;
      if (this.opts.admit?.(response.identityKey, token) === false) return;
      let drawn: Artifact[] | null = null;
      const identityRows = response.result.artifactsIdentity;
      if (identityRows !== null) drawn = this.resolveIdentity(identityRows, response);
      if (drawn === null) {
        // An identity answer the store cannot resolve (a new generation, a payload never held)
        // falls back once to a full ask for the same view, under the same abort signal.
        if (identityRows !== null) {
          response = await ask(token, null);
          if (this.inFlight !== signal) return;
          if (this.opts.admit?.(response.identityKey, token) === false) return;
        }
        drawn = this.hold(response.result.artifacts, response.identityKey, response.contentKey);
        // Only an unfiltered response marks a scope held whole: a filtered response's rows answer a
        // narrower question.
        if (!filterExpr) this.markWholeExtent(layers, view);
      }
      this.inFlight = null;
      this.state = {
        ...this.state,
        artifacts: drawn,
        status: 'shown',
        refusal: null,
        version: this.state.version + 1,
        held: this.held.size
      };
      this.emit();
      this.schedulePromotion();
    } catch (error) {
      if (signal.signal.aborted || this.inFlight !== signal) return;
      this.inFlight = null;
      // A refusal drops the served set, which answered a superseded request. The store is kept: a
      // failed request says nothing about whether it is still true.
      this.state = {
        ...this.state,
        artifacts: [],
        status: 'refused',
        refusal: refusalOf(error),
        version: this.state.version + 1
      };
      this.emit();
    }
  }
}

/**
 * The hierarchy among the artifacts a response served, built from their `parentIds`, as the
 * store's `artifacts` projection holds it in `lineage`. A parent is named only where the same
 * response served it, so an artifact whose parent was withheld has no parent here and is a root.
 * On a `dag` layer a child is listed under every served parent. The lineage changes as the map
 * moves and as the artifact budget cuts the hierarchy.
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
