/**
 * The loader the components' single-file bundle takes in place of `aggregation-loader.ts`
 * (`components/vite.config.ts`). A file with one chunk holds the aggregation layers either way, and
 * a dynamic import inlined into it makes the bundler wrap every module in a lazy initialiser.
 */
import * as aggregation from './aggregation.js';

export function importAggregation(): Promise<typeof aggregation> {
  return Promise.resolve(aggregation);
}
