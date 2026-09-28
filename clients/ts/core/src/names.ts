import type {Artifact, BrowseRow} from './types.js';

/**
 * What to call an artifact: its own first supplied text, else the text of the artifact attached to
 * it (a clustering's topic label) in `attached`, else `null`. An empty text counts as none. A key
 * such as `hdb-2422486` is never used. Pass `attached` from {@link ArtifactsProjection.attached}.
 *
 * @category Artifacts
 */
export function artifactName(a: Pick<Artifact, 'tesseraId' | 'content'> | Pick<BrowseRow, 'tesseraId' | 'name'>, attached: ReadonlyMap<bigint, string> = NONE): string | null {
  const own = 'content' in a ? a.content[0] : a.name;
  return own !== undefined && own !== null && own.length > 0 ? own : (attached.get(a.tesseraId) ?? null);
}

const NONE: ReadonlyMap<bigint, string> = new Map();

/**
 * The first text of each artifact attached to another, keyed by the `target` it names. An
 * artifact with no target or no text adds nothing. Where several attach to one target, the one
 * whose layer comes first in `layerOrder` (the declaration order in `meta`) names it; a layer not
 * listed comes after every listed one.
 *
 * @internal
 */
export function attachedTextOf(artifacts: Iterable<Pick<Artifact, 'layer' | 'target' | 'content'>>, layerOrder: readonly string[]): Map<bigint, string> {
  const rank = (layer: string) => {
    const i = layerOrder.indexOf(layer);
    return i === -1 ? layerOrder.length : i;
  };
  const best = new Map<bigint, {rank: number; text: string}>();
  for (const a of artifacts) {
    const text = a.content[0];
    if (a.target === null || text === undefined || text.length === 0) continue;
    const r = rank(a.layer);
    const held = best.get(a.target);
    if (!held || r < held.rank) best.set(a.target, {rank: r, text});
  }
  return new Map([...best].map(([target, {text}]) => [target, text]));
}
