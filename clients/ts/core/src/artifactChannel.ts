import type {TesseraClient} from './client.js';
import {rectArea} from './rects.js';
import {tileRectOfBbox} from './budget.js';
import {rectToRequestBbox} from './coords.js';
import {worldBbox, type Viewport} from './prefetch.js';
import type {ViewState} from './driver.js';
import type {Artifact, Quantisation} from './types.js';
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
 * **No accumulation.** Held artifacts are replaced wholesale by each response, never merged. A
 * cluster is served because a member visible to this principal falls inside the requested tiles,
 * so a merged set would show clusters for ground the user has panned away from — presented,
 * inevitably, as though they were in view.
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
};

export class ArtifactChannel {
  private inFlight: AbortController | null = null;
  private timer: unknown = null;
  private view: {bbox: [number, number, number, number]; depth: number; zoom: number} | null = null;
  private readonly clock: ArtifactChannelClock;
  private readonly settleMs: number;
  private readonly table: SessionArtifactTable | null;
  /** The ordinals the current served set holds a reference on, released when it rotates. */
  private heldOrdinals: Uint32Array | null = null;
  private state: ArtifactChannelState = {
    layer: null,
    layers: [],
    artifacts: [],
    status: 'idle',
    refusal: null,
    version: 0
  };

  constructor(
    private readonly client: TesseraClient,
    private readonly opts: ArtifactChannelOptions
  ) {
    this.clock = opts.clock ?? defaultClock();
    this.settleMs = opts.settleMs ?? SETTLE_MS;
    this.table = opts.table ?? null;
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
    this.releaseHeld();
    this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1};
    this.emit();
  }

  cancel(): void {
    if (this.timer) this.clock.cancel(this.timer);
    this.timer = null;
    this.inFlight?.abort();
    this.inFlight = null;
  }

  private releaseHeld(): void {
    if (this.table && this.heldOrdinals) this.table.release(this.heldOrdinals);
    this.heldOrdinals = null;
  }

  /** Take the served set into the session table, releasing the reference the previous set held. */
  private hold(artifacts: readonly Artifact[]): void {
    if (!this.table) return;
    const refs: ArtifactRef[] = artifacts.map((a) => ({
      tesseraId: a.tesseraId,
      layer: a.layer,
      parentId: a.parentId,
      centroid: a.centroid,
      level: a.level
    }));
    const taken = this.table.take(refs);
    this.releaseHeld();
    this.heldOrdinals = taken;
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
      this.releaseHeld();
      this.state = {...this.state, artifacts: [], status: 'idle', refusal: null, version: this.state.version + 1};
      this.emit();
      return;
    }

    const signal = new AbortController();
    this.inFlight = signal;
    this.state = {...this.state, status: 'loading'};
    this.emit();
    try {
      const response = await this.client.viewport(
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
          artifactBudget: artifactBudgetFor(view.zoom)
        },
        signal.signal
      );
      if (this.inFlight !== signal) return;
      this.inFlight = null;
      this.hold(response.result.artifacts);
      this.state = {
        ...this.state,
        artifacts: response.result.artifacts,
        status: 'shown',
        refusal: null,
        version: this.state.version + 1
      };
      this.emit();
    } catch (error) {
      if (signal.signal.aborted || this.inFlight !== signal) return;
      this.inFlight = null;
      const e = error as {code?: string; detail?: string; message?: string};
      // A refusal is not an empty view. Held artifacts are dropped: they answered a request that
      // has been superseded, and drawing them beside a failure would present the last view's
      // clusters as this one's.
      this.releaseHeld();
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
