import type {Layer} from './types.js';

/**
 * One entry of a layer picker: a root layer and its closure, which is what the store names in a
 * request. A clustering's labels are a second layer that `depends_on` it; named alone, one would
 * serve labels with no clusters, or clusters without their labels.
 *
 * @category Layers and views
 */
export type LayerEntry = {
  /**
   * A layer that depends on no layer `meta` lists. A layer whose dependency this viewer cannot
   * reach is therefore a root.
   */
  root: Layer;
  /** The closure of `root`, as {@link layerClosure} gives it. Two roots with a common dependent both carry it. */
  closure: string[];
};

/**
 * The names in the closures of `names`, in `layers` order, each once. A layer's closure is the
 * layer and every layer linked to it through `depends_on`, in either direction and transitively. A
 * name `layers` does not list is kept, after the others, since the server intersects a request
 * with what the viewer reaches.
 *
 * @category Layers and views
 */
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

/**
 * The entries a layer picker offers: one per root (a layer that depends on no layer in `layers`),
 * with its closure, in `layers` order.
 *
 * @category Layers and views
 */
export function layerEntries(layers: readonly Layer[]): LayerEntry[] {
  const reachable = new Set(layers.map((l) => l.name));
  return layers
    .filter((l) => l.depsOn.every((d) => !reachable.has(d)))
    .map((root) => ({root, closure: layerClosure(layers, [root.name])}));
}

/**
 * Whether a layer is a filter layer: one that declares no computed geometry (`computedContent` is
 * empty) and depends on no layer. Its artifacts have counts and no geometry. The store does not
 * draw a filter layer or name it in a viewport request's `layers`; it is applied as a `member_of`
 * clause, from a hierarchy panel. The server treats it as any other layer.
 *
 * @category Layers and views
 */
export function isFilterLayer(layer: Layer): boolean {
  // A labels layer also declares no geometry; its text is drawn at the artifact it depends on.
  return layer.computedContent.length === 0 && layer.depsOn.length === 0;
}

/**
 * The layers a client may draw: every layer that is not a filter layer.
 *
 * @category Layers and views
 */
export function drawableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l));
}

/**
 * The layers points may be coloured by, with `setColourBy('cluster:<layer>')`: every layer that is
 * not a filter layer and depends on no layer. A labels layer depends on the layer it labels and is
 * left out, since its artifacts have no members of their own.
 *
 * @category Layers and views
 */
export function colourLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l) && l.depsOn.length === 0);
}

/**
 * The layers a hierarchy panel offers: every layer with a lineage (a `hierarchy.kind` other than
 * `flat`) that depends on no layer. A layer that depends on another is left out, since its text is
 * the name of the artifact it is attached to.
 *
 * @category Layers and views
 */
export function browsableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => l.depsOn.length === 0 && l.hierarchy.kind !== 'flat');
}
