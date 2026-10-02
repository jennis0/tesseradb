import {retryDelayMs, type Clock, type RetryOptions} from './driver.js';
import {PartialAggregate} from './aggregate.js';
import {TesseraError} from './client.js';
import {refusalOf, type Refusal} from './presented.js';
import type {AggregateRequest, AggregateResult, FilterExpr, Grouping} from './types.js';

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
  /**
   * A column whose control in the store's filter position is left out of the request's `filters`,
   * named as the filter draft keys it. The `member_of` clauses, unless `withoutMembersOf` names
   * their layer, and the selected region are always sent. Unset leaves nothing out.
   */
  without?: string;
  /**
   * A layer whose `member_of` clauses in the store's filter position are left out of the request's
   * `filters`, so a control listing that layer's artifacts keeps counting the ones its clauses
   * exclude. The other layers' clauses and the selected region are still sent. Unset leaves
   * nothing out.
   */
  withoutMembersOf?: string;
  /**
   * Whether the counts are taken over the items that also satisfy the store's highlight: the
   * controls and `member_of` clauses in the highlight position are joined to `filters` by `all_of`,
   * so each count is the viewport's `highlighted` count over the same items. Unset, or with no
   * highlight set, `filters` is sent as it stands.
   */
  highlighted?: boolean;
};

/**
 * One registered aggregate, as the `aggregates` projection holds it.
 *
 * @category Projections
 */
export type AggregateEntry = {
  /**
   * `loading` from the moment the store asks until the answer lands, `retrying` while a `429` or
   * `503` waits to be sent again, `shown` once the answer has landed, and `refused` where the
   * request failed.
   */
  status: 'loading' | 'retrying' | 'shown' | 'refused';
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
 * one for the same id being aborted, and an answer landing only while it answers the latest ask. A
 * `429` or `503` is sent again after {@link retryDelayMs}; a newer ask cancels the wait.
 */
export class Aggregates {
  private readonly specs = new Map<string, AggregateSpec>();
  private readonly asking = new Map<string, AbortController>();
  /** Per id: the timer of a retry waiting to be sent. */
  private readonly waiting = new Map<string, unknown>();
  private entries: Map<string, AggregateEntry> = new Map();
  private disposed = false;

  constructor(
    private readonly ask: (spec: AggregateSpec, signal: AbortSignal) => Promise<{result: AggregateResult; view: string}>,
    private readonly publish: (entries: AggregatesProjection) => void,
    private readonly clock: Clock,
    private readonly retry: RetryOptions
  ) {}

  /** Register `spec` under `id`, replacing what it held, and ask; `null` drops the id. */
  set(id: string, spec: AggregateSpec | null): void {
    if (this.disposed) return;
    if (spec === null) {
      this.stop(id);
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
    for (const id of [...this.asking.keys(), ...this.waiting.keys()]) this.stop(id);
  }

  /** Abort `id`'s request in flight and cancel its waiting retry. */
  private stop(id: string): void {
    this.asking.get(id)?.abort();
    this.asking.delete(id);
    const timer = this.waiting.get(id);
    if (timer !== undefined) this.clock.cancel(timer);
    this.waiting.delete(id);
  }

  private run(ids: string[], drop: boolean): void {
    if (ids.length === 0) return;
    const next = new Map(this.entries);
    for (const id of ids) {
      this.stop(id);
      const held = next.get(id);
      const kept = drop ? null : (held?.result ?? null);
      next.set(id, {status: 'loading', result: kept, view: kept === null ? null : (held?.view ?? null), refusal: null});
    }
    this.replace(next);
    for (const id of ids) void this.one(id, this.specs.get(id)!, 0);
  }

  private async one(id: string, spec: AggregateSpec, attempt: number): Promise<void> {
    const controller = new AbortController();
    this.asking.set(id, controller);
    let entry: AggregateEntry;
    let wait: number | null = null;
    try {
      const {result, view} = await this.ask(spec, controller.signal);
      entry = {status: 'shown', result, view, refusal: null};
    } catch (error) {
      wait = retryDelayMs(error, attempt, this.retry);
      const held = this.entries.get(id);
      entry =
        wait === null
          ? {status: 'refused', result: null, view: null, refusal: refusalOf(error)}
          : {status: 'retrying', result: held?.result ?? null, view: held?.view ?? null, refusal: refusalOf(error)};
    }
    // A newer ask for this id, a drop or a dispose has taken its place.
    if (this.asking.get(id) !== controller || controller.signal.aborted) return;
    this.asking.delete(id);
    const next = new Map(this.entries);
    next.set(id, entry);
    this.replace(next);
    if (wait === null) return;
    this.waiting.set(
      id,
      this.clock.after(wait, () => {
        this.waiting.delete(id);
        if (!this.disposed && this.specs.get(id) === spec) void this.one(id, spec, attempt + 1);
      })
    );
  }

  private replace(next: Map<string, AggregateEntry>): void {
    this.entries = next;
    this.publish(next);
  }
}

/** One caller's share of a joined request; `dropped` is set once the request is sent. */
type Part = {
  groupings: Grouping[];
  signal: AbortSignal;
  resolve: (result: AggregateResult) => void;
  reject: (error: unknown) => void;
  dropped: (() => void) | null;
};

/**
 * How many microtasks a request waits for others to join it. Elements that answer one store change
 * update one after another within the same task, each in a microtask of its own, and their requests
 * arrive within this many of each other. Counting microtasks rather than waiting for a timer keeps
 * the join independent of the clock.
 */
const JOIN_MICROTASKS = 64;

/**
 * Sends aggregate requests, joining those that differ only in their groupings into one request
 * when they are asked for together. Elements that answer one store change each register their own
 * aggregate, and over the same filters they are one read of the same set. Each caller gets its own
 * tables back, numbered from 0. A caller whose signal aborts is refused at once, and the joined
 * request is aborted once every caller's signal has. One whose groupings would pass `maxGroupings`
 * is sent alone. Where the joined request is refused as a contract error or cut short, each part is
 * sent again alone, so one caller's grouping does not refuse the others.
 *
 * @internal
 */
export function joinedAggregate(
  send: (token: string, req: AggregateRequest, signal: AbortSignal) => Promise<AggregateResult>,
  maxGroupings: () => number
): (token: string, req: AggregateRequest, signal: AbortSignal) => Promise<AggregateResult> {
  const waiting = new Map<string, {token: string; req: Omit<AggregateRequest, 'groupings'>; parts: Part[]}>();

  // A send that throws rather than rejecting is that caller's refusal all the same.
  const sent = (token: string, req: AggregateRequest, signal: AbortSignal): Promise<AggregateResult> => {
    try {
      return send(token, req, signal);
    } catch (error) {
      return Promise.reject(error);
    }
  };

  const alone = (token: string, req: Omit<AggregateRequest, 'groupings'>, part: Part): void => {
    if (!part.signal.aborted) sent(token, {...req, groupings: part.groupings}, part.signal).then(part.resolve, part.reject);
  };

  const flush = (key: string): void => {
    const batch = waiting.get(key)!;
    waiting.delete(key);
    const live = batch.parts.filter((p) => !p.signal.aborted);
    if (live.length === 0) return;
    if (live.length === 1) return alone(batch.token, batch.req, live[0]!);
    const controller = new AbortController();
    let open = live.length;
    for (const part of live) {
      part.dropped = () => {
        if (--open === 0) controller.abort();
      };
    }
    sent(batch.token, {...batch.req, groupings: live.flatMap((p) => p.groupings)}, controller.signal).then(
      (result) => {
        let from = 0;
        for (const part of live) {
          const own = (t: AggregateResult['tables'][number]) => t.grouping >= from && t.grouping < from + part.groupings.length;
          const tables = result.tables.filter(own).map((t) => ({...t, grouping: t.grouping - from}));
          from += part.groupings.length;
          part.resolve({...result, tables});
        }
      },
      (error: unknown) => {
        const separately = error instanceof PartialAggregate || (error instanceof TesseraError && error.status === 422);
        for (const part of live) {
          if (separately) alone(batch.token, batch.req, part);
          else part.reject(error);
        }
      }
    );
  };

  return (token, req, signal) =>
    new Promise<AggregateResult>((resolve, reject) => {
      if (signal.aborted) return reject(signal.reason);
      const {groupings, ...rest} = req;
      const part: Part = {groupings, signal, resolve, reject, dropped: null};
      signal.addEventListener(
        'abort',
        () => {
          reject(signal.reason);
          part.dropped?.();
        },
        {once: true}
      );
      const key = JSON.stringify([token, rest], (_k, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));
      const batch = waiting.get(key);
      if (batch && batch.parts.reduce((n, p) => n + p.groupings.length, 0) + groupings.length > maxGroupings()) return alone(token, rest, part);
      if (batch) {
        batch.parts.push(part);
        return;
      }
      waiting.set(key, {token, req: rest, parts: [part]});
      const hop = (left: number): void => (left === 0 ? flush(key) : queueMicrotask(() => hop(left - 1)));
      hop(JOIN_MICROTASKS);
    });
}
