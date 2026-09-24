/**
 * Numbers typed by what they are. A masked map shows two kinds of figure, and a panel that confuses
 * them presents a sample as a set: "12,040 of 12,040" against a cluster is false. The formatters
 * render each kind correctly, so a customer drawing their own panel can rely on the type.
 */

/**
 * A served sample of a set: `served` for the view, the draw list, the marks inside a region.
 * `shown` is how many were drawn and `total` the set they sample. Both are shown or neither, since
 * a sample alone reads as the set.
 */
export type Count = {shown: number; total: number; exact: boolean};

/**
 * A count with no sample behind it: `visible`, `matched`, an artifact's masked count, a region's
 * counts. `exact` is false where the figure is exact for a cover of the region at some depth rather
 * than for the shape.
 */
export type Masked = {value: number; exact: boolean};

export type FormatOptions = {
  /** Whether the view the figure belongs to is stale, its content key having moved. Nothing renders then. */
  stale?: boolean;
  locale?: string;
};

const fmt = (n: number, locale: string) => n.toLocaleString(locale);

/** The zero count, before anything is served. */
export const NO_COUNT: Count = {shown: 0, total: 0, exact: false};
export const NO_MASKED: Masked = {value: 0, exact: false};

/**
 * `shown of total`, or the empty string where the count is not exact (the drawn set is a superset)
 * or the view is stale.
 */
export function formatCount(count: Count, opts: FormatOptions = {}): string {
  if (opts.stale || !count.exact) return '';
  const locale = opts.locale ?? 'en-GB';
  return `${fmt(count.shown, locale)} of ${fmt(count.total, locale)}`;
}

/**
 * One figure, or the empty string against a stale view. An inexact figure is marked as
 * approximate.
 */
export function formatMasked(masked: Masked, opts: FormatOptions = {}): string {
  if (opts.stale) return '';
  const locale = opts.locale ?? 'en-GB';
  return masked.exact ? fmt(masked.value, locale) : `≈ ${fmt(masked.value, locale)}`;
}
