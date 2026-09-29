import {refusalOf, type Refusal} from './presented.js';
import type {AggregateResult, FilterExpr, Grouping} from './types.js';

/**
 * What a component registers with {@link Store.setAggregate}: the groupings to count, and the
 * comparison set where it wants one. The store sends its own filters and selected region as the
 * request's `filters`.
 *
 * @category Store
 */
export type AggregateSpec = {
  /** One table each, in this order. */
  groupings: Grouping[];
  /**
   * The set each count is compared with: `'visible'` for everything this viewer may see in the view,
   * or a filter expression for another set drawn from it. Unset asks for no comparison.
   */
  reference?: FilterExpr | 'visible';
};

/**
 * One registered aggregate, as the `aggregates` projection holds it.
 *
 * @category Projections
 */
export type AggregateEntry = {
  /**
   * `loading` from the moment the store asks until the answer lands, `shown` once it has, and
   * `refused` where the request failed.
   */
  status: 'loading' | 'shown' | 'refused';
  /**
   * The last answer. While a request asked for new filters or a new selection is loading, the
   * answer to the previous one stays here. It is `null` before the first answer, after a refusal,
   * after a view switch and once the store forgets what the server answered.
   */
  result: AggregateResult | null;
  /** The view `result` was counted in; `null` where `result` is. */
  view: string | null;
  /** The refusal of the last request, else `null`. */
  refusal: Refusal | null;
};

/**
 * The `aggregates` projection: each registered aggregate by the id it was registered under.
 *
 * @category Projections
 */
export type AggregatesProjection = ReadonlyMap<string, AggregateEntry>;

/**
 * The registered aggregates: one request each in flight at most, a request superseded by a newer
 * one for the same id being aborted, and an answer landing only while it answers the latest ask.
 */
export class Aggregates {
  private readonly specs = new Map<string, AggregateSpec>();
  private readonly asking = new Map<string, AbortController>();
  private entries: Map<string, AggregateEntry> = new Map();
  private disposed = false;

  constructor(
    private readonly ask: (spec: AggregateSpec, signal: AbortSignal) => Promise<{result: AggregateResult; view: string}>,
    private readonly publish: (entries: AggregatesProjection) => void
  ) {}

  /** Register `spec` under `id`, replacing what it held, and ask; `null` drops the id. */
  set(id: string, spec: AggregateSpec | null): void {
    if (this.disposed) return;
    if (spec === null) {
      this.asking.get(id)?.abort();
      this.asking.delete(id);
      this.specs.delete(id);
      if (this.entries.delete(id)) this.replace(new Map(this.entries));
      return;
    }
    this.specs.set(id, spec);
    this.run([id], true);
  }

  /**
   * Ask again for every registered aggregate. With `drop`, the answers held are dropped at once,
   * as after a view switch; without it they stay until the new ones land.
   */
  refresh(drop: boolean): void {
    if (!this.disposed) this.run([...this.specs.keys()], drop);
  }

  /** Abort every request in flight. Nothing lands afterwards. */
  dispose(): void {
    this.disposed = true;
    for (const controller of this.asking.values()) controller.abort();
    this.asking.clear();
  }

  private run(ids: string[], drop: boolean): void {
    if (ids.length === 0) return;
    const next = new Map(this.entries);
    for (const id of ids) {
      this.asking.get(id)?.abort();
      const held = next.get(id);
      const kept = drop ? null : (held?.result ?? null);
      next.set(id, {status: 'loading', result: kept, view: kept === null ? null : (held?.view ?? null), refusal: null});
    }
    this.replace(next);
    for (const id of ids) void this.one(id, this.specs.get(id)!);
  }

  private async one(id: string, spec: AggregateSpec): Promise<void> {
    const controller = new AbortController();
    this.asking.set(id, controller);
    let entry: AggregateEntry;
    try {
      const {result, view} = await this.ask(spec, controller.signal);
      entry = {status: 'shown', result, view, refusal: null};
    } catch (error) {
      entry = {status: 'refused', result: null, view: null, refusal: refusalOf(error)};
    }
    // A newer ask for this id, a drop or a dispose has taken its place.
    if (this.asking.get(id) !== controller || controller.signal.aborted) return;
    this.asking.delete(id);
    const next = new Map(this.entries);
    next.set(id, entry);
    this.replace(next);
  }

  private replace(next: Map<string, AggregateEntry>): void {
    this.entries = next;
    this.publish(next);
  }
}
