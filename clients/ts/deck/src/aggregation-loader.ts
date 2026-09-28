/**
 * Loads `aggregation.ts` the first time the hexagons or contours are drawn. A host's bundler
 * puts it in a chunk of its own, which a page that never draws them does not load.
 */
export function importAggregation(): Promise<typeof import('./aggregation.js')> {
  return import('./aggregation.js');
}
