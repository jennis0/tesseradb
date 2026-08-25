/**
 * The filter controls' UI units, and a re-export of the composition that now lives in the store.
 *
 * **Composition moved to `@tesseradb/client`** (design client-components §4): `composeFilters`,
 * `emptyDraft`, `isPopulated`, `activeCount` and the draft types are the store's, so every customer
 * composes a request the same way. What stays here is the vis half: which control a family draws
 * and the unit conversions a date box needs, neither of which is a request-shaping rule.
 */
import type {Meta} from '@tesseradb/client';

export {
  activeCount,
  composeFilters,
  emptyDraft,
  isPopulated,
  type ColumnDraft,
  type FilterDraft,
  type TextMode
} from '@tesseradb/client';

/** Whether a text column's published operands include `phrase`, so the mode may offer it. */
export function offersPhrase(
  operands: {column: string; operands: string[]}[],
  column: string
): boolean {
  return operands.find((o) => o.column === column)?.operands.includes('phrase') ?? false;
}

/**
 * A numeric column's control units.
 *
 * `timestamp_us` gets date inputs, because a microsecond epoch count is not a thing anyone types.
 * The conversion is here rather than in the panel so that the value the draft holds is always the
 * column's own unit — the panel formats, and nothing downstream has to know which columns are dates.
 */
export function isDateColumn(meta: Meta | null, column: string): boolean {
  return meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us';
}

/** yyyy-mm-dd to microseconds since the epoch, or null for an unparseable or empty box. */
export function dateToMicros(value: string): number | null {
  if (!value) return null;
  const ms = Date.parse(`${value}T00:00:00Z`);
  return Number.isFinite(ms) ? ms * 1000 : null;
}

/** Microseconds since the epoch to the yyyy-mm-dd an input[type=date] wants. */
export function microsToDate(micros: number | null): string {
  if (micros === null) return '';
  return new Date(micros / 1000).toISOString().slice(0, 10);
}
