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
 */
export const BASE_ARTIFACT_BUDGET = 24;
export const MAX_ARTIFACT_BUDGET = 2048;

export function artifactBudgetFor(zoom: number): number {
  const z = Number.isFinite(zoom) ? Math.max(0, zoom) : 0;
  return Math.min(MAX_ARTIFACT_BUDGET, Math.round(BASE_ARTIFACT_BUDGET * 2 ** z));
}

/**
 * The level a tiered layer is drawn at when the server served every level: the deepest whose
 * artifacts, with every level above it, fit the budget — so a 16 / 46 / 161 / 574 layer draws
 * its 16 at the overview and refines as the zoom deepens, following the cut the budget asks
 * for. `countsByLevel[i]` is how many were served at level `i`; level 0 is always drawn.
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
