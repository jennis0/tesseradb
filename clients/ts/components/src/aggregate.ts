import type {AggregateEntry, AggregateSpec, AggregateTable, Grouping, Layer, Meta, Store} from '@tesseradb/client';

/**
 * One listed group of an aggregate table: its key (a vocabulary key for a field, an artifact's
 * `tessera_id` in decimal for a layer), its title (a field value's, else `null`) and its count.
 */
export type GroupCount = {key: string; title: string | null; count: number};

/**
 * The listed groups of `table`, in the server's order: largest count first for a `top` grouping,
 * the order named otherwise. The `rest` and `none` rows are left out.
 */
export function listedGroups(table: AggregateTable | undefined): GroupCount[] {
  if (!table) return [];
  const rows = table.rows;
  const group = rows.getChild('group');
  const key = rows.getChild('key');
  const title = rows.getChild('title');
  const count = rows.getChild('count');
  if (!group || !key || !count) return [];
  const out: GroupCount[] = [];
  for (let i = 0; i < rows.numRows; i++) {
    if (group.get(i) !== 'listed') continue;
    const k = key.get(i) as unknown;
    if (k === null || k === undefined) continue;
    const t = title?.get(i) as unknown;
    out.push({key: String(k), title: typeof t === 'string' ? t : null, count: Number(count.get(i) as bigint | number)});
  }
  return out;
}

/** Every listed group's count across the tables of `entry`, by key; `null` before an answer lands. */
export function countsByKey(entry: AggregateEntry | undefined): Map<string, number> | null {
  const result = entry?.result;
  if (!result) return null;
  const out = new Map<string, number>();
  for (const table of result.tables) for (const g of listedGroups(table)) out.set(g.key, g.count);
  return out;
}

/**
 * Whether the aggregate route addresses `layer` by level: it requires a level on a layer that
 * declares several and refuses one on a layer that declares one or none.
 */
export function countedByLevel(layer: Pick<Layer, 'levels'>): boolean {
  return layer.levels.length > 1;
}

/**
 * The groupings that count `artifacts` of `layer` by name: one per level where the route addresses
 * the layer by level, else one. The artifacts are taken in the order given, and the first
 * `maxAggregateNamed` of each level and the first `maxAggregateGroupings` levels met are counted,
 * so what is counted is what is listed first. The ids are sorted within a grouping only so that one
 * set always makes one request.
 */
export function artifactGroupings(layer: Pick<Layer, 'name' | 'levels'>, artifacts: readonly {tesseraId: bigint; rung: number}[], limits: Meta['selection']): Grouping[] {
  const byLevel = new Map<number, bigint[]>();
  for (const a of artifacts) {
    const at = countedByLevel(layer) ? a.rung : -1;
    if (!byLevel.has(at) && byLevel.size >= limits.maxAggregateGroupings) continue;
    const ids = byLevel.get(at) ?? [];
    if (ids.length < limits.maxAggregateNamed && !ids.includes(a.tesseraId)) ids.push(a.tesseraId);
    byLevel.set(at, ids);
  }
  return [...byLevel]
    .sort(([x], [y]) => x - y)
    .map(([level, ids]) => ({by: {layer: layer.name, ...(level < 0 ? {} : {level}), artifacts: [...ids].sort((x, y) => (x < y ? -1 : x > y ? 1 : 0))}}));
}

let registered = 0;

/**
 * One aggregate an element keeps registered with its store under an id of its own. {@link set}
 * registers a spec only when it differs from the one held, since each registration sends a
 * request; `null` drops it.
 */
export class HeldAggregate {
  readonly id: string;
  private store: Store | null = null;
  private held = '';

  constructor(prefix: string) {
    registered += 1;
    this.id = `${prefix}#${registered}`;
  }

  /** Register `spec` with `store`, or drop the registration where `spec` is `null` or the store changed. */
  set(store: Store | null, spec: AggregateSpec | null): void {
    const key = spec === null ? '' : JSON.stringify(spec, (_k, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));
    if (store === this.store && key === this.held) return;
    if (this.store && (this.store !== store || key === '')) this.store.setAggregate(this.id, null);
    this.store = store;
    this.held = key;
    if (store && spec) store.setAggregate(this.id, spec);
    if (!spec) this.store = null;
  }

  /** The entry the store holds for this registration. */
  entry(): AggregateEntry | undefined {
    return this.store?.get('aggregates').get(this.id);
  }
}
