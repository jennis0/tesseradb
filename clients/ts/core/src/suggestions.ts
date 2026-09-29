import type {Clock} from './driver.js';
import type {ClauseVerb} from './filters.js';
import {refusalOf, type Refusal} from './presented.js';
import type {SuggestResult, SuggestValue} from './types.js';

/** How long a column's typeahead waits after a keystroke before it asks. */
const SUGGEST_DEBOUNCE_MS = 120;

/**
 * The floor on a `shed` retry's delay. `retry_after_s` may be `0`, and a filter panel's controls
 * all ask in the same tick, so without a floor they would collide again on every retry.
 */
const SUGGEST_MIN_RETRY_S = 0.25;

/** How many `shed` retries one ask gets before it is published as a `backpressure` refusal. */
const SUGGEST_MAX_RETRIES = 5;

/**
 * One column's last landed suggestion page.
 *
 * @category Projections
 */
export type SuggestionPage = {
  /** The `q` the page answers. */
  q: string;
  /**
   * The position of the control that asked, which decides the filter the counts are taken under:
   * in `filter`, the filter without the column's own clause; in `highlight`, the whole filter.
   */
  verb: ClauseVerb;
  /** The values this viewer can see, each with its count. */
  values: SuggestValue[];
  /** True where the server stopped before running out of matches. */
  more: boolean;
  /** The number of items the counts are taken over, which a value's share is out of; `null` where the server sent none. */
  total: number | null;
};

/**
 * The category typeahead's state, which the store publishes as part of the `filters` projection.
 * {@link Store.suggest} fills it.
 *
 * @category Projections
 */
export type SuggestState = {
  /** The last page that landed per column. */
  suggestions: Record<string, SuggestionPage>;
  /**
   * The refusal per column for its last ask, which replaces the column's page. An ask the server
   * still sheds after five retries is published here as `backpressure`, with the server's detail.
   */
  suggestErrors: Record<string, Refusal>;
  /**
   * Moves each time every page, refusal and pending ask is dropped (at a view switch and at
   * {@link Store.clear}), so a control can tell that its page was dropped.
   */
  suggestEpoch: number;
};

/**
 * The category typeahead: one debounced ask per column, retried when the session sheds it, and
 * landed only while it is still the column's latest ask. A newer ask cancels the request of the
 * one before it.
 */
export class Suggestions {
  private state: SuggestState = {suggestions: {}, suggestErrors: {}, suggestEpoch: 0};
  /** Per column: the debounce or retry timer armed for the latest ask. */
  private readonly timers = new Map<string, unknown>();
  /** Per column: the `q` and position last asked for. */
  private readonly want = new Map<string, {q: string; verb: ClauseVerb}>();
  /** Per column: the number of its latest ask. A response to any other is dropped. */
  private readonly latest = new Map<string, number>();
  /** Per column: the request in flight, cancelled when a newer ask replaces it. */
  private readonly inflight = new Map<string, AbortController>();
  /** Per column: retries spent on the latest ask. */
  private readonly retries = new Map<string, number>();
  /** Numbers every ask, across columns and resets, so no two share one. */
  private asked = 0;
  private disposed = false;

  constructor(
    private readonly clock: Clock,
    private readonly ask: (column: string, q: string, verb: ClauseVerb, signal: AbortSignal) => Promise<SuggestResult>,
    private readonly publish: (state: SuggestState) => void
  ) {}

  /**
   * Ask `column` for `q` from a control in position `verb`, after the debounce. Asking again for
   * what was last asked does nothing, so a caller that re-asks on every store tick cannot hold the
   * debounce off.
   */
  suggest(column: string, q: string, verb: ClauseVerb): void {
    if (this.disposed) return;
    const held = this.want.get(column);
    if (held?.q === q && held.verb === verb) return;
    this.want.set(column, {q, verb});
    this.again(column);
  }

  /**
   * Ask every column's latest `q` again, after the debounce. The filter changed, so each count
   * answers another question; a column keeps its page until the new one lands.
   */
  refresh(): void {
    if (this.disposed) return;
    for (const column of this.want.keys()) this.again(column);
  }

  /**
   * Drop every page, refusal and pending ask. A page answers one view under one mask, so a view
   * switch or a re-authorise calls this. The epoch moves even with nothing held.
   */
  reset(): void {
    this.stop();
    this.want.clear();
    this.retries.clear();
    this.set({suggestions: {}, suggestErrors: {}, suggestEpoch: this.state.suggestEpoch + 1});
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
  }

  private stop(): void {
    for (const timer of this.timers.values()) this.clock.cancel(timer);
    this.timers.clear();
    for (const request of this.inflight.values()) request.abort();
    this.inflight.clear();
    this.latest.clear();
  }

  /** Start a new ask for `column`'s wanted `q`, cancelling the one before it. */
  private again(column: string): void {
    const pending = this.timers.get(column);
    if (pending !== undefined) this.clock.cancel(pending);
    this.inflight.get(column)?.abort();
    this.inflight.delete(column);
    this.retries.delete(column);
    this.latest.set(column, ++this.asked);
    this.arm(column, SUGGEST_DEBOUNCE_MS);
  }

  private arm(column: string, ms: number): void {
    this.timers.set(
      column,
      this.clock.after(ms, () => {
        this.timers.delete(column);
        void this.run(column);
      })
    );
  }

  private set(state: SuggestState): void {
    this.state = state;
    this.publish(state);
  }

  private async run(column: string): Promise<void> {
    const wanted = this.want.get(column);
    if (this.disposed || !wanted) return;
    const turn = this.latest.get(column);
    const stale = () => this.disposed || this.latest.get(column) !== turn;
    const request = new AbortController();
    this.inflight.set(column, request);
    try {
      const result = await this.ask(column, wanted.q, wanted.verb, request.signal);
      if (stale()) return;
      this.inflight.delete(column);
      if (result.status === 'shed') {
        const attempt = (this.retries.get(column) ?? 0) + 1;
        if (attempt > SUGGEST_MAX_RETRIES) {
          this.retries.delete(column);
          this.set({
            ...this.state,
            suggestions: without(this.state.suggestions, column),
            suggestErrors: {
              ...this.state.suggestErrors,
              [column]: {code: 'backpressure', detail: result.detail ?? `shed ${attempt} times`}
            }
          });
          return;
        }
        this.retries.set(column, attempt);
        this.arm(column, Math.max(result.retryAfterS, SUGGEST_MIN_RETRY_S) * 1000);
        return;
      }
      this.retries.delete(column);
      // A landed page replaces the column's refusal, and a refusal its page.
      const page: SuggestionPage = {q: result.q, verb: wanted.verb, values: result.values, more: result.more, total: result.total ?? null};
      this.set({
        ...this.state,
        suggestions: {...this.state.suggestions, [column]: page},
        suggestErrors: without(this.state.suggestErrors, column)
      });
    } catch (error) {
      if (stale()) return;
      this.inflight.delete(column);
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
