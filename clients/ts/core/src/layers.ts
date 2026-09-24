import type {Layer} from './types.js';

/**
 * What the layer picker offers and the store names in a request: a layer with its closure. A
 * clustering's labels are a second layer that `depends_on` it; named alone, one would serve labels
 * with no clusters, or clusters without their labels.
 *
 * The closure is the layer, every layer that depends on it transitively, and every layer those
 * depend on. An entry is a layer nothing else in `meta` depends on, so two roots sharing a
 * dependency each carry it, and a layer whose dependency this principal does not reach is a root.
 */
export type LayerEntry = {root: Layer; closure: string[]};

/** The names in `names`' closures, in `meta` order, each once. */
export function layerClosure(layers: readonly Layer[], names: readonly string[]): string[] {
  const byName = new Map(layers.map((l) => [l.name, l]));
  const dependents = new Map<string, string[]>();
  for (const l of layers) for (const d of l.depsOn) (dependents.get(d) ?? dependents.set(d, []).get(d)!).push(l.name);
  const seen = new Set<string>();
  const stack = names.filter((n) => byName.has(n));
  while (stack.length > 0) {
    const name = stack.pop()!;
    if (seen.has(name)) continue;
    seen.add(name);
    for (const d of byName.get(name)!.depsOn) if (byName.has(d)) stack.push(d);
    for (const d of dependents.get(name) ?? []) stack.push(d);
  }
  // A name `meta` does not list is kept: the server intersects the request with what the principal
  // reaches, and a host that set a layer before `meta` arrived keeps its choice.
  const unknown = names.filter((n) => !byName.has(n));
  return [...layers.filter((l) => seen.has(l.name)).map((l) => l.name), ...unknown.filter((n, i) => unknown.indexOf(n) === i)];
}

/** The entries a picker offers: one per root, with its closure. */
export function layerEntries(layers: readonly Layer[]): LayerEntry[] {
  const reachable = new Set(layers.map((l) => l.name));
  return layers
    .filter((l) => l.depsOn.every((d) => !reachable.has(d)))
    .map((root) => ({root, closure: layerClosure(layers, [root.name])}));
}

/**
 * Whether a layer is a filter layer: `computed = []`, counts with no geometry. A filter layer stays
 * in `/v1/meta` and in the client's roster, but is not drawn, listed in view, labelled or fitted,
 * and is not named in a viewport request's `layers`. It is reached through the hierarchy panel and
 * applied as a `member_of` clause. The server does nothing different for it.
 */
export function isFilterLayer(layer: Layer): boolean {
  // A labels layer also declares no geometry; its text is drawn at the artifact it depends on.
  return layer.computedContent.length === 0 && layer.depsOn.length === 0;
}

/** The layers a client may draw: every layer that is not a filter layer. */
export function drawableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l));
}

/**
 * The layers points may be coloured by (`colourBy = "cluster:<layer>"`): every drawable layer that
 * is not a labels layer, since a labels layer's artifacts have no members of their own.
 */
export function colourLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l) && l.depsOn.length === 0);
}

/**
 * The layers the hierarchy panel offers: every layer except `flat` ones, which have no lineage
 * (the levelled kinds are `stacked` and `tiered`), and layers attached to another, whose text is
 * their target's name.
 */
export function browsableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => l.depsOn.length === 0 && l.hierarchy.kind !== 'flat');
}
