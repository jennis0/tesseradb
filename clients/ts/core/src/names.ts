import type {Artifact, BrowseRow} from './types.js';

/**
 * What to call an artifact: its own supplied text, else the text of the artifact attached to it
 * (a clustering's topic label), else `null`. Pass `attached` from
 * {@link ArtifactsProjection.attached} for a served artifact, or from {@link Store.attachedText}
 * for a {@link Store.browse} row.
 *
 * The server serves a label only to a viewer who may read it, under the label layer's own terms
 * and over that viewer's visible set, so a name the viewer may not see is never in `attached`. A
 * key such as `hdb-2422486` is an identifier and is never used as a name.
 *
 * @category Artifacts
 */
export function artifactName(a: Pick<Artifact, 'tesseraId' | 'content'> | Pick<BrowseRow, 'tesseraId' | 'name'>, attached: ReadonlyMap<bigint, string>): string | null {
  const own = 'content' in a ? a.content[0] : a.name;
  return own !== undefined && own !== null && own.length > 0 ? own : (attached.get(a.tesseraId) ?? null);
}

/**
 * The first text of each artifact attached to another, keyed by the `target` it names. An
 * artifact with no target or no text adds nothing.
 *
 * @internal
 */
export function attachedTextOf(artifacts: Iterable<Pick<Artifact, 'target' | 'content'>>): Map<bigint, string> {
  const out = new Map<bigint, string>();
  for (const a of artifacts) {
    const text = a.content[0];
    if (a.target !== null && text !== undefined && text.length > 0) out.set(a.target, text);
  }
  return out;
}
