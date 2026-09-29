import type {Composition} from './compose.js';
import {ValueReservoir, countCodesCached, countCodesInPiece, extendRanks, lacksValues, sizesPoints, widenDomain, widenDomainOver, type Domain, type Ranks, type ValueSample} from './encoding.js';
import type {Masked} from './counts.js';
import {refusalOf, type Refusal} from './presented.js';
import type {CategoryValue, DeclaredScalar} from './types.js';

/**
 * The prefix of a `colourBy` value that names a layer's cluster colours in place of a column, as in
 * `cluster:<layer>`.
 *
 * @category Layers and views
 */
export const CLUSTER_PREFIX = 'cluster:';

/**
 * The legend of the colour and size columns, accumulated from the marks drawn, which are drawn from
 * this viewer's visible set. A category column gets ranks and names, a numeric column a domain, the
 * column sized by whether any point lacks a value and, sized by rank, a sample of its values, and
 * colouring by `cluster:<layer>` accumulates nothing. {@link Store.clear} empties it and keeps `colourBy` and `sizeBy`.
 *
 * @category Projections
 */
export type LegendProjection = {
  /**
   * Palette rank per category code, per column. Codes are ranked by how often they appear among the
   * marks drawn, most frequent first, and a rank once given does not change, so panning does not
   * recolour the map.
   */
  ranks: Record<string, Ranks>;
  /** The numeric range per column, widened as marks arrive and never narrowed. */
  domains: Record<string, Domain>;
  /** A sample of the values drawn per column sized by rank. */
  samples: Record<string, ValueSample>;
  /** Per column sized by, whether a point drawn had no finite value, and so drew as a ring. */
  missing: Record<string, boolean>;
  /** The codes drawn, per category column, with their names as `/v1/categories` resolves them. */
  categories: Record<string, CategoryValue[]>;
  /** The refusal per category column whose names could not be fetched. A refused column is not asked again. */
  categoryErrors: Record<string, Refusal>;
  /** What the points are coloured by, as `setColourBy` last set it: a column, `cluster:<layer>`, or `null` for uniform. */
  colourBy: string | null;
  /** The number column the points are sized by, as `setSizeBy` last set it, or `null` for one size. */
  sizeBy: string | null;
  /**
   * Exact counts per category value, per column, by category key, over the viewer's current set,
   * for the count beside each legend entry. Not built yet: no route serves per-value counts, so the
   * store never sets this and the legend shows no counts. The marks are a sample and give no count.
   */
  counts?: Record<string, Record<string, Masked>>;
};

/** The legend with nothing accumulated and nothing chosen. */
export const EMPTY_LEGEND: LegendProjection = {ranks: {}, domains: {}, samples: {}, missing: {}, categories: {}, categoryErrors: {}, colourBy: null, sizeBy: null};

/**
 * The legend of the colour and size columns, accumulated from the marks drawn: ranks and names for
 * a category, a domain for a number, and a sample of the size column's values. A cluster colour
 * accumulates nothing here, because the vis side resolves it from the session artifact table.
 */
export class Legend {
  private state: LegendProjection = EMPTY_LEGEND;
  /** Whether the size column is sized by rank, and so sampled. */
  private rank = false;
  /** The reservoir behind the size column's sample while it is sized by rank. */
  private reservoir: ValueReservoir | null = null;
  /** The size column's bands already looked at for points with no value. */
  private scanned = new WeakSet<object>();
  /** Moved by {@link clear}, so names asked for before it are not taken. */
  private epoch = 0;
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

  get sizeBy(): string | null {
    return this.state.sizeBy;
  }

  /** Size by `column`, keeping a sample of its values where `rank` is set. */
  setSizeBy(column: string | null, rank = false): void {
    if (column !== this.state.sizeBy) this.scanned = new WeakSet();
    if (column !== this.state.sizeBy || rank !== this.rank) this.reservoir = column !== null && rank ? new ValueReservoir() : null;
    this.rank = rank;
    this.set({...this.state, sizeBy: column});
  }

  /** Extend the legend of the colour and size columns over one frame. */
  accumulate(frame: Composition, columns: readonly DeclaredScalar[]): void {
    const colourBy = this.state.colourBy;
    const colour = colourBy && !colourBy.startsWith(CLUSTER_PREFIX) ? columns.find((c) => c.name === colourBy) : undefined;
    if (colour?.category) this.accumulateCodes(frame, colour.name);
    else if (colour) this.widen(frame, colour.name);
    const size = columns.find((c) => c.name === this.state.sizeBy && sizesPoints(c));
    if (size) {
      if (size !== colour) this.widen(frame, size.name);
      this.scanMissing(frame, size.name);
      if (this.reservoir) this.sample(frame, size.name, this.reservoir);
    }
  }

  /** Rank the codes of a category column drawn in the frame, and ask for the names of new ones. */
  private accumulateCodes(frame: Composition, colourBy: string): void {
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
  }

  /** Widen a number column's domain over the frame's exact bands and stand-ins. */
  private widen(frame: Composition, name: string): void {
    let domain: Domain | null = this.state.domains[name] ?? null;
    for (const band of frame.exact) {
      const values = band.scalars[name];
      if (values) domain = widenDomain(domain, values);
    }
    for (const piece of frame.standIn) {
      const values = piece.band.scalars[name];
      if (values) domain = widenDomainOver(domain, values, piece.indices, piece.limit);
    }
    if (domain && domain !== this.state.domains[name]) {
      this.set({...this.state, domains: {...this.state.domains, [name]: domain}});
    }
  }

  /** Note whether a point drawn lacks a value; a band without the column draws every point as one. */
  private scanMissing(frame: Composition, name: string): void {
    if (this.state.missing[name]) return;
    for (const band of frame.exact) {
      if (this.scanned.has(band)) continue;
      this.scanned.add(band);
      const values = band.scalars[name];
      if (band.ids.length > 0 && (!values || lacksValues(values))) {
        this.set({...this.state, missing: {...this.state.missing, [name]: true}});
        return;
      }
    }
  }

  /** Offer the frame's exact bands to the column's sample, and publish it when it has grown enough. */
  private sample(frame: Composition, name: string, reservoir: ValueReservoir): void {
    for (const band of frame.exact) {
      const values = band.scalars[name];
      if (values) reservoir.offer(values);
    }
    const taken = reservoir.take();
    if (taken) this.set({...this.state, samples: {...this.state.samples, [name]: taken}});
  }

  /** Forget everything accumulated, keeping the colour and size columns. */
  clear(): void {
    this.epoch += 1;
    this.reservoir = this.state.sizeBy !== null && this.rank ? new ValueReservoir() : null;
    this.scanned = new WeakSet();
    this.set({...EMPTY_LEGEND, colourBy: this.state.colourBy, sizeBy: this.state.sizeBy});
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
    const epoch = this.epoch;
    try {
      const resolved = await this.resolve(column, wanted);
      if (this.disposed || epoch !== this.epoch) return;
      const byCode = new Map((this.state.categories[column] ?? []).map((v) => [v.code, v]));
      for (const v of resolved) byCode.set(v.code, v);
      this.set({...this.state, categories: {...this.state.categories, [column]: [...byCode.values()]}});
    } catch (error) {
      if (this.disposed || epoch !== this.epoch) return;
      this.set({...this.state, categoryErrors: {...this.state.categoryErrors, [column]: refusalOf(error)}});
    }
  }
}
