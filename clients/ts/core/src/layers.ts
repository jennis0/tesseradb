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
