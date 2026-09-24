/**
 * The artifact budget a view asks for: a coarse cut at the overview, refined as the zoom deepens,
 * so a nested or tiered layer serves the ancestors a viewport can label. The server chooses the cut.
 *
 * The budget is `BASE × 2^zoom`, capped, where `zoom` is the map's own: 0 when the whole extent fits
 * the world, +1 per doubling. The base is 48 because the server cuts a whole tree at one depth: a
 * root with 26 children is served as the root alone under any budget below 27.
 */
export const BASE_ARTIFACT_BUDGET = 48;
export const MAX_ARTIFACT_BUDGET = 2048;

export function artifactBudgetFor(zoom: number): number {
  const z = Number.isFinite(zoom) ? Math.max(0, zoom) : 0;
  return Math.min(MAX_ARTIFACT_BUDGET, Math.round(BASE_ARTIFACT_BUDGET * 2 ** z));
}

/**
 * The level a tiered layer is drawn at, over the levels the response carried: the deepest whose
 * artifacts, with every level above it, fit the budget. `countsByLevel[i]` is how many were served
 * at level `i`; level 0 is always drawn. A request naming no `levels` is answered at the levels
 * declared for its zoom, so the response usually carries fewer levels than the layer has.
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
