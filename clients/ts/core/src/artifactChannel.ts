import type {TesseraClient} from './client.js';
import {rectArea} from './rects.js';
import {tileRectOfBbox} from './budget.js';
import {GRID32, rectToRequestBbox} from './coords.js';
import {worldBbox, type Viewport} from './prefetch.js';
import type {ViewState} from './driver.js';
import type {Artifact, ArtifactIdentity, FilterExpr, Layer, Quantisation, ViewportResponse} from './types.js';
import {SessionArtifactTable, type ArtifactRef} from './artifactTable.js';
import {artifactBudgetFor} from './artifactBudget.js';

/**
 * The annotation channel: which artifacts the current view is served, and what each one's masked
 * count is.
 *
 * ## It asks for itself, rather than riding the point path
 *
 * The obvious construction — read `artifacts` off the viewport responses the replica is already
 * fetching — is wrong here, and quietly so. **The replica elides tiles it already holds**, and an
 * elided tile is not in the request, so it contributes no artifacts to the response either. A
 * cluster's presence therefore depends on whether its ground happened to be novel, and the map
 * would *lose* clusters as the cache warmed — the worst kind of bug, because the cache working is
 * what makes it appear.
 *
 * So the point requests declare `layers: []` (which costs the server nothing) and this issues one
 * request per settled view with `k = 0`: the tile stream, the artifacts frame, and not a single
 * point. What it costs is the counting stage over the view's tiles, which is the same work the
 * replica's own revalidation pays.
 *
 * ## What it does not do
 *
 * **The served set is replaced wholesale and the payloads are not, and the two must not be
 * confused.** A cluster is served because a member visible to this principal falls inside the
 * requested tiles, so a *served set* that merged responses would show clusters for ground the user
 * has panned away from — presented, inevitably, as though they were in view. What accumulates is
 * the **payload store** beside it: an artifact's key, count, geometry, content and parent
 * are a function of `(artifact, M_auth, generation)` and of nothing in the request
 * (`artifact-cache-handover.md` §2), so an artifact panned away from and back is the same answer
 * and is not renamed, recoloured or re-uploaded. Only `matched` moves — and, on a treed layer,
 * the response-local `rung` beside it — and both are taken from the response every time
 * (decision 0104; contracts §3.2 r43).
 *
 * The store goes when the identity key or the content key it was filled under rotates — rule 7 of
 * `client-obligations.md`, which is exactly *is what you hold still true*.
 *
 * ## The fetch model (`artifact-fetch-protocol.md` §6) — policy, not obligation
 *
 * **Hold a scope whole where observation says it is cheap; pick in-view locally; ask per view
 * everywhere else.** A scope is a layer at a level over the whole extent. The rule that sanctions
 * the local pick is the protocol's §4: rows come from a response, or from a scope held whole —
 * never assembled from partial history. Concretely:
 *
 * - **A scope is marked held-whole** when an unfiltered response answered a request whose bbox
 *   covered the whole extent — a whole-extent opening view marks its levels for free. Only a
 *   levelled layer (per declared level) or a flat one qualifies; a **treed** layer (parent-linked,
 *   no declared levels) is never marked, because its per-view answer is not a clip of its
 *   whole-extent answer — `prune_children` and the budget re-shape the cut per request (protocol
 *   §4), so treed layers ask per view always.
 * - **A settled, unfiltered view over scopes all held whole is served locally**: an artifact is in
 *   view when its box (or, failing one, its centroid) intersects the viewport, and one with no
 *   geometry at all is always in view. The served set is still replaced wholesale per view. The
 *   pick occasionally draws the edge of a shape whose visible members lie off screen — a boundary
 *   that ought to be drawn, the geometry being over the whole visible membership (settled by the
 *   owner; `artifact-cache-handover.md` §4).
 * - **A filter always asks the server.** The bit is per request (decision 0104) and cannot be
 *   computed from held points — the sample-as-set error — so a filtered view goes to the network
 *   whatever is held. Where every applicable scope IS held whole, the ask is for **identity rows**
 *   (`artifact_rows: "identity"`, protocol §5.2): the same row set in four columns, resolved
 *   against the held payloads by `(layer, tessera_id)` with the response's `rung` and `matched` —
 *   the bit always from the response, never the store. A row the store cannot resolve is the
 *   projection's self-detected misuse: the view falls back once to a full ask — one round trip,
 *   never a wrong map — and a filtered response never marks anything held-whole.
 * - **Idle promotion is a ratchet with no gate** (the owner's ruling, 2026-08-28): after a settled
 *   view is served at scopes not held whole, the channel fetches one whole per idle window —
 *   whole-extent bbox, the level named, no budget (the budget is inert on levelled and flat layers
 *   by contract). A candidate is every scope the declared map names for the current view — a
 *   levelled layer per declared level at the view's depth, a flat layer as level 0, never a treed
 *   one — that is not yet held whole. Nothing observes sizes and nothing stops: the bound is the
 *   drop rules (rule 7 — key rotation, and reset), exactly as it is for the store itself, and the
 *   declared map is what limits what a view names in the first place.
 * - **Held-whole marks drop exactly when the store drops** — key rotation, and reset. A mark must
 *   never outlive the store, or the local pick would serve a stale generation as current.
 *
 * **No retry, and no reason for an absence.** A cluster below its layer's existence criterion, one
 * whose layer this principal cannot reach, one suppressed and one that never existed are the same
 * answer: nothing. There is nothing on the wire to distinguish them and nothing here that tries.
 *
 * **Headless.** The debounce clock is injected — `setTimeout`'s shape — so the channel is testable
 * in node with a fake, like the driver and the presenter. The default is `setTimeout`.
 */

/** The session's held artifacts, and the channel's state — what a projection is built from. */
export type ArtifactChannelState = {
  /** The first layer on — the one a single-layer reader keeps naming. */
  layer: string | null;
  /** Every layer on, with the closure the store named (decision 0096). */
  layers: string[];
  artifacts: Artifact[];
  status: 'idle' | 'loading' | 'shown' | 'refused';
  refusal: {code: string; detail: string} | null;
  /** Bumped whenever `artifacts` is replaced — a paint key that a count of held would miss. */
  version: number;
  /**
   * How many payloads the session holds — the served set plus everything served earlier under the
   * same keys. Instrumentation and nothing else: what is drawn is `artifacts`.
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
 * How long the view must have been quiet before a whole-scope promotion goes out — well past the
 * settle debounce, so a pause inside a sequence of drags does not spend a whole-rung fetch on a
 * view the user is already leaving, and short enough that the first genuine dwell buys the hold.
 */
export const PROMOTE_IDLE_MS = 1500;

/**
 * How a layer participates in the fetch model. **Levelled** (declares levels — the stacked and
 * tiered kinds) and **flat** layers may be held whole: the budget is inert on both by contract, so
 * a whole-extent answer is the whole scope. **Treed** (parent-linked, declares no levels) may not:
 * `prune_children` and the budget re-shape its cut per request, so its per-view answer is not a
 * clip of its whole-extent one (protocol §4 records the failure). A declaration this module does
 * not recognise is treated as treed, which is the conservative direction — it only ever asks.
 */
export function scopeKindOf(layer: Pick<Layer, 'hierarchy' | 'levels'>): 'levelled' | 'flat' | 'treed' {
  if (layer.levels.length > 0) return 'levelled';
  if (layer.hierarchy.kind === 'flat') return 'flat';
  return 'treed';
}

/**
 * The levels a request naming no `levels` is answered at, for one layer at one asked depth — the
 * client-side mirror of the server's declared-map default (decision 0103, `viewport.rs`'s
 * `Declared` arm): no level declaring a zoom range means every level answers; otherwise a level
 * answers when its range covers the depth inclusively, and a range-less level among ranged ones
 * answers at every depth.
 */
export function declaredLevelsAt(layer: Pick<Layer, 'levels'>, zoom: number): number[] {
  const declared = layer.levels;
  if (declared.length === 0) return [];
  if (!declared.some((d) => d.zoom !== null)) return declared.map((d) => d.level);
  return declared.filter((d) => d.zoom === null || (d.zoom[0] <= zoom && zoom <= d.zoom[1])).map((d) => d.level);
}

/**
 * Whether an artifact belongs to a locally served view (protocol §6): its box intersects the
 * viewport; failing a box, its centroid falls inside it; and one carrying no geometry at all is
 * always in view — there is nothing to exclude it by, and the server's own intersection test is a
 * fetch bound rather than an assertion. `box` is `[minX, minY, maxX, maxY]` in wire grid units,
 * closed; the viewport arrives in the same units.
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

/**
 * **There is no cap on what the store holds, and the drop rules are the whole bound.**
 *
 * A cap in *artifacts* cannot mean anything: bytes per artifact span orders of magnitude — a
 * count-only artifact is tens of bytes and one carrying a hull is a ring per separated group of its
 * visible members, unbounded — so a number of artifacts is not a number of megabytes and choosing
 * one is choosing a figure that looks like a budget without being one. A cap in bytes would mean
 * something and is not built; if one is ever wanted, the client has already parsed the rings and
 * can total them at insert.
 *
 * What bounds this instead is that the payloads are what the response already carried, parsed into
 * the objects this holds — nothing is materialised that a response did not — and that the store
 * goes whole when the identity key or the content key rotates, on a reset, and on a refresh. So it
 * is bounded by one layer's population under one content key.
 *
 * **The growth case, stated rather than hidden**: a layer whose artifacts the tile index *can*
 * bound is served a viewport at a time, so a session panning across a very large one accumulates
 * towards that layer's whole population. That is the same total a single request over the whole
 * extent would have returned, reached slowly.
 */

/** One held payload: the artifact as served, and the ordinal it was named under. */
type HeldArtifact = {artifact: Artifact; ordinal: number};

/** The store's key. Identity is `(layer, tessera_id)`: ids are unique per layer, not across. */
function keyOf(a: {layer: string; tesseraId: bigint}): string {
  return `${a.layer}\u0000${a.tesseraId}`;
}

/**
 * The payload without the filter bit — what is held.
 *
 * A held artifact must never carry a `matched` from the request that fetched it: that is the one
 * field a filter moves, and holding it would answer this filter's question with the last one's.
 */
function withoutBit(a: Artifact): Artifact {
  return a.matched === null ? a : {...a, matched: null};
}

export type ArtifactChannelOptions = {
  view: string;
  quantisation: Quantisation;
  /** How to authorise the request — the store watches the token, so it hands one in per ask. */
  token(): string | null;
  /** The depth the map is drawn at, or undefined before the first frame. */
  depth(): number | undefined;
  /**
   * The deployment's `max_tiles_per_request`. The ask is clamped to a depth whose rect stays under
   * it — a drawn depth paired with a wider view than the one it was drawn for (a camera move
   * landing between a frame and its settle) otherwise asks for a million tiles and is refused.
   */
  maxTiles?: number;
  onChange(state: ArtifactChannelState): void;
  clock?: ArtifactChannelClock;
  settleMs?: number;
  /** The table this channel feeds — its served set is the only holder until D12 (§5.10). */
  table?: SessionArtifactTable;
  /**
   * The filter expression the view is under, or null for the unfiltered view — the same
   * composition the point path sends, handed in as a supplier because the channel asks per settle.
   * A filtered request carries it and always goes to the network: the bit is per request
   * (decision 0104) and the local pick is sanctioned only for the question a held scope answers
   * whole, which a filter is not. Absent means the channel never sends a filter.
   */
  filters?: () => FilterExpr | null;
  /**
   * `/v1/meta`'s layer declarations — what classifies a layer as levelled, flat or treed and
   * carries the declared zoom→level map the server's absent-`levels` default follows. Without
   * them nothing is ever held whole or promoted: an unclassifiable layer is treated as treed,
   * which only ever asks, so the channel without this option behaves exactly as it did before
   * the fetch model existed.
   */
  declarations?: readonly Layer[];
  /** How long the view must be quiet before a promotion goes out. See {@link PROMOTE_IDLE_MS}. */
  promoteIdleMs?: number;
};

export class ArtifactChannel {
  private inFlight: AbortController | null = null;
  private timer: unknown = null;
  private view: {bbox: [number, number, number, number]; depth: number; zoom: number} | null = null;
  private readonly clock: ArtifactChannelClock;
  private readonly settleMs: number;
  private readonly table: SessionArtifactTable | null;
  /**
   * The payload store: one entry per artifact this session has been served under the current keys,
   * with the ordinal it was named under and the response version it was last served in.
   *
   * The ordinal reference is taken **once, when the payload enters**, and released when it leaves —
   * so an artifact panned away from and back keeps its ordinal, and the session table's colours and
   * the lookup texture built from it do not move (§5.10).
   */
  private held = new Map<string, HeldArtifact>();
  /** The keys the store was filled under. Rule 7: it goes when either rotates. */
  private heldUnder: {identityKey: string; contentKey: string} | null = null;
  /** The layer declarations by name — empty when the caller supplied none. */
  private readonly declarations: Map<string, Layer>;
  /**
   * The held-whole marks: layer → the levels held whole under {@link heldUnder}'s keys (a flat
   * layer is its single level-0 scope). Cleared with the store, and only with it — a mark that
   * outlived the store would let the local pick serve a stale generation as current.
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

  /** Whether a view has been noted — false until a drawn frame gave `noteView` a depth. */
  get hasView(): boolean {
    return this.view !== null;
  }

  /** Point the channel at one layer — {@link setLayers} with one name, or none. */
  setLayer(layer: string | null): void {
    this.setLayers(layer ? [layer] : []);
  }

  /**
   * Point the channel at the layers that are on — a closure, usually one layer with its
   * dependents (decision 0096). Every one is named in the request and each costs its own pass.
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
   * Note the view and ask once it settles.
   *
   * Debounced rather than issued per frame: the artifact set changes only when the view crosses a
   * tile boundary, and a request per pointer-move would put the server's counting stage inside the
   * gesture. Long enough to swallow a drag, short enough that the counts land as the hand stops.
   */
  schedule(view: ViewState, width: number, height: number): void {
    // Interaction: a pending promotion is idle work, and the view is no longer idle.
    this.cancelPromotion();
    if (!this.noteView(view, width, height)) return;
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = this.clock.after(this.settleMs, () => {
      this.timer = null;
      void this.request();
    });
  }

  /**
   * Ask now, for this view — a layer toggle, where the view has not moved and nothing else will
   * ask. It takes the view rather than reusing the noted one, because the noted one may be absent:
   * the channel notes a view only once a frame has been drawn to annotate.
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
    // **The depth the map is actually drawn at**, not one chosen here: which artifacts a view is
    // served depends on the tiles requested, so asking at a different depth from the one on screen
    // would answer for ground the marks do not cover. Before the first frame there is no such
    // depth, and nothing is drawn to annotate.
    const depth = this.opts.depth();
    if (depth === undefined) return false;
    // The *visible* box, with no margin: what a viewer is looking at is what the clusters should
    // answer for. The point path fetches a wider ring so a small pan costs no request, and
    // borrowing that box here would put clusters on screen for ground the user cannot see.
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

  /** Drop what is held and abandon anything in flight: a new principal, or a dataset switch. */
  reset(): void {
    this.cancel();
    this.dropHeld();
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

  /** Cancel the pending promotion and abandon one in flight — idle work, cheap to re-derive. */
  private cancelPromotion(): void {
    if (this.promoteTimer) this.clock.cancel(this.promoteTimer);
    this.promoteTimer = null;
    this.promoting?.abort();
    this.promoting = null;
  }

  /** Drop the whole store, releasing every ordinal it held. The held-whole marks go with it. */
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
   * The content key another channel observed — the point path carries it on every response, so a
   * client learns of a rotation without this channel asking anything (`artifact-cache-handover.md`
   * §4a.3). While the local pick answers views with no request of its own, this is the only route
   * by which rule 7 can fire; on a rotation the store and its marks go, and the noted view is
   * re-asked so the drawn set does not sit stale until the next gesture.
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
   * Take this response's served set into the store and the session table, and return the artifacts
   * to draw — the **held payload** for each, wearing this response's `matched`.
   *
   * **The payload is held and the bit is not.** Key, count, geometry, content and parent are a
   * function of `(artifact, M_auth, generation)`, which the two keys below pin exactly; `matched`
   * is a function of the request's filter as well, so it is read from the response every time
   * (decision 0104), and `rung` rides with it — response-local on a treed layer, unmoving on the
   * others (contracts §3.2 r43). Returning the held object where the bit agrees is what keeps a re-served
   * artifact referentially identical, so a consumer memoising on it does no work.
   */
  private hold(artifacts: readonly Artifact[], identityKey: string, contentKey: string): Artifact[] {
    // **Rule 7, at the one place that can enforce it.** A rotated content key is exactly *what you
    // hold may no longer be true*, and a changed identity key is a different principal's answer —
    // which must never be shown, and is a disclosure rather than a staleness bug (decision 0029).
    if (this.heldUnder && (this.heldUnder.identityKey !== identityKey || this.heldUnder.contentKey !== contentKey)) {
      this.dropHeld();
    }
    this.heldUnder = {identityKey, contentKey};

    // Named in one batch, so a parent link between two artifacts of this response resolves — the
    // table sets links only within the batch it is given.
    const novel = artifacts.filter((a) => !this.held.has(keyOf(a)));
    const ordinals = this.table
      ? this.table.take(
          novel.map((a) => ({
            tesseraId: a.tesseraId,
            layer: a.layer,
            parentId: a.parentId,
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
      // The response's own `rung` beside its bit: on a treed layer the rung is response-local —
      // the depth in the forest this cut's `parent_id` links form (contracts §3.2 r43) — so a
      // re-served artifact wears this response's number, not the one it was first held under.
      // Levelled and flat rungs never move, so the held object comes back unchanged there.
      return a.matched === artifact.matched && a.rung === artifact.rung ? artifact : {...artifact, rung: a.rung, matched: a.matched};
    });
  }

  /**
   * The identity projection's rows, resolved against the payload store by `(layer, tessera_id)` —
   * the drawn artifact is the **held payload wearing this response's `rung` and `matched`**, the
   * bit always from the response and never the store (decision 0104). Null where any row fails to
   * resolve, and where the response's keys are not the store's — a held payload under a rotated
   * key is another generation's answer, which the projection must never dress as this one's.
   * A null is §5.2's self-detected misuse, and the caller re-asks with full rows: one round trip,
   * never a wrong map.
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
      out.push(row.matched === artifact.matched && row.rung === artifact.rung ? artifact : {...artifact, rung: row.rung, matched: row.matched});
    }
    return out;
  }

  /** Whether the request this view produces covers the whole extent — every tile at its depth. */
  private static isWholeExtent(view: {bbox: [number, number, number, number]; depth: number}): boolean {
    const r = tileRectOfBbox(view.bbox, view.depth);
    const edge = 2 ** view.depth - 1;
    return r.x0 === 0 && r.y0 === 0 && r.x1 === edge && r.y1 === edge;
  }

  /**
   * The levels the view's request would be answered at for one layer, or null where the layer can
   * never be held whole — a treed layer, and any layer this channel holds no declaration for.
   * A flat layer is its single level-0 scope; a levelled one follows the declared map at the
   * request's own depth, exactly as the server's absent-`levels` default does.
   */
  private scopeLevels(layer: string, depth: number): number[] | null {
    const decl = this.declarations.get(layer);
    if (!decl) return null;
    const kind = scopeKindOf(decl);
    if (kind === 'treed') return null;
    return kind === 'flat' ? [0] : declaredLevelsAt(decl, depth);
  }

  /**
   * Whether every scope this view touches is held whole — the condition under which the local pick
   * is sanctioned (protocol §4: rows come from a response, or from a scope held whole).
   */
  private servesWhole(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number}): boolean {
    if (!this.heldUnder) return false;
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.depth);
      if (!levels) return false;
      for (const level of levels) if (!this.isHeldWhole(layer, level)) return false;
    }
    return true;
  }

  /**
   * The served set built locally: what the view's request would have named, picked from the whole
   * hold — an artifact at an answered level whose geometry intersects the viewport, the viewport
   * being the same tile-quantised box the request would have asked over. Occasionally that draws
   * the edge of a shape whose visible members lie off screen, which is a boundary that ought to be
   * drawn (the geometry describes the whole visible membership, never the part in view).
   */
  private pickLocal(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number}): Artifact[] {
    const r = tileRectOfBbox(view.bbox, view.depth);
    const span = GRID32 / 2 ** view.depth;
    const box = {x0: r.x0 * span, y0: r.y0 * span, x1: (r.x1 + 1) * span, y1: (r.y1 + 1) * span};
    const wanted = new Map<string, Set<number>>();
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.depth);
      if (levels) wanted.set(layer, new Set(levels));
    }
    const out: Artifact[] = [];
    for (const {artifact} of this.held.values()) {
      if (!wanted.get(artifact.layer)?.has(artifact.rung)) continue;
      if (artifactInView(artifact, box)) out.push(artifact);
    }
    return out;
  }

  /**
   * A whole-extent, unfiltered response marks every scope it answered held whole — a whole-extent
   * opening view marks its levels for free. A partial response marks nothing, and observes
   * nothing either: there is no cardinality hint on this surface (S5, declined), and the ratchet
   * no longer asks for one — its bound is the drop rules, not a size.
   */
  private markWholeExtent(layers: readonly string[], view: {bbox: [number, number, number, number]; depth: number}): void {
    if (!ArtifactChannel.isWholeExtent(view)) return;
    for (const layer of layers) {
      const levels = this.scopeLevels(layer, view.depth);
      if (!levels) continue;
      for (const level of levels) this.markWhole(layer, level);
    }
  }

  /** Mark `(layer, level)` held whole under the store's current keys. */
  private markWhole(layer: string, level: number): void {
    let levels = this.wholeLevels.get(layer);
    if (!levels) {
      levels = new Set();
      this.wholeLevels.set(layer, levels);
    }
    levels.add(level);
  }

  /**
   * The scopes the declared map names for the current view that are not yet held whole — a
   * levelled layer per declared level at the view's depth, a flat layer as level 0, never a treed
   * one. Every one is a candidate: nothing about a scope's size is known or asked before the
   * fetch, and the declared map is what bounds the list.
   */
  private promotionCandidates(): {layer: string; level: number; flat: boolean}[] {
    if (!this.view) return [];
    const out: {layer: string; level: number; flat: boolean}[] = [];
    for (const layer of this.state.layers) {
      const decl = this.declarations.get(layer);
      if (!decl) continue;
      const kind = scopeKindOf(decl);
      if (kind === 'treed') continue;
      const levels = kind === 'flat' ? [0] : declaredLevelsAt(decl, this.view.depth);
      for (const level of levels) {
        if (this.isHeldWhole(layer, level)) continue;
        out.push({layer, level, flat: kind === 'flat'});
      }
    }
    return out;
  }

  /** Arm the idle promotion where there is something to promote. Interaction disarms it. */
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
   * Fetch one un-held scope whole: whole-extent bbox, the level named (`levels` is inert on a flat
   * layer and is omitted there), no filter — the hold must answer the unfiltered question — and no
   * `artifact_budget`, the budget being inert on levelled and flat layers by contract, omitted for
   * clarity. The response feeds the payload store and the marks and never the served set, so a
   * promotion landing can never redraw the map; a response under keys other than the store's is
   * discarded rather than allowed to rotate it — an answer from another generation is not this
   * hold's, and rotation is the per-view path's to observe.
   */
  private async promote(): Promise<void> {
    const token = this.opts.token();
    if (!token || this.inFlight) return;
    const candidate = this.promotionCandidates()[0];
    if (!candidate) return;
    const signal = new AbortController();
    this.promoting?.abort();
    this.promoting = signal;
    try {
      const response = await this.client.viewport(
        token,
        {
          view: this.opts.view,
          zoom: 0,
          bbox: rectToRequestBbox({x0: 0, y0: 0, x1: 0, y1: 0}, 0, this.opts.quantisation),
          k: 0,
          layers: [candidate.layer],
          ...(candidate.flat ? {} : {levels: [candidate.level]})
        },
        signal.signal
      );
      if (this.promoting !== signal) return;
      this.promoting = null;
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
      // A failed promotion is a saving not made, not an answer lost: the per-view path is intact,
      // and the ratchet re-arms on the next served view rather than retrying here.
    }
  }

  private async request(): Promise<void> {
    const view = this.view;
    const token = this.opts.token();
    const layers = this.state.layers;
    if (!token || !view) return;
    this.inFlight?.abort();
    // No layer selected is not a request. It is also not an error, and not an empty answer to a
    // question that was asked — so the held set is simply cleared.
    if (layers.length === 0) {
      this.inFlight = null;
      // **The served set goes and the store stays.** Switching a layer off is a question not asked,
      // not an answer gone stale: what is held is still true, and switching it back on draws it
      // without a refetch. The store goes on a key rotation and on reset, and there only.
      this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1};
      this.emit();
      return;
    }

    // **A filter always asks the server** (decision 0104): the bit is per request and the points
    // held are a sample of the matches, so a locally derived bit would read false for exactly the
    // small clusters a filter is used to find. The unfiltered settled view over scopes all held
    // whole is the one question a held scope answers, and it is answered locally, with no request.
    const filterExpr = this.opts.filters?.() ?? null;
    if (!filterExpr && this.servesWhole(layers, view)) {
      this.inFlight = null;
      // The served set is still replaced wholesale — the pick is over what is *in view*, never a
      // merge of history — and the payloads it names carry no bit, there being no filter to answer.
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

    // **A filtered view over scopes all held whole asks for identity rows** (protocol §5.2): the
    // held payloads answer every column but the bit, and the bit is the one field a filter moves
    // (0104) — so the ask is the same row set at 13.6 B/row instead of a full re-send.
    const identityAsk = filterExpr !== null && this.servesWhole(layers, view);

    const signal = new AbortController();
    this.inFlight = signal;
    this.state = {...this.state, status: 'loading'};
    this.emit();
    const ask = (rows: 'identity' | null) =>
      this.client.viewport(
        token,
        {
          view: this.opts.view,
          zoom: view.depth,
          bbox: rectToRequestBbox(tileRectOfBbox(view.bbox, view.depth), view.depth, this.opts.quantisation),
          // The counts and the artifacts frame, and no points at all: this channel draws none, and
          // the points on screen are the point path's business.
          k: 0,
          layers,
          // A budgeted cut for the view (design §6): the coarse ancestors at the overview, refined
          // as the zoom deepens, rather than every artifact of a nested or tiered layer at once.
          artifactBudget: artifactBudgetFor(view.zoom),
          ...(filterExpr ? {filters: filterExpr} : {}),
          ...(rows ? {artifactRows: rows} : {})
        },
        signal.signal
      );
    try {
      let response = await ask(identityAsk ? 'identity' : null);
      if (this.inFlight !== signal) return;
      let drawn: Artifact[] | null = null;
      const identityRows = response.result.artifactsIdentity;
      if (identityRows !== null) drawn = this.resolveIdentity(identityRows, response);
      if (drawn === null) {
        // Either a full answer, or an identity one the store could not resolve — §5.2's
        // self-detecting misuse (a rotated generation, a payload never held). The latter falls
        // back ONCE to a full ask for the same view, under the same in-flight token so a
        // superseding gesture aborts it like any other request.
        if (identityRows !== null) {
          response = await ask(null);
          if (this.inFlight !== signal) return;
        }
        drawn = this.hold(response.result.artifacts, response.identityKey, response.contentKey);
        // Only an UNFILTERED response may mark a scope held whole: a filtered row set answers a
        // narrower question, whatever extent it covered.
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
      const e = error as {code?: string; detail?: string; message?: string};
      // A refusal is not an empty view. The **served set** is dropped: it answered a request that
      // has been superseded, and drawing it beside a failure would present the last view's clusters
      // as this one's. The store is untouched — a request that failed said nothing about whether
      // what is held is still true, and the next successful response carries the keys that do.
      this.state = {
        ...this.state,
        artifacts: [],
        status: 'refused',
        refusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)},
        version: this.state.version + 1
      };
      this.emit();
    }
  }
}

/**
 * The tree the response carried, assembled from `parentId`.
 *
 * **Built from what was served and nothing else.** A parent is named only where it is in the same
 * response (decision 0087), and an artifact whose parent was withheld arrives with `parentId` null
 * — identically to one that has no parent at all. So a link that does not resolve is treated as no
 * link, and the artifact is a root of what this viewer was given. There is no "hidden parent"
 * state here because there is nothing on the wire to fill one from.
 *
 * **It is this response's tree, not the layer's.** The set changes as the map moves and as the
 * cut's budget bites: two viewers, and the same viewer at two depths, correctly see different
 * shapes over the same layer.
 */
export type ServedLineage = {
  byId: Map<bigint, Artifact>;
  /** A parent's served children, by the parent's identifier. Absent means none were served. */
  childrenOf: Map<bigint, Artifact[]>;
  /** Those with no served parent — where a walk of the tree starts. */
  roots: Artifact[];
  /** Whether any link resolved at all: a flat layer, and a tree cut to one level, look the same. */
  linked: boolean;
};

export function servedLineage(artifacts: readonly Artifact[]): ServedLineage {
  const byId = new Map(artifacts.map((a) => [a.tesseraId, a]));
  const childrenOf = new Map<bigint, Artifact[]>();
  const roots: Artifact[] = [];
  for (const artifact of artifacts) {
    const parent = artifact.parentId === null ? undefined : byId.get(artifact.parentId);
    if (!parent) {
      roots.push(artifact);
      continue;
    }
    const siblings = childrenOf.get(parent.tesseraId);
    if (siblings) siblings.push(artifact);
    else childrenOf.set(parent.tesseraId, [artifact]);
  }
  return {byId, childrenOf, roots, linked: childrenOf.size > 0};
}

/**
 * One artifact and everything served beneath it — the subtree a viewer picks out by opening it.
 *
 * The visited set is not defensive tidiness about a server that might send a cycle; it is what
 * makes a walk over data from *outside* this program terminate. A malformed response should slow
 * a panel down, never hang the frame loop.
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
