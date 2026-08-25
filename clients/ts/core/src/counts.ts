/**
 * Numbers typed by what they are (design client-components §4).
 *
 * A masked map shows two kinds of figure, and a panel that confuses them presents a sample as a
 * set — "12,040 of 12,040" against a cluster is false, not secret. The types make the confusion
 * unnatural to write, and the two formatters render each correctly so a customer drawing their own
 * panel gets the right figure by taking the type's word for it.
 */

/**
 * A **served sample** of a set: `served` for the view, the draw list, the marks inside a region.
 * `shown` is how many of the set were drawn; `total` is the set they sample. Both figures are
 * shown, or neither — a sample presented alone reads as the set.
 */
export type Count = {shown: number; total: number; exact: boolean};

/**
 * A **number-channel scalar** with no sample behind it: `visible`, `matched`, an artifact's masked
 * count, a region's counts. One figure, or none. `exact` is false where the figure is exact for a
 * cover the user cannot see — a region counted at a cell coarser than a pixel (§5.11).
 */
export type Masked = {value: number; exact: boolean};

export type FormatOptions = {
  /**
   * Whether the view the figure belongs to is stale — the content key moved under it. Nothing
   * renders against a stale view: a stale number beside a refresh control is the honest state,
   * and the acceptance harness (§9) checks that no count renders while `status.stale` is set.
   */
  stale?: boolean;
  locale?: string;
};

const fmt = (n: number, locale: string) => n.toLocaleString(locale);

/** The zero count — what a view holds before anything is served. */
export const NO_COUNT: Count = {shown: 0, total: 0, exact: false};
export const NO_MASKED: Masked = {value: 0, exact: false};

/**
 * Both figures — `shown of total` — or the empty string.
 *
 * Nothing when the count is not exact (the drawn set is a superset of the served one, so the
 * figures would compare a superset to a set) and nothing against a stale view.
 */
export function formatCount(count: Count, opts: FormatOptions = {}): string {
  if (opts.stale || !count.exact) return '';
  const locale = opts.locale ?? 'en-GB';
  return `${fmt(count.shown, locale)} of ${fmt(count.total, locale)}`;
}

/**
 * One figure, or the empty string.
 *
 * Nothing against a stale view. An inexact figure is still one figure, marked as approximate —
 * a region counted at a cell coarser than a pixel is exact for the cells and not for the shape.
 */
export function formatMasked(masked: Masked, opts: FormatOptions = {}): string {
  if (opts.stale) return '';
  const locale = opts.locale ?? 'en-GB';
  return masked.exact ? fmt(masked.value, locale) : `≈ ${fmt(masked.value, locale)}`;
}
