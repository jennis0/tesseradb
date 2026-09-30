import type {Clock} from './driver.js';
import type {ClauseVerb} from './filters.js';
import {refusalOf, type Refusal} from './presented.js';
import type {FilterExpr, SuggestResult, SuggestValue} from './types.js';

/** How long a column's typeahead waits after a keystroke before it asks. */
const SUGGEST_DEBOUNCE_MS = 120;

/**
 * The floor on a `shed` retry's delay. `retry_after_s` may be `0`, and a filter panel's controls
 * all ask in the same tick, so without a floor they would collide again on every retry.
 */
const SUGGEST_MIN_RETRY_S = 0.25;

/** How many `shed` retries one ask gets before it is given up. */
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
   * The refusal per column for its last ask, which replaces the column's page. An ask typed into
   * the box that the server still sheds after five retries is published here as `backpressure`,
   * with the server's detail.
   */
  suggestErrors: Record<string, Refusal>;
  /**
   * Moves each time every page, refusal and pending ask is dropped (at a view switch and at
   * {@link Store.clear}), so a control can tell that its page was dropped.
   */
  suggestEpoch: number;
};

/** What one column last asked for. */
type Want = {
  q: string;
  verb: ClauseVerb;
  /** The filter the counts are taken under, as sent or about to be; `null` before the first send. */
  sent: string | null;
  /** Whether the ask came from a change of filter rather than from the box; such an ask never reports being shed. */
  refresh: boolean;
};

/**
 * The category typeahead. Each column's ask waits out a debounce, then joins a queue; the session
 * has one suggestion running at a time on the server, so the queue sends one request at a time.
 * An ask replaces its column's queued or running one, and the running request is cancelled. A
 * response lands only while it answers its column's latest ask.
 */
export class Suggestions {
  private state: SuggestState = {suggestions: {}, suggestErrors: {}, suggestEpoch: 0};
  private readonly want = new Map<string, Want>();
  /** Per column: the debounce or retry timer armed for its ask. */
  private readonly timers = new Map<string, unknown>();
  /** The columns whose ask is due, oldest first. */
  private readonly queue: string[] = [];
  /** The request in flight, if any. */
  private running: {column: string; turn: number; request: AbortController} | null = null;
  /** Per column: the number of its latest ask. A response to any other is dropped. */
  private readonly latest = new Map<string, number>();
  /** Per column: retries spent on its latest ask. */
  private readonly retries = new Map<string, number>();
  private asked = 0;
  private disposed = false;

  constructor(
    private readonly clock: Clock,
    /** The filter an ask from `verb` counts `column` under, as things stand. */
    private readonly filtersFor: (column: string, verb: ClauseVerb) => FilterExpr | null,
    private readonly ask: (column: string, q: string, filters: FilterExpr | null, signal: AbortSignal) => Promise<SuggestResult>,
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
    if (held && held.q === q && held.verb === verb) {
      // A box that asks again what a refresh is asking takes the refresh over, so a shed is reported.
      held.refresh = false;
      return;
    }
    this.want.set(column, {q, verb, sent: null, refresh: false});
    this.again(column, SUGGEST_DEBOUNCE_MS);
  }

  /**
   * The filter changed: ask again each column whose request would now carry another filter. A
   * column keeps its page until the new one lands, and a refresh the server sheds leaves it.
   */
  refresh(): void {
    if (this.disposed) return;
    for (const [column, held] of this.want) {
      // An ask not sent yet composes its filter when it is sent.
      if (held.sent === null || JSON.stringify(this.filtersFor(column, held.verb)) === held.sent) continue;
      held.sent = null;
      held.refresh = true;
      this.again(column, SUGGEST_DEBOUNCE_MS);
    }
  }

  /** Stop asking for `column` and drop its page and refusal: its box emptied or its control went away. */
  forget(column: string): void {
    if (!this.want.has(column) && !(column in this.state.suggestions) && !(column in this.state.suggestErrors)) return;
    this.cancel(column);
    this.want.delete(column);
    this.set({...this.state, suggestions: without(this.state.suggestions, column), suggestErrors: without(this.state.suggestErrors, column)});
  }

  /**
   * Drop every page, refusal and pending ask. A page answers one view under one mask, so a view
   * switch or a re-authorise calls this. The epoch moves even with nothing held.
   */
  reset(): void {
    this.queue.length = 0;
    for (const column of [...this.want.keys()]) this.cancel(column);
    this.want.clear();
    this.set({suggestions: {}, suggestErrors: {}, suggestEpoch: this.state.suggestEpoch + 1});
  }

  dispose(): void {
    this.disposed = true;
    for (const column of [...this.want.keys()]) this.cancel(column);
  }

  /** Cancel `column`'s timer, queue place and request; a cancelled request frees the queue at once. */
  private cancel(column: string): void {
    const timer = this.timers.get(column);
    if (timer !== undefined) this.clock.cancel(timer);
    this.timers.delete(column);
    const at = this.queue.indexOf(column);
    if (at >= 0) this.queue.splice(at, 1);
    this.latest.delete(column);
    this.retries.delete(column);
    if (this.running?.column === column) {
      this.running.request.abort();
      this.running = null;
      this.pump();
    }
  }

  /** Start a new ask for `column`'s wanted `q` after `ms`, replacing the one before it. */
  private again(column: string, ms: number): void {
    this.cancel(column);
    this.latest.set(column, ++this.asked);
    this.arm(column, ms);
  }

  private arm(column: string, ms: number): void {
    this.timers.set(
      column,
      this.clock.after(ms, () => {
        this.timers.delete(column);
        if (!this.queue.includes(column)) this.queue.push(column);
        this.pump();
      })
    );
  }

  private set(state: SuggestState): void {
    this.state = state;
    this.publish(state);
  }

  /** Send the next due ask, where none is running. */
  private pump(): void {
    if (this.disposed || this.running) return;
    const column = this.queue.shift();
    if (column === undefined) return;
    const wanted = this.want.get(column);
    const turn = this.latest.get(column);
    if (!wanted || turn === undefined) return this.pump();
    const filters = this.filtersFor(column, wanted.verb);
    wanted.sent = JSON.stringify(filters);
    const running = {column, turn, request: new AbortController()};
    this.running = running;
    void this.run(column, wanted, filters, running).finally(() => {
      if (this.running === running) this.running = null;
      this.pump();
    });
  }

  private async run(column: string, wanted: Want, filters: FilterExpr | null, running: {turn: number; request: AbortController}): Promise<void> {
    const stale = () => this.disposed || this.latest.get(column) !== running.turn;
    try {
      const result = await this.ask(column, wanted.q, filters, running.request.signal);
      if (stale()) return;
      if (result.status === 'shed') {
        const attempt = (this.retries.get(column) ?? 0) + 1;
        if (attempt <= SUGGEST_MAX_RETRIES) {
          this.retries.set(column, attempt);
          this.arm(column, Math.max(result.retryAfterS, SUGGEST_MIN_RETRY_S) * 1000);
          return;
        }
        this.retries.delete(column);
        this.latest.delete(column);
        // A refresh given up leaves the page it would have replaced, and asks again at the next change.
        if (wanted.refresh) return;
        this.set({
          ...this.state,
          suggestions: without(this.state.suggestions, column),
          suggestErrors: {...this.state.suggestErrors, [column]: {code: 'backpressure', detail: result.detail ?? `shed ${attempt} times`}}
        });
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
