/**
 * The artifact budget a view asks for (design §6's fetch model): a budgeted coarse cut at the
 * overview, refined as the zoom deepens, so a nested or tiered layer is served the ancestors a
 * viewport can label rather than every artifact it holds. The server chooses the cut; this is
 * only how many the view can use.
 *
 * **The formula: `BASE × 2^zoom`, capped.** `zoom` is the map's own — 0 when the whole extent
 * fits the 512-unit world, +1 per doubling — so the budget doubles per zoom level: about the
 * labels a viewport fits at the overview, and the finer cut a zoomed view can show. Named here so
 * a test pins it and a reader finds one place to change it.
 *
 * The base is 48 because the server cuts a whole tree at one depth: a root with 26 children is
 * served as the root alone under any budget below 27, and the overview would show one artifact.
 * Forty-eight serves the first split of a tree that wide and its topics; a per-branch cut is
 * asked of the server (S9 in the delivery record) and would let this fall to the labels a
 * viewport fits.
 */
export const BASE_ARTIFACT_BUDGET = 48;
export const MAX_ARTIFACT_BUDGET = 2048;

export function artifactBudgetFor(zoom: number): number {
  const z = Number.isFinite(zoom) ? Math.max(0, zoom) : 0;
  return Math.min(MAX_ARTIFACT_BUDGET, Math.round(BASE_ARTIFACT_BUDGET * 2 ** z));
}

/**
 * The level a tiered layer is drawn at, over the levels the response actually carried: the deepest
 * whose
 * artifacts, with every level above it, fit the budget — so a 16 / 46 / 161 / 574 layer draws
 * its 16 at the overview and refines as the zoom deepens, following the cut the budget asks
 * for. `countsByLevel[i]` is how many were served at level `i`; level 0 is always drawn.
 *
 * **It is a drawing choice over what arrived, not over the layer.** Since 2026-08-28 a request that
 * names no `levels` is answered at the levels the layer's declared zoom ranges give for its depth
 * (decision 0103), so the response usually carries fewer levels than the layer has and this picks
 * among those. An earlier revision of this comment said "when the server served every level", which
 * was the behaviour then and is now the `levels: "all"` case alone.
 */
export function levelForBudget(countsByLevel: readonly number[], budget: number): number {
  let level = 0;
  let cumulative = 0;
  for (let i = 0; i < countsByLevel.length; i++) {
    cumulative += countsByLevel[i]!;
    if (i > 0 && cumulative > budget) break;
    level = i;
  }
  return level;
}
