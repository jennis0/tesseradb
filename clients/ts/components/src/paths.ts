import {artifactName, type BrowseRow, type Layer, type Store} from '@mosaicajs/client';
import {isTree} from './aggregate.js';
import {UNNAMED} from './base.js';

/**
 * The names of clusters' parents on a `nested` or `dag` layer, learnt from browse pages: for each
 * cluster asked about, a page of its served parents, and then one of its first parent's. Each is
 * asked for once until {@link reset}, which also drops an answer still loading.
 */
export class ClusterPaths {
  /** Every cluster met on a browse page. */
  private readonly met = new Map<bigint, BrowseRow>();
  /** Each cluster's served parents, once asked for. */
  private readonly parentsOf = new Map<bigint, bigint[]>();
  private readonly asked = new Set<bigint>();
  private epoch = 0;

  /** `changed` is called as each page lands. */
  constructor(private readonly changed: () => void) {}

  reset(): void {
    this.epoch += 1;
    this.met.clear();
    this.parentsOf.clear();
    this.asked.clear();
  }

  /** The rung a cluster was met at, where a page has named it. */
  rungOf(id: bigint): number | undefined {
    return this.met.get(id)?.rung;
  }

  /** Ask for the parents of each of `ids` and of its first parent, on a `nested` or `dag` layer. */
  ask(store: Store, layer: Layer, ids: readonly bigint[]): void {
    if (!isTree(layer)) return;
    const epoch = this.epoch;
    const ask = (id: bigint) => {
      if (this.asked.has(id)) return;
      this.asked.add(id);
      void store
        .browse({layer: layer.name, filters: null, parent: id, limit: 1})
        .then((p) => {
          if (epoch !== this.epoch) return;
          for (const r of p.parents) this.met.set(r.mosaicaId, r);
          this.parentsOf.set(id, p.parents.map((r) => r.mosaicaId));
          this.changed();
        })
        .catch(() => undefined);
    };
    for (const id of ids) {
      ask(id);
      const first = this.parentsOf.get(id)?.[0];
      if (first !== undefined) ask(first);
    }
  }

  /** A cluster's two nearest parents' names, nearest last, as far up as the pages have named them. */
  pathOf(id: bigint): string {
    const names: string[] = [];
    let parents = this.parentsOf.get(id) ?? this.met.get(id)?.parentIds ?? [];
    for (let i = 0; i < 2 && parents.length > 0; i++) {
      const at = this.met.get(parents[0]!);
      if (!at) break;
      names.unshift(artifactName(at) ?? UNNAMED);
      parents = this.parentsOf.get(at.mosaicaId) ?? at.parentIds;
    }
    return names.join(' › ');
  }
}
