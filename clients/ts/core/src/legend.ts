import type {Composition} from './compose.js';
import {countCodesCached, countCodesInPiece, extendRanks, widenDomain, widenDomainOver, type Domain, type Ranks} from './encoding.js';
import {refusalOf, type Refusal} from './presented.js';
import type {CategoryValue, DeclaredScalar} from './types.js';

/** The `colourBy` prefix that names a layer's cluster colour rather than a column. */
export const CLUSTER_PREFIX = 'cluster:';

export type LegendProjection = {
  /** Palette rank per code, per column, assigned by observed frequency and never reordered. */
  ranks: Record<string, Ranks>;
  /** Numeric domains per column, widened as marks arrive and never narrowed. */
  domains: Record<string, Domain>;
  /** The codes drawn, per column, with their names. */
  categories: Record<string, CategoryValue[]>;
  categoryErrors: Record<string, Refusal>;
  colourBy: string | null;
};

/**
 * The colour column's legend, accumulated from the marks drawn: ranks and names for a category,
 * a domain for a number. A cluster colour accumulates nothing here, because the vis side resolves
 * it from the session artifact table.
 */
export class Legend {
  private state: LegendProjection = {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null};
  private disposed = false;

  constructor(
    /** Names for category codes, asked for when codes are first drawn. */
    private readonly resolve: (column: string, codes: number[]) => Promise<CategoryValue[]>,
    private readonly publish: (legend: LegendProjection) => void
  ) {}

  get colourBy(): string | null {
    return this.state.colourBy;
  }

  setColourBy(column: string | null): void {
    this.set({...this.state, colourBy: column});
  }

  /** Extend the legend of the colour column over one frame. */
  accumulate(frame: Composition, columns: readonly DeclaredScalar[]): void {
    const colourBy = this.state.colourBy;
    if (!colourBy || colourBy.startsWith(CLUSTER_PREFIX)) return;
    const column = columns.find((c) => c.name === colourBy);
    if (!column) return;
    if (column.category) {
      const counts = new Map<number, number>();
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) for (const [code, n] of countCodesCached(values)) counts.set(code, (counts.get(code) ?? 0) + n);
      }
      // The stand-in pieces seed the ranks while this depth's own bands stream in.
      if (counts.size === 0) for (const piece of frame.standIn) countCodesInPiece(counts, piece, colourBy);
      if (counts.size === 0) return;
      const ranks = extendRanks(this.state.ranks[colourBy] ?? {}, counts);
      if (ranks !== this.state.ranks[colourBy]) {
        this.set({...this.state, ranks: {...this.state.ranks, [colourBy]: ranks}});
        void this.name(colourBy, counts);
      }
    } else {
      let domain: Domain | null = this.state.domains[colourBy] ?? null;
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) domain = widenDomain(domain, values);
      }
      for (const piece of frame.standIn) {
        const values = piece.band.scalars[colourBy];
        if (values) domain = widenDomainOver(domain, values, piece.indices, piece.limit);
      }
      if (domain && domain !== this.state.domains[colourBy]) {
        this.set({...this.state, domains: {...this.state.domains, [colourBy]: domain}});
      }
    }
  }

  /** Forget everything accumulated, keeping the colour column. */
  clear(): void {
    this.set({ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: this.state.colourBy});
  }

  dispose(): void {
    this.disposed = true;
  }

  private set(state: LegendProjection): void {
    this.state = state;
    this.publish(state);
  }

  /** Ask for the names of the codes drawn that are not named yet. A refused column is not asked again. */
  private async name(column: string, counts: Map<number, number>): Promise<void> {
    if (this.state.categoryErrors[column]) return;
    const held = new Set((this.state.categories[column] ?? []).map((v) => v.code));
    const wanted = [...counts.keys()].filter((code) => !held.has(code));
    if (wanted.length === 0) return;
    try {
      const resolved = await this.resolve(column, wanted);
      if (this.disposed) return;
      const byCode = new Map((this.state.categories[column] ?? []).map((v) => [v.code, v]));
      for (const v of resolved) byCode.set(v.code, v);
      this.set({...this.state, categories: {...this.state.categories, [column]: [...byCode.values()]}});
    } catch (error) {
      if (this.disposed) return;
      this.set({...this.state, categoryErrors: {...this.state.categoryErrors, [column]: refusalOf(error)}});
    }
  }
}
