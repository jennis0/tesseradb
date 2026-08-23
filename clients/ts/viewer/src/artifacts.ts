import {
  TesseraClient,
  rectToRequestBbox,
  tileRectOfBbox,
  worldBbox,
  type Artifact,
  type Quantisation
} from '@tessera/client';
import type {Store} from './state.js';
import type {ViewState} from './viewportLayer.js';

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
 */
export class ArtifactChannel {
  /** The in-flight request, aborted when a later view supersedes it. */
  private inFlight: AbortController | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private view: {bbox: [number, number, number, number]; depth: number} | null = null;

  constructor(
    private readonly client: TesseraClient,
    private readonly store: Store,
    private readonly quantisation: Quantisation
  ) {}

  /**
   * Note the view and ask once it settles.
   *
   * Debounced rather than issued per frame: the artifact set changes only when the view crosses a
   * tile boundary, and a request per pointer-move would put the server's counting stage inside the
   * gesture. Long enough to swallow a drag, short enough that the counts land as the hand stops.
   */
  schedule(view: ViewState, width: number, height: number): void {
    if (!this.noteView(view, width, height)) return;
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.request();
    }, SETTLE_MS);
  }

  /** What to ask for: the visible box, at the depth the map is drawn at. False when there is none. */
  private noteView(view: ViewState, width: number, height: number): boolean {
    // **The depth the map is actually drawn at**, not one chosen here: which artifacts a view is
    // served depends on the tiles requested, so asking at a different depth from the one on screen
    // would answer for ground the marks do not cover. It also keeps this off a second copy of the
    // budget rule. Before the first frame there is no such depth, and nothing is drawn to annotate.
    const depth = this.store.state.assembled?.depth;
    if (depth === undefined) return false;
    // The *visible* box, with no margin: what a viewer is looking at is what the clusters should
    // answer for. The point path fetches a wider ring so a small pan costs no request, and
    // borrowing that box here would put clusters on screen for ground the user cannot see.
    const viewport = {
      target: [view.target[0], view.target[1]] as [number, number],
      zoom: view.zoom,
      width,
      height
    };
    this.view = {bbox: worldBbox(viewport, 1), depth};
    return true;
  }

  /**
   * Ask now, for this view — a layer toggle, where the view has not moved and nothing else will
   * ask.
   *
   * It takes the view rather than reusing the noted one, because the noted one may be absent: the
   * channel notes a view only once a frame has been drawn to annotate, and a user who changes the
   * layer before then would otherwise get silence and no way to tell it from an empty answer.
   */
  refresh(view: ViewState, width: number, height: number): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    const noted = this.noteView(view, width, height);
    if (!noted) return;
    void this.request();
  }

  /** Drop what is held and abandon anything in flight: a new principal, or a dataset switch. */
  reset(): void {
    this.cancel();
    this.store.update((s) => {
      s.artifacts = [];
      s.artifactVersion += 1;
      s.artifactStatus = 'idle';
      s.artifactError = null;
      // A different principal is a different count for the same identifier, so the opened cluster
      // goes with the session that opened it rather than being left showing the last one's number.
      s.selectedArtifact = null;
      s.artifactDetailError = null;
    });
  }

  cancel(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.inFlight?.abort();
    this.inFlight = null;
  }

  private async request(): Promise<void> {
    const {session, view: viewId, artifactLayer} = this.store.state;
    const view = this.view;
    if (!session || !view) return;
    this.inFlight?.abort();
    // No layer selected is not a request. It is also not an error, and not an empty answer to a
    // question that was asked — so the held set is simply cleared.
    if (!artifactLayer) {
      this.inFlight = null;
      this.store.update((s) => {
        s.artifacts = [];
        s.artifactVersion += 1;
        s.artifactStatus = 'idle';
        s.artifactError = null;
      });
      return;
    }

    const signal = new AbortController();
    this.inFlight = signal;
    this.store.update((s) => {
      s.artifactStatus = 'loading';
    });
    try {
      const response = await this.client.viewport(
        session.token,
        {
          view: viewId,
          zoom: view.depth,
          bbox: rectToRequestBbox(tileRectOfBbox(view.bbox, view.depth), view.depth, this.quantisation),
          // The counts and the artifacts frame, and no points at all: this channel draws none, and
          // the points on screen are the replica's business.
          k: 0,
          layers: [artifactLayer]
        },
        signal.signal
      );
      if (this.inFlight !== signal) return;
      this.inFlight = null;
      this.store.update((s) => {
        s.artifacts = response.result.artifacts;
        s.artifactVersion += 1;
        s.artifactStatus = 'shown';
        s.artifactError = null;
      });
    } catch (error) {
      if (signal.signal.aborted || this.inFlight !== signal) return;
      this.inFlight = null;
      const e = error as {code?: string; detail?: string; message?: string};
      this.store.update((s) => {
        // A refusal is not an empty view, and the panel says which it is. Held artifacts are
        // dropped: they answered a request that has been superseded, and drawing them beside a
        // failure would present the last view's clusters as this one's.
        s.artifacts = [];
        s.artifactVersion += 1;
        s.artifactStatus = 'refused';
        s.artifactError = {
          code: e.code ?? 'fetch-failed',
          detail: e.detail ?? e.message ?? String(error)
        };
      });
    }
  }
}

/** How long the view must be still before the artifact request goes out. */
const SETTLE_MS = 200;

/**
 * Where the publisher put each cluster — see `scripts/publish-clusters.mjs`.
 *
 * **Development scaffolding, and the one thing here that is not a pattern.** There is no artifact
 * geometry on the wire at this stage, deliberately: a bounding box over full membership would
 * disclose a cluster's extent by panning. Real geometry arrives as derived content, gated by the
 * containment test. Until then a demo has to get a position from somewhere, and it comes from the
 * publisher's own sidecar.
 *
 * **It supplies position and nothing else.** What is drawn is decided entirely by what the server
 * served: an entry here with no artifact in the response is not drawn, so the sidecar can never
 * put a cluster on screen that this principal was not served. It carries no membership and no
 * declared size, both of which the publisher knows and neither of which may reach a viewer.
 */
export type ArtifactPlaces = Map<string, {x: number; y: number}>;

/**
 * Every published layer's positions, by layer name — a run of the publish script per layer, since
 * a name is never reused and an edit is a delete plus a re-publish.
 *
 * A missing or malformed document is not an error, and is the ordinary state of a server nobody
 * has published to: the layer panel still lists what is served and its counts, and nothing is
 * drawn on the map.
 */
export async function loadArtifactPlaces(): Promise<Map<string, ArtifactPlaces>> {
  const byLayer = new Map<string, ArtifactPlaces>();
  try {
    const response = await fetch('/clusters.json', {cache: 'no-store'});
    if (!response.ok) return byLayer;
    const body = (await response.json()) as {
      layers?: Record<string, {key: string; x: number; y: number}[]>;
    };
    for (const [layer, clusters] of Object.entries(body.layers ?? {})) {
      byLayer.set(layer, new Map(clusters.map((c) => [c.key, {x: c.x, y: c.y}])));
    }
  } catch {
    // Left empty on a parse failure, for the same reason.
  }
  return byLayer;
}

/**
 * The artifacts that can actually be drawn, in the order they should be: largest count last, so a
 * small cluster is never hidden under a large one.
 *
 * An artifact with no key, or one whose key the sidecar does not know, is **not placed and not
 * drawn** — it is still listed with its count in the panel, which is the honest split: the count
 * came from the service and the position did not.
 */
export function placedArtifacts(
  artifacts: Artifact[],
  places: ArtifactPlaces
): {artifact: Artifact; x: number; y: number}[] {
  const placed: {artifact: Artifact; x: number; y: number}[] = [];
  for (const artifact of artifacts) {
    const at = artifact.key === null ? undefined : places.get(artifact.key);
    if (!at) continue;
    placed.push({artifact, x: at.x, y: at.y});
  }
  placed.sort((a, b) => (a.artifact.maskedCount < b.artifact.maskedCount ? -1 : 1));
  return placed;
}

/**
 * The tree the response carried, assembled from `parentId`.
 *
 * **Built from what was served and nothing else.** A parent is named only where it is in the same
 * response ([decision 0087](../../../../docs/decisions/0087-cross-level-edges-are-information-not-rollup.md)),
 * and an artifact whose parent was withheld arrives with `parentId` null — identically to one that
 * has no parent at all. So a link that does not resolve is treated as no link, and the artifact is
 * a root of what this viewer was given. There is no "hidden parent" state here because there is
 * nothing on the wire to fill one from, and modelling one would assert the existence of a coarser
 * artifact this principal was not shown.
 *
 * **It is this response's tree, not the layer's.** The set changes as the map moves and as the
 * cut's budget bites: two viewers, and the same viewer at two depths, correctly see different
 * shapes over the same layer.
 */
export type ServedLineage = {
  /** Every served artifact by identifier. */
  byId: Map<bigint, Artifact>;
  /** A parent's served children, by the parent's identifier. Absent means none were served. */
  childrenOf: Map<bigint, Artifact[]>;
  /** Those with no served parent — where a walk of the tree starts. */
  roots: Artifact[];
  /** Whether any link resolved at all: a flat layer, and a tree cut to one level, look the same. */
  linked: boolean;
};

export function servedLineage(artifacts: Artifact[]): ServedLineage {
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
