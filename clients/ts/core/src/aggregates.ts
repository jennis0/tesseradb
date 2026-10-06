import {retryDelayMs, type Clock, type RetryOptions} from './driver.js';
import {PartialAggregate} from './aggregate.js';
import {TesseraError} from './client.js';
import {refusalOf, type Refusal} from './presented.js';
import type {AggregateRequest, AggregateResult, AggregateTable, FilterExpr, Grouping} from './types.js';

/**
 * A grouping as an {@link AggregateSpec} names it: a {@link Grouping}, or one ranking a `nested` or
 * `dag` layer's top artifacts at `cut: 'drawn'`, the cut the store's map draws that layer at: the
 * artifact channel's tile depth and box for the camera, under {@link Store.setClusterBudget}'s
 * budget. Each request takes the cut as it stands when it is sent.
 *
 * @category Store
 */
export type AggregateSpecGrouping = Grouping | DrawnGrouping;

/** A grouping ranking a tree layer's top artifacts at the cut the store's map draws. */
type DrawnGrouping = Omit<Grouping, 'by'> & {by: {layer: string; top: number; cut: 'drawn'}};

/** Whether `grouping` ranks at the cut the store's map draws. */
export function isDrawn(grouping: AggregateSpecGrouping): grouping is DrawnGrouping {
  return grouping.by !== undefined && 'cut' in grouping.by && grouping.by.cut === 'drawn';
}

/**
 * What a component registers with {@link Store.setAggregate}: the groupings to count, the set they
 * are counted over, and the comparison set where it wants one. The store sends its own filters and
 * selected region as the request's `filters`.
 *
 * @category Store
 */
export type AggregateSpec = {
  /** One table each, in this order. */
  groupings: AggregateSpecGrouping[];
  /**
   * The set the counts are taken over.
   *
   * - `match`, the default: what the store's filters admit, under the selected region.
   * - `view`: the same, within the area the counts in view cover: the camera's box, or the
   *   selected region while one is selected. A pan asks again only once the camera has rested for
   *   250 ms; a change of filter, highlight or selection, or a view switch, asks at once over the
   *   area as it stands. A `reference` is limited to the same area, so a lift compares with what
   *   is in it.
   * - `visible`: every item this viewer may see in the view. No filters are sent, so `without`,
   *   `withoutMembersOf` and `highlighted` change nothing.
   */
  subject?: 'match' | 'view' | 'visible';
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
   * `loading` from the moment the store asks until the answer lands, and for a `view` aggregate
   * from its registration until the camera has rested; `retrying` while a `429` or `503` waits to
   * be sent again; `shown` once the answer has landed; and `refused` where the request failed.
   */
  status: 'loading' | 'retrying' | 'shown' | 'refused';
  /**
   * The last answer. While the next request is loading, the answer to the previous one stays here.
   * It is `null` before the first answer, after a refusal, after a view switch and once the store
   * forgets what the server answered. A histogram asked for with a `sample` says in its table's
   * `sample` whether its counts were scaled and how many items were counted, and its `total` is
   * the size of the set.
   */
  result: AggregateResult | null;
  /** The view `result` was counted in; `null` where `result` is. */
  view: string | null;
  /** The refusal of the last request, else `null`. */
  refusal: Refusal | null;
  /**
   * One per grouping `result` answers, in order: the field's figures where the grouping is a
   * `summary`, else `null`. Empty where `result` is `null`.
   */
  summaries: readonly (FieldSummary | null)[];
};

/**
 * A `summary` grouping's one row: the figures of a number or timestamp field over every item this
 * viewer may see in the view, whatever the filters.
 *
 * @category Projections
 */
export type FieldSummary = {
  /** The items counted. */
  items: bigint;
  /** How many of them hold a finite value. */
  count: bigint;
  /** How many hold no value. */
  none: bigint;
  /**
   * The smallest finite value, typed as a histogram's edge: a `bigint` on an integer field, a
   * number on a float field, and milliseconds since the Unix epoch on a timestamp field. `null`
   * where `count` is 0.
   */
  min: number | bigint | null;
  /** The largest finite value, typed as `min`. */
  max: number | bigint | null;
  /** The mean of the finite values, in milliseconds since the Unix epoch on a timestamp field. `null` where `count` is 0. */
  mean: number | null;
};

/**
 * The `aggregates` projection: each registered aggregate by the id it was registered under.
 *
 * @category Projections
 */
export type AggregatesProjection = ReadonlyMap<string, AggregateEntry>;

/** An aggregate request less its view, which is the store's at the moment it is sent. */
export type AggregateBody = Omit<AggregateRequest, 'view'>;

/** `body` as JSON, an artifact id as its decimal string. */
function bodyKey(body: AggregateBody): string {
  return JSON.stringify(body, (_k, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));
}

/** The figures of each summary grouping of `spec` that `result` answers. */
function summariesOf(spec: AggregateSpec, result: AggregateResult): (FieldSummary | null)[] {
  return spec.groupings.map((grouping, i) => {
    const table = result.tables.find((t) => t.grouping === i);
    return grouping.by !== undefined && 'summary' in grouping.by && table !== undefined ? summaryOf(table) : null;
  });
}

function summaryOf(table: AggregateTable): FieldSummary | null {
  if (table.rows.numRows === 0) return null;
  const at = (name: string): unknown => table.rows.getChild(name)?.get(0) ?? null;
  const [items, count, none] = [at('items'), at('count'), at('none')];
  if (typeof items !== 'bigint' || typeof count !== 'bigint' || typeof none !== 'bigint') return null;
  const edge = (v: unknown) => (typeof v === 'bigint' || typeof v === 'number' ? v : null);
  const mean = at('mean');
  return {items, count, none, min: edge(at('min')), max: edge(at('max')), mean: typeof mean === 'number' ? mean : null};
}

/**
 * The registered aggregates: one request each in flight at most, a request superseded by a newer
 * one for the same id being aborted, and an answer landing only while it answers the latest ask. A
 * `429` or `503` is sent again after {@link retryDelayMs}; a newer ask cancels the wait.
 *
 * `compose` gives the request a spec sends now, less its view, or `null` where it cannot be sent
 * yet. A request is composed once `ready` has resolved, so it names what the `/v1/meta` read then
 * offers, and `ask` sends it; a retry composes it again. A refresh that asks only where the request
 * changed compares what `compose` gives at the refresh with the request last sent.
 */
export class Aggregates {
  private readonly specs = new Map<string, AggregateSpec>();
  private readonly asking = new Map<string, AbortController>();
  /** Per id: the timer of a retry waiting to be sent. */
  private readonly waiting = new Map<string, unknown>();
  /** Per id: the request last sent, as JSON. */
  private readonly sent = new Map<string, string>();
  private entries: Map<string, AggregateEntry> = new Map();
  private disposed = false;

  constructor(
    private readonly ready: () => Promise<unknown>,
    private readonly ask: (spec: AggregateSpec, body: AggregateBody, signal: AbortSignal) => Promise<{result: AggregateResult; view: string}>,
    private readonly publish: (entries: AggregatesProjection) => void,
    private readonly clock: Clock,
    private readonly retry: RetryOptions,
    private readonly compose: (spec: AggregateSpec) => AggregateBody | null
  ) {}

  /**
   * Register `spec` under `id`, replacing what it held, and ask, or with `now` false hold it as
   * loading until a refresh asks; `null` drops the id.
   */
  set(id: string, spec: AggregateSpec | null, now = true): void {
    if (this.disposed) return;
    if (spec === null) {
      this.stop(id);
      this.specs.delete(id);
      this.sent.delete(id);
      if (this.entries.delete(id)) this.replace(new Map(this.entries));
      return;
    }
    this.specs.set(id, spec);
    this.run([id], true, false, now);
  }

  /**
   * Ask again for the registered aggregates `which` selects, every one by default. With `drop`,
   * the answers held are dropped at once, as after a view switch; without it they stay until the
   * new ones land. With `changed`, an aggregate whose request is the one last sent, and was not
   * refused, is left as it is.
   */
  refresh(drop: boolean, options: {which?: (spec: AggregateSpec) => boolean; changed?: boolean} = {}): void {
    if (this.disposed) return;
    const which = options.which ?? (() => true);
    this.run(
      [...this.specs].filter(([, spec]) => which(spec)).map(([id]) => id),
      drop,
      options.changed ?? false,
      true
    );
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

  private run(ids: string[], drop: boolean, changed: boolean, now: boolean): void {
    const next = new Map(this.entries);
    const asked: string[] = [];
    for (const id of ids) {
      const body = now ? this.compose(this.specs.get(id)!) : null;
      const key = body === null ? null : bodyKey(body);
      const held = next.get(id);
      if (changed && key !== null && key === this.sent.get(id) && held?.status !== 'refused') continue;
      // Still waiting to be sendable, as it was.
      if (changed && key === null && !this.sent.has(id) && held?.status === 'loading') continue;
      this.stop(id);
      const kept = drop ? null : (held?.result ?? null);
      next.set(id, {
        status: 'loading',
        result: kept,
        view: kept === null ? null : (held?.view ?? null),
        refusal: null,
        summaries: kept === null ? [] : (held?.summaries ?? [])
      });
      if (key === null) {
        this.sent.delete(id);
      } else {
        this.sent.set(id, key);
        asked.push(id);
      }
    }
    if (ids.every((id) => next.get(id) === this.entries.get(id))) return;
    this.replace(next);
    for (const id of asked) void this.one(id, this.specs.get(id)!, 0);
  }

  private async one(id: string, spec: AggregateSpec, attempt: number): Promise<void> {
    const controller = new AbortController();
    this.asking.set(id, controller);
    let entry: AggregateEntry;
    let wait: number | null = null;
    try {
      await this.ready();
      if (this.asking.get(id) !== controller || controller.signal.aborted) return;
      const body = this.compose(spec);
      if (body === null) {
        // Not sendable under the meta just read, as a `view` aggregate with no camera; it stays
        // loading until a refresh finds it sendable.
        this.asking.delete(id);
        this.sent.delete(id);
        return;
      }
      this.sent.set(id, bodyKey(body));
      const {result, view} = await this.ask(spec, body, controller.signal);
      entry = {status: 'shown', result, view, refusal: null, summaries: summariesOf(spec, result)};
    } catch (error) {
      wait = retryDelayMs(error, attempt, this.retry);
      const held = this.entries.get(id);
      entry =
        wait === null
          ? {status: 'refused', result: null, view: null, refusal: refusalOf(error), summaries: []}
          : {status: 'retrying', result: held?.result ?? null, view: held?.view ?? null, refusal: refusalOf(error), summaries: held?.summaries ?? []};
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

/** The parts waiting to be sent as one request. */
type Batch = {token: string; req: Omit<AggregateRequest, 'groupings'>; parts: Part[]};

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
 * request is aborted once every caller's signal has. Where a part would take a request past
 * `maxGroupings`, that request is closed to further parts and the part starts the next one; a part
 * past it on its own is sent alone. Where the joined request is refused as a contract error or cut
 * short, each part is sent again alone, so one caller's grouping does not refuse the others.
 *
 * @internal
 */
export function joinedAggregate(
  send: (token: string, req: AggregateRequest, signal: AbortSignal) => Promise<AggregateResult>,
  maxGroupings: () => number
): (token: string, req: AggregateRequest, signal: AbortSignal) => Promise<AggregateResult> {
  /** The batch still open to parts, by the request its parts share. */
  const open = new Map<string, Batch>();

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

  const flush = (key: string, batch: Batch): void => {
    if (open.get(key) === batch) open.delete(key);
    const live = batch.parts.filter((p) => !p.signal.aborted);
    if (live.length === 0) return;
    if (live.length === 1) return alone(batch.token, batch.req, live[0]!);
    const controller = new AbortController();
    let left = live.length;
    for (const part of live) {
      part.dropped = () => {
        if (--left === 0) controller.abort();
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
      if (groupings.length > maxGroupings()) return alone(token, rest, part);
      const key = JSON.stringify([token, rest], (_k, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));
      const batch = open.get(key);
      if (batch && batch.parts.reduce((n, p) => n + p.groupings.length, 0) + groupings.length <= maxGroupings()) {
        batch.parts.push(part);
        return;
      }
      // A full batch is sent when its own wait ends.
      const started: Batch = {token, req: rest, parts: [part]};
      open.set(key, started);
      const hop = (left: number): void => (left === 0 ? flush(key, started) : queueMicrotask(() => hop(left - 1)));
      hop(JOIN_MICROTASKS);
    });
}
