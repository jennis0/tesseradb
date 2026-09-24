import type {FiltersProjection, RegionProjection, ViewProjection} from '@tesseradb/client';

/**
 * Which count the density wash reads, and so its label: `highlighted` under a highlight, `matched`
 * when the request's `filters` carries anything (the filter clauses, `member_of` clauses and the
 * drawn region), `visible` otherwise. The choice follows what was asked, since the counts are
 * equal when nothing narrows them.
 */
export function washChannel(
  filters: FiltersProjection,
  view: ViewProjection,
  region: RegionProjection | null
): 'visible' | 'matched' | 'highlighted' {
  if (view.highlighting) return 'highlighted';
  const filtering = filters.expr !== null || filters.members.some((c) => c.verb === 'filter') || region !== null;
  return filtering ? 'matched' : 'visible';
}
