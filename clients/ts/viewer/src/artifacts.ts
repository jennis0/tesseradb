/**
 * The publisher's cluster sidecar — the one thing here that is vis-only, and the demo's scaffolding.
 *
 * The annotation channel and the served-lineage helpers moved to `@tesseradb/client` (design
 * client-components §4). What stays is where a cluster is *drawn*, which has no source on the wire
 * yet: real geometry arrives as derived content gated by containment (step 3). Until then a demo
 * gets a position from the publish script's own sidecar, and `servedLineage`/`subtreeOf` come from
 * core so there is one source for the tree.
 */
import type {Artifact} from '@tesseradb/client';
export {servedLineage, subtreeOf, type ServedLineage} from '@tesseradb/client';

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
