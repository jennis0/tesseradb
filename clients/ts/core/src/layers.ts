import type {Layer} from './types.js';

/**
 * What the layer picker offers, and what the store names in a request (decision 0096, design
 * §5.3): a layer **with its closure**. A clustering's labels are a second layer that
 * `depends_on` it, so one entry names both and the request names every layer in it — each
 * costs its own pass, and a layer named alone would serve labels with no clusters under them
 * or clusters with their labels withheld.
 *
 * The closure of a layer is the layer, every layer that depends on it transitively, and every
 * layer those depend on. An entry is a layer nothing else in `meta` depends on — the root of
 * its closure — so two roots sharing a dependency each carry it, and a layer whose dependency
 * this principal does not reach is a root of what they *were* given. Never a count of a
 * layer's artifacts: the wire carries none (§5.3).
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
  // A name `meta` does not list is kept as given: the server intersects the request with the
  // reachable set, so naming it costs nothing and learns nothing (contracts §3.2), and a host
  // that set a layer before `meta` arrived keeps its intent.
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
 * Whether a layer is a **filter layer** — `computed = []` on its declaration
 * (`artifact-shapes.md` §8 C; `highlight-and-hierarchy.md` §5.4, the owner's ruling of
 * 2026-09-02).
 *
 * *Counts with no geometry* is what the declaration already meant on the wire. What the client
 * reads it as is: **a filter layer is still a layer**. It stays in `/v1/meta`'s roster and in the
 * client's, listed as a filter layer rather than presented for viewing — no draw toggle, no place
 * in the *In view* list, no label, no shape, and no *fit* on its card — and it is **never named in
 * `layers` on a viewport request**, so no artifact pass runs for it and no artifacts frame rows
 * arrive. It is reached through the hierarchy panel and applied as a `member_of` clause.
 *
 * `mesh/descriptors` is declared so from 2026-09-02: a MeSH descriptor's members are spread over
 * the whole layout, so its hull is the map's outline and its cut deepens uniformly until the
 * leaves arrive at a zoom nobody reaches. The rung's clustering keeps its shapes, because its
 * artifacts are compact — the rule is per layer and the declaration states it.
 *
 * The server changes nothing for it. This is a reading of a declaration, not a new field.
 */
export function isFilterLayer(layer: Layer): boolean {
  // A labels layer declares no geometry either: its text is drawn at the artifact it depends on.
  return layer.computedContent.length === 0 && layer.depsOn.length === 0;
}

/** The layers a client may draw — every layer that is not a filter layer. */
export function drawableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l));
}

/**
 * The layers whose clusters points may be coloured by (`colourBy = "cluster:<layer>"`): every
 * drawable layer that is not a labels layer. A labels layer's artifacts have no members of their
 * own to colour.
 */
export function colourLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => !isFilterLayer(l) && l.depsOn.length === 0);
}

/**
 * Whether a layer has a lineage to walk — anything but `flat`
 * (`highlight-and-hierarchy.md` §4, §5.1).
 *
 * A `flat` layer is one page of roots and no row has children, which the browse verb still
 * answers; the panel's *layer picker over the bundle's hierarchical layers* is this list, so a
 * flat clustering is not offered a tree it does not have. **`flat` is the one kind with no
 * lineage and it declares no levels** — the levelled shapes are `stacked` and `tiered` — so the
 * kind decides this on its own and a *flat with levels* disjunct would be a state the wire cannot
 * produce, written as though it could.
 *
 * A layer that attaches to another — a label layer hanging from a clustering — has no lineage of
 * its own and its text is what its target shows as a name, so it is not browsed separately either.
 */
export function browsableLayers(layers: readonly Layer[]): Layer[] {
  return layers.filter((l) => l.depsOn.length === 0 && l.hierarchy.kind !== 'flat');
}
