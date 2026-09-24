/**
 * The session artifact table: ordinal to artifact, one table per session.
 *
 * An artifact's id is stable across responses, so one table names every artifact the session has
 * held. The ordinal is a `u32` with `0` reserved for none, and it is what each point's membership
 * carries. Naming happens only here: the decode worker cannot share the table, so it emits a
 * response-local index that the main thread remaps.
 *
 * A layer may hold millions of artifacts, so ordinals are reference-counted and reused. Each
 * holder takes a reference per distinct ordinal when built and releases it when dropped; an ordinal
 * at zero returns to a free list. Live ordinals are bounded by what is held, and a lookup texture
 * sized to them stays small. The holders are the artifact channel's store and every band.
 */

export type ArtifactEntry = {
  tesseraId: bigint;
  layer: string;
  /**
   * The ordinals of the parents this table holds, in the wire's order (ascending by `tesseraId`),
   * and empty where none resolved. A parent is on the wire only where both ends are in one
   * response, so a walk that meets an empty list resolves to none rather than a guessed ancestor. A
   * tree holds at most one parent; a `dag` layer may hold several, and the walk takes the first.
   */
  parentOrdinals: readonly number[];
  /**
   * The rung the wire gave: the declared level on a levelled layer, the parent-chain depth in the
   * response on a treed one, `0` on a flat one. The number a client draws and coarsens by.
   */
  rung: number;
  /**
   * The centroid the naming frame declared, in wire grid units, or null. A positional colour
   * depends only on it (`palette.ts`), so an entry can be coloured whether or not the current view
   * served it.
   */
  centroid: readonly [number, number] | null;
};

/**
 * What a holder hands over: the artifact's id, layer, parents in the same response, centroid and
 * rung. An omitted `rung` means 0, the rung of a flat layer's artifacts and a tree's roots.
 */
export type ArtifactRef = {tesseraId: bigint; layer: string; parentIds: readonly bigint[]; centroid?: readonly [number, number] | null; rung?: number};

/** Ordinal `0` is reserved for no artifact. */
export const NO_ORDINAL = 0;

/**
 * What changed for one ordinal, as {@link SessionArtifactTable.changesSince} reports it. Each kind
 * asks a reader to recompute something different:
 *
 * - `named`: the ordinal was assigned. It is on no other ordinal's parent chain yet, so only its
 *   own derived value moved; a link that puts it on one is reported as the child's `linked`.
 * - `placed`: a centroid arrived for an ordinal already named, so its colour moved, and every value
 *   resolved through it.
 * - `linked`: a parent link was set on an entry that existed before the batch, so its descendants
 *   resolve differently.
 * - `freed`: the last reference went and the slot returned to the free list.
 */
export type ArtifactTableChange = {ordinal: number; kind: 'named' | 'placed' | 'linked' | 'freed'};

const KINDS = ['named', 'placed', 'linked', 'freed'] as const;

/**
 * How many changes the journal keeps. A reader further behind is told so and rebuilds whole, so
 * the limit bounds memory and not correctness.
 */
const JOURNAL_LIMIT = 1 << 16;

function keyOf(layer: string, tesseraId: bigint): string {
  return `${layer}\u0000${tesseraId}`;
}

export class SessionArtifactTable {
  /** Indexed by ordinal; slot 0 is reserved. A freed slot holds null until reused. */
  private entries: (ArtifactEntry | null)[] = [null];
  private refs: number[] = [0];
  private ordinals = new Map<string, number>();
  private free: number[] = [];
  /**
   * Incremented whenever an ordinal is named or freed, a centroid arrives, or a link is set on an
   * entry that already existed. Callers derive their colours under it.
   */
  private stamp = 0;
  /**
   * One packed change per version, oldest first. The version of `journal[i]` is
   * `journalFrom + i + 1`, since each change increments the stamp by one, so
   * {@link changesSince} answers by index.
   */
  private journal: number[] = [];
  /** The version `journal[0]` follows: the oldest version {@link changesSince} can answer from. */
  private journalFrom = 0;

  /** How many times the table's contents have changed; see {@link stamp}. */
  get version(): number {
    return this.stamp;
  }

  /** Records one change against a new version. */
  private record(ordinal: number, kind: ArtifactTableChange['kind']): void {
    this.stamp += 1;
    this.journal.push(ordinal * KINDS.length + KINDS.indexOf(kind));
    if (this.journal.length > JOURNAL_LIMIT) {
      const dropped = this.journal.length - JOURNAL_LIMIT / 2;
      this.journal.splice(0, dropped);
      this.journalFrom += dropped;
    }
  }

  /**
   * Every change since `version`, oldest first, or `null` where the journal cannot answer: a reader
   * more than {@link JOURNAL_LIMIT} changes behind, or one from before a clear.
   *
   * A colour map and a lookup texture are derived from the whole table, which can hold hundreds of
   * thousands of entries while a settled view names a few hundred new ones. Applying the changes
   * costs what changed; rebuilding costs the whole table.
   */
  changesSince(version: number): ArtifactTableChange[] | null {
    if (version === this.stamp) return [];
    if (version < this.journalFrom || version > this.stamp) return null;
    const out: ArtifactTableChange[] = [];
    for (let i = version - this.journalFrom; i < this.journal.length; i++) {
      const packed = this.journal[i]!;
      out.push({ordinal: Math.floor(packed / KINDS.length), kind: KINDS[packed % KINDS.length]!});
    }
    return out;
  }

  /** Ordinals in use. */
  get live(): number {
    return this.ordinals.size;
  }

  /** One past the highest ordinal ever assigned: the range a texture must be sized to. */
  get range(): number {
    return this.entries.length;
  }

  ordinalOf(layer: string, tesseraId: bigint): number {
    return this.ordinals.get(keyOf(layer, tesseraId)) ?? NO_ORDINAL;
  }

  entry(ordinal: number): ArtifactEntry | null {
    return this.entries[ordinal] ?? null;
  }

  /** Every ordinal in use, with its entry, for building a colour per ordinal. */
  liveEntries(): {ordinal: number; entry: ArtifactEntry}[] {
    const out: {ordinal: number; entry: ArtifactEntry}[] = [];
    for (const ordinal of this.ordinals.values()) {
      const entry = this.entries[ordinal];
      if (entry) out.push({ordinal, entry});
    }
    return out;
  }

  /**
   * Takes one reference on each of `refs`, naming any that are new, and returns their ordinals in
   * order. A parent link is set for every parent the table holds, in the reference's order. Links
   * already known survive a batch that carries the child alone or resolves only some of its
   * parents.
   */
  take(refs: readonly ArtifactRef[]): Uint32Array {
    const ordinals = new Uint32Array(refs.length);
    // A link set on an ordinal this batch named is part of naming it, and is not reported as `linked`.
    const namedHere = new Set<number>();
    for (let i = 0; i < refs.length; i++) {
      const ref = refs[i]!;
      const key = keyOf(ref.layer, ref.tesseraId);
      let ordinal = this.ordinals.get(key);
      if (ordinal === undefined) {
        ordinal = this.free.pop() ?? this.entries.length;
        this.entries[ordinal] = {tesseraId: ref.tesseraId, layer: ref.layer, parentOrdinals: [], rung: ref.rung ?? 0, centroid: ref.centroid ?? null};
        this.refs[ordinal] = 0;
        this.ordinals.set(key, ordinal);
        namedHere.add(ordinal);
        this.record(ordinal, 'named');
      } else if (ref.centroid && !this.entries[ordinal]!.centroid) {
        // A centroid arriving late is a colour arriving late, on the same identity.
        this.entries[ordinal] = {...this.entries[ordinal]!, centroid: ref.centroid};
        this.record(ordinal, 'placed');
      }
      this.refs[ordinal]! += 1;
      ordinals[i] = ordinal;
    }
    // Links second, once every end of this batch has an ordinal.
    for (let i = 0; i < refs.length; i++) {
      const ref = refs[i]!;
      if (ref.parentIds.length === 0) continue;
      const parents: number[] = [];
      for (const parentId of ref.parentIds) {
        const parent = this.ordinals.get(keyOf(ref.layer, parentId));
        if (parent !== undefined) parents.push(parent);
      }
      if (parents.length === 0) continue;
      const entry = this.entries[ordinals[i]!]!;
      // A partial resolution does not shrink a list already held. The point path's batch comes from
      // the membership column and lands before the channel's, so it often lacks some parents.
      if (parents.length < ref.parentIds.length && entry.parentOrdinals.length > 0) continue;
      if (parents.length === entry.parentOrdinals.length && parents.every((p, j) => p === entry.parentOrdinals[j])) continue;
      const moved = parents[0] !== entry.parentOrdinals[0];
      this.entries[ordinals[i]!] = {...entry, parentOrdinals: parents};
      // A link on an entry that was already here changes what its descendants resolve to. It is
      // reported only when the first parent changed, since {@link resolve} walks the first alone; a
      // second parent arriving behind the first changes no colour.
      if (moved && !namedHere.has(ordinals[i]!)) this.record(ordinals[i]!, 'linked');
    }
    return ordinals;
  }

  /**
   * Takes one more reference on each of `ordinals`, as a band does for the distinct ordinals it
   * carries. Ordinal `0` and a freed slot are skipped.
   */
  retain(ordinals: ArrayLike<number>): void {
    for (let i = 0; i < ordinals.length; i++) {
      const ordinal = ordinals[i]!;
      if (ordinal === NO_ORDINAL || !this.entries[ordinal]) continue;
      this.refs[ordinal]! += 1;
    }
  }

  /** Releases one reference per ordinal; an ordinal at zero returns to the free list. */
  release(ordinals: ArrayLike<number>): void {
    for (let i = 0; i < ordinals.length; i++) {
      const ordinal = ordinals[i]!;
      if (ordinal === NO_ORDINAL) continue;
      const entry = this.entries[ordinal];
      if (!entry) continue;
      const left = (this.refs[ordinal] ?? 0) - 1;
      if (left > 0) {
        this.refs[ordinal] = left;
        continue;
      }
      this.refs[ordinal] = 0;
      this.entries[ordinal] = null;
      this.ordinals.delete(keyOf(entry.layer, entry.tesseraId));
      this.free.push(ordinal);
      this.record(ordinal, 'freed');
    }
  }

  /**
   * Walks `ordinal`'s parent links to the nearest ordinal `served` admits, or `0` where the walk
   * fails: an edge never seen, or a cut finer than anything held. `served` is anything with `has`,
   * such as a set of ordinals or the colour map.
   *
   * `maxLevel` is the level to colour at: the walk passes a served ancestor whose rung is deeper and
   * stops at the first at or above it, so a coarser level is a lookup-texture rewrite, not a
   * refetch. Undefined colours at the deepest served. On a `dag` layer the walk takes the first
   * parent at each step; parents are ordered by `tessera_id`, so the chain is the same on every
   * rebuild, and every artifact on it holds the point.
   */
  resolve(ordinal: number, served: {has(ordinal: number): boolean}, maxLevel?: number): number {
    let at = ordinal;
    // Bounded by the table's size, so a link cycle cannot hang the caller.
    for (let steps = 0; at !== NO_ORDINAL && steps <= this.entries.length; steps++) {
      const here = this.entries[at];
      if (served.has(at) && (maxLevel === undefined || (here?.rung ?? 0) <= maxLevel)) return at;
      at = this.entries[at]?.parentOrdinals[0] ?? NO_ORDINAL;
    }
    return NO_ORDINAL;
  }

  /** Drops everything, because the identity key changed. */
  clear(): void {
    this.entries = [null];
    this.refs = [0];
    this.ordinals.clear();
    this.free = [];
    this.stamp += 1;
    // The journal restarts at the new version, so no reader patches across a clear.
    this.journal = [];
    this.journalFrom = this.stamp;
  }
}
