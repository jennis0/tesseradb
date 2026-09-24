import type {Clock} from './driver.js';
import {refusalOf, type Refusal} from './presented.js';
import type {SuggestResult, SuggestValue} from './types.js';

/** How long a column's typeahead waits after a keystroke before it asks. */
const SUGGEST_DEBOUNCE_MS = 120;

/**
 * The floor on a `superseded` retry's delay. `retry_after_s` may be `0`, and a filter panel's
 * controls all ask in the same tick, so without a floor they would collide again on every retry.
 */
const SUGGEST_MIN_RETRY_S = 0.25;

/** How many `superseded` retries one `q` gets before it is published as a `backpressure` refusal. */
const SUGGEST_MAX_RETRIES = 5;

/**
 * The category typeahead's state, which the store publishes as part of the `filters` projection.
 * {@link Store.suggest} fills it.
 *
 * @category Projections
 */
export type SuggestState = {
  /**
   * The last page that landed per column: the `q` it answers, the values this viewer can see, and
   * `more`, which is true where the server stopped before running out of matches.
   */
  suggestions: Record<string, {q: string; values: SuggestValue[]; more: boolean}>;
  /**
   * The refusal per column for its last ask, which replaces the column's page. An ask the server
   * still sheds as `superseded` after five retries is published here as `backpressure`.
   */
  suggestErrors: Record<string, Refusal>;
  /**
   * Moves each time every page, refusal and pending ask is dropped (at a view switch and at
   * {@link Store.clear}), so a control can tell that its page was dropped.
   */
  suggestEpoch: number;
};

/**
 * The category typeahead: one debounced ask per column, retried when the session sheds it as
 * `superseded`, and landed only while it still answers the `q` last asked for.
 */
export class Suggestions {
  private state: SuggestState = {suggestions: {}, suggestErrors: {}, suggestEpoch: 0};
  /** Per column: the debounce or retry timer armed for the latest ask. */
  private readonly timers = new Map<string, unknown>();
  /** Per column: the `q` last asked for. A response echoing another `q` is dropped. */
  private readonly want = new Map<string, string>();
  /** Per column: `superseded` retries spent on the current `q`. */
  private readonly retries = new Map<string, number>();
  private disposed = false;

  constructor(
    private readonly clock: Clock,
    private readonly ask: (column: string, q: string) => Promise<SuggestResult>,
    private readonly publish: (state: SuggestState) => void
  ) {}

  /**
   * Ask `column` for `q` after the debounce. Asking again for the `q` already asked for does
   * nothing, so a caller that re-asks on every store tick cannot hold the debounce off.
   */
  suggest(column: string, q: string): void {
    if (this.disposed) return;
    if (this.want.get(column) === q) return;
    const pending = this.timers.get(column);
    if (pending !== undefined) this.clock.cancel(pending);
    this.want.set(column, q);
    this.retries.delete(column);
    this.arm(column, q, SUGGEST_DEBOUNCE_MS);
  }

  /**
   * Drop every page, refusal and pending ask. A page answers one view under one mask, so a view
   * switch or a re-authorise calls this. The epoch moves even with nothing held, because a request
   * in flight across the reset must not land, even where a fresh ask repeats its `q`.
   */
  reset(): void {
    for (const timer of this.timers.values()) this.clock.cancel(timer);
    this.timers.clear();
    this.want.clear();
    this.retries.clear();
    this.set({suggestions: {}, suggestErrors: {}, suggestEpoch: this.state.suggestEpoch + 1});
  }

  dispose(): void {
    this.disposed = true;
    for (const timer of this.timers.values()) this.clock.cancel(timer);
    this.timers.clear();
  }

  private arm(column: string, q: string, ms: number): void {
    this.timers.set(
      column,
      this.clock.after(ms, () => {
        this.timers.delete(column);
        void this.run(column, q);
      })
    );
  }

  private set(state: SuggestState): void {
    this.state = state;
    this.publish(state);
  }

  private async run(column: string, q: string): Promise<void> {
    if (this.disposed) return;
    const epoch = this.state.suggestEpoch;
    const stale = () => this.disposed || this.want.get(column) !== q || this.state.suggestEpoch !== epoch;
    try {
      const result = await this.ask(column, q);
      if (stale()) return;
      if (result.status === 'superseded') {
        const attempt = (this.retries.get(column) ?? 0) + 1;
        if (attempt > SUGGEST_MAX_RETRIES) {
          this.retries.delete(column);
          this.set({
            ...this.state,
            suggestions: without(this.state.suggestions, column),
            suggestErrors: {
              ...this.state.suggestErrors,
              [column]: {code: 'backpressure', detail: `still superseded after ${SUGGEST_MAX_RETRIES} retries`}
            }
          });
          return;
        }
        this.retries.set(column, attempt);
        this.arm(column, q, Math.max(result.retryAfterS, SUGGEST_MIN_RETRY_S) * 1000);
        return;
      }
      this.retries.delete(column);
      // A landed page replaces the column's refusal, and a refusal its page.
      this.set({
        ...this.state,
        suggestions: {...this.state.suggestions, [column]: {q: result.q, values: result.values, more: result.more}},
        suggestErrors: without(this.state.suggestErrors, column)
      });
    } catch (error) {
      if (stale()) return;
      this.set({
        ...this.state,
        suggestions: without(this.state.suggestions, column),
        suggestErrors: {...this.state.suggestErrors, [column]: refusalOf(error)}
      });
    }
  }
}

function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  if (!(key in record)) return record;
  const next = {...record};
  delete next[key];
  return next;
}
