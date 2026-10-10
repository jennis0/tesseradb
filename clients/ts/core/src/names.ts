import type {Artifact, BrowseRow, Layer} from './types.js';

/**
 * What to call an artifact: its own first supplied text, else the text of the artifact attached to
 * it (a clustering's topic label) in `attached`, else `null`. An empty text counts as none. A key
 * such as `hdb-2422486` is never used. Pass `attached` from {@link ArtifactsProjection.attached}.
 *
 * @category Artifacts
 */
export function artifactName(a: Pick<Artifact, 'mosaicaId' | 'content'> | Pick<BrowseRow, 'mosaicaId' | 'name'>, attached: ReadonlyMap<bigint, string> = NONE): string | null {
  const own = 'content' in a ? a.content[0] : a.name;
  return own !== undefined && own !== null && own.length > 0 ? own : (attached.get(a.mosaicaId) ?? null);
}

const NONE: ReadonlyMap<bigint, string> = new Map();

/**
 * The first text of each artifact attached to another, keyed by the `target` it names. An
 * artifact with no target or no text adds nothing. Where several attach to one target, the one
 * the server names a browse row by wins: the layer that comes first in `layers` (the order `meta`
 * lists them), then the lower level, then an artifact with a key before one without, keys in
 * ascending order, then the one met first, which in a viewport's frame is publication order. A
 * layer not listed comes after every listed one.
 *
 * @internal
 */
export function attachedTextOf(artifacts: Iterable<Pick<Artifact, 'layer' | 'target' | 'content' | 'key' | 'rung'>>, layers: readonly Pick<Layer, 'name' | 'hierarchy'>[]): Map<bigint, string> {
  const order = new Map(layers.map((l, i) => [l.name, {rank: i, levelled: l.hierarchy.kind === 'stacked' || l.hierarchy.kind === 'tiered'}]));
  const place = (a: Pick<Artifact, 'layer' | 'key' | 'rung'>): [number, number, string | null] => {
    const at = order.get(a.layer);
    return [at?.rank ?? layers.length, at?.levelled ? a.rung : 0, a.key];
  };
  const before = (a: [number, number, string | null], b: [number, number, string | null]): boolean => {
    if (a[0] !== b[0]) return a[0] < b[0];
    if (a[1] !== b[1]) return a[1] < b[1];
    if (a[2] === null || b[2] === null) return a[2] !== null && b[2] === null;
    return a[2] < b[2];
  };
  const best = new Map<bigint, {at: [number, number, string | null]; text: string}>();
  for (const a of artifacts) {
    const text = a.content[0];
    if (a.target === null || text === undefined || text.length === 0) continue;
    const at = place(a);
    const held = best.get(a.target);
    if (!held || before(at, held.at)) best.set(a.target, {at, text});
  }
  return new Map([...best].map(([target, {text}]) => [target, text]));
}
