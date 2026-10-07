import type {AggregateEntry, AggregateSpec, AggregateSpecGrouping, AggregateTable, Layer, Meta, Store} from '@tesseradb/client';

/**
 * One listed group of an aggregate table: its key (a vocabulary key for a field, an artifact's
 * `tessera_id` in decimal for a layer), its title (a field value's, else `null`), its count, and
 * on a layer asked with a palette size its slot (else `null`).
 */
export type GroupCount = {key: string; title: string | null; count: number; slot: number | null};

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
  const slot = rows.getChild('slot');
  const count = rows.getChild('count');
  if (!group || !key || !count) return [];
  const out: GroupCount[] = [];
  for (let i = 0; i < rows.numRows; i++) {
    if (group.get(i) !== 'listed') continue;
    const k = key.get(i) as unknown;
    if (k === null || k === undefined) continue;
    const t = title?.get(i) as unknown;
    const n = slot?.get(i) as unknown;
    out.push({key: String(k), title: typeof t === 'string' ? t : null, count: Number(count.get(i) as bigint | number), slot: typeof n === 'number' ? n : null});
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

/** Whether `layer` is a tree, `nested` or `dag`, whose clusters are ranked at the cut the map draws. */
export function isTree(layer: Pick<Layer, 'hierarchy'>): boolean {
  return layer.hierarchy.kind === 'nested' || layer.hierarchy.kind === 'dag';
}

/**
 * The level of a `stacked` or `tiered` layer that is counted and coloured: `chosen` where the layer
 * declares it, else its deepest. `undefined` on a layer with no levels to choose.
 */
export function levelOf(layer: Pick<Layer, 'hierarchy' | 'levels'>, chosen: number | null): number | undefined {
  if ((layer.hierarchy.kind !== 'stacked' && layer.hierarchy.kind !== 'tiered') || layer.levels.length === 0) return undefined;
  return chosen !== null && layer.levels.some((l) => l.level === chosen) ? chosen : layer.levels.at(-1)!.level;
}

/**
 * The grouping ranking a layer's `top` clusters: at the cut the map draws on a tree, else at
 * {@link levelOf} `level`. Each row carries its slot in the palette the map colours clusters from.
 */
export function rankedGrouping(layer: Pick<Layer, 'name' | 'hierarchy' | 'levels'>, top: number, level: number | null): AggregateSpecGrouping {
  if (isTree(layer)) return {by: {layer: layer.name, top, cut: 'drawn', paletteSize: 'drawn'}};
  const at = levelOf(layer, level);
  return {by: {layer: layer.name, ...(at === undefined || !countedByLevel(layer) ? {} : {level: at}), top, paletteSize: 'drawn'}};
}

/**
 * The groupings that count `artifacts` of `layer` by name: one per level where the route addresses
 * the layer by level, else one. The artifacts are taken in the order given, and the first
 * `maxAggregateNamed` of each level and the first `maxAggregateGroupings` levels met are counted,
 * so what is counted is what is listed first. The ids are sorted within a grouping only so that one
 * set always makes one request. With `paletteSize`, each row carries its slot in a palette that size,
 * or with `'drawn'` in the palette the store's map colours clusters from.
 */
export function artifactGroupings(
  layer: Pick<Layer, 'name' | 'levels'>,
  artifacts: readonly {tesseraId: bigint; rung: number}[],
  limits: Meta['selection'],
  paletteSize?: number | 'drawn'
): AggregateSpec['groupings'] {
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
    .map(([level, ids]) => ({
      by: {layer: layer.name, ...(level < 0 ? {} : {level}), artifacts: [...ids].sort((x, y) => (x < y ? -1 : x > y ? 1 : 0)), ...(paletteSize === undefined ? {} : {paletteSize})}
    }));
}

let registered = 0;

/** A spec as one string, so two that ask the same are equal; empty for none. */
function keyOf(spec: AggregateSpec | null): string {
  return spec === null ? '' : JSON.stringify(spec, (_k, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));
}

/**
 * One aggregate an element keeps registered with its store under an id of its own. {@link set}
 * registers a spec only when it differs from the one held, since each registration sends a
 * request; `null` drops it.
 *
 * The registration is made in a microtask after the call. A registration publishes the store's
 * `aggregates` projection at once, and an element calls `set` from its `updated`, so registering
 * there would ask every subscribed element for an update while one is finishing.
 */
export class HeldAggregate {
  readonly id: string;
  private store: Store | null = null;
  private held = '';
  /** The last call's arguments, applied in the microtask. */
  private wanted: {store: Store | null; key: string; spec: AggregateSpec | null} = {store: null, key: '', spec: null};
  private queued = false;

  constructor(prefix: string) {
    registered += 1;
    this.id = `${prefix}#${registered}`;
  }

  /** Register `spec` with `store`, or drop the registration where `spec` is `null` or the store changed. */
  set(store: Store | null, spec: AggregateSpec | null): void {
    const key = keyOf(spec);
    if (store === this.wanted.store && key === this.wanted.key) return;
    this.wanted = {store, key, spec};
    if (this.queued) return;
    this.queued = true;
    queueMicrotask(() => this.apply());
  }

  private apply(): void {
    this.queued = false;
    const {store, key, spec} = this.wanted;
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

  /**
   * The entry `store` holds for this registration while it is registered with `spec`; nothing
   * while another spec is registered or one is waiting to be, so an answer to what was asked
   * before is not read as the answer to `spec`.
   */
  entryOf(store: Store | null, spec: AggregateSpec | null): AggregateEntry | undefined {
    const key = keyOf(spec);
    return key !== '' && key === this.held && key === this.wanted.key ? this.entryFor(store) : undefined;
  }

  /**
   * The entry `store` holds for this registration, or nothing where the registration is held with
   * another store, as in the moment after an element adopts a new one.
   */
  entryFor(store: Store | null): AggregateEntry | undefined {
    return store !== null && store === this.store ? this.entry() : undefined;
  }
}
