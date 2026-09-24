/**
 * Numbers typed by what they are. A masked map shows two kinds of figure, and a panel that confuses
 * them presents a sample as a set: "12,040 of 12,040" against a cluster is false. The formatters
 * render each kind correctly, so a customer drawing their own panel can rely on the type.
 */

/**
 * A sample of a set: how many marks were drawn out of how many items the set holds. The store
 * publishes one as `view.served`, `marks.count` and a region's `served`. Show `shown` and `total`
 * together or not at all, since a sample shown alone reads as the whole set.
 *
 * @category Counts
 */
export type Count = {
  /** How many marks were drawn. */
  shown: number;
  /** How many items the sampled set holds, counted over the viewer's visible set. */
  total: number;
  /**
   * Whether the pair is exact. `false` before anything is served and while a region waits for its
   * numbers; {@link formatCount} then renders nothing.
   */
  exact: boolean;
};

/**
 * A count with no sample behind it, over the viewer's visible set: `visible`, `matched` and
 * `highlighted` in the `view` projection, a region's counts, or an artifact's masked count.
 *
 * @category Counts
 */
export type Masked = {
  /** The count. */
  value: number;
  /**
   * Whether `value` is exact. A region's count is inexact where the server answered for a cover of
   * the shape at some depth, which is a superset of the shape. {@link formatMasked} prefixes an
   * inexact figure with `≈`.
   */
  exact: boolean;
};

/**
 * Options for {@link formatCount} and {@link formatMasked}.
 *
 * @category Counts
 */
export type FormatOptions = {
  /**
   * Whether the figure's view is stale (`status.stale` in the store). A stale figure renders as the
   * empty string. Defaults to `false`.
   */
  stale?: boolean;
  /** The locale for thousands separators, as `Number.prototype.toLocaleString` takes it. Defaults to `en-GB`. */
  locale?: string;
};

const fmt = (n: number, locale: string) => n.toLocaleString(locale);

/**
 * The count before anything is served: zero of zero, not exact.
 *
 * @category Counts
 */
export const NO_COUNT: Count = {shown: 0, total: 0, exact: false};
/**
 * The masked count before anything is served: zero, not exact.
 *
 * @category Counts
 */
export const NO_MASKED: Masked = {value: 0, exact: false};

/**
 * Formats a count as `<shown> of <total>`, such as `1,204 of 29,935`. Returns the empty string
 * where the count is not exact or `opts.stale` is set.
 *
 * @category Counts
 */
export function formatCount(count: Count, opts: FormatOptions = {}): string {
  if (opts.stale || !count.exact) return '';
  const locale = opts.locale ?? 'en-GB';
  return `${fmt(count.shown, locale)} of ${fmt(count.total, locale)}`;
}

/**
 * Formats a masked count as one figure, such as `29,935`, prefixed `≈ ` where it is not exact.
 * Returns the empty string where `opts.stale` is set.
 *
 * @category Counts
 */
export function formatMasked(masked: Masked, opts: FormatOptions = {}): string {
  if (opts.stale) return '';
  const locale = opts.locale ?? 'en-GB';
  return masked.exact ? fmt(masked.value, locale) : `≈ ${fmt(masked.value, locale)}`;
}
