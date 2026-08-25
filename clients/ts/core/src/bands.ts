import {MAX_DEPTH, WORLD_SIZE, tileContains, tileXY} from './coords.js';
import {NO_ORDINAL, type ArtifactRef, type SessionArtifactTable} from './artifactTable.js';
import type {ScalarColumn, ViewportResult} from './types.js';
import {
  coverageAdd,
  coverageAt,
  rectArea,
  rectContainsTile,
  rectIntersection,
  rectSubtractAll,
  type Coverage,
  type TileRect
} from './rects.js';

/**
 * The replica: what a client holds, as per-tile bands of points.
 *
 * **The unit is the (tile, cut) band, not the point** (`caching.md` §6). Design §7.2 serves a
 * prefix of each tile's visible set in `tessera_id` order, and those prefixes nest across depth, so
 * a point served at depth 3 is served again by every deeper fetch covering it. Storing points
 * independently would carry per-point metadata over 10^7 points and fight the nesting the sampler
 * already paid for; storing prefixes means one bound per tile describes everything held.
 *
 * That bound is what a client declares (`delta-serving.md` §3): *I hold every member of this tile's
 * visible set with `tessera_id` below X*. It is an identity bound rather than a count because the
 * same value is meaningful at every depth — one declaration against a parent answers for all four
 * children — and because a band assembled from several responses still describes itself exactly.
 *
 * Nothing here fetches. This is the replica store `client-interaction.md` §10 places above the
 * stateless client, and it is in `core` because its rules are invariant-bearing: the eviction
 * order, the prefix-only truncation, and the identity-key partition are each load-bearing and each
 * testable without a browser.
 */

/** A tile's address: its Morton prefix at a depth. Depth is not recoverable from the prefix. */
export type TileAddress = {depth: number; prefix: bigint};

/** `${depth}:${prefix}` — the map key. Depth leads so a scan can stop at a depth boundary. */
export type BandKey = string;

export function bandKey(depth: number, prefix: bigint): BandKey {
  return `${depth}:${prefix}`;
}

/**
 * One tile's held points, ascending by `tessera_id` — the wire's own order (contracts §3.2), kept
 * because every operation here is a prefix operation over it.
 *
 * **The arrays are copies, never views onto a response buffer.** A `subarray` keeps the whole
 * response alive, so evicting a band would free nothing and `bytes` would be fiction — and the
 * byte ledger is the only thing standing between a 30-minute session and 2–6 GB of accumulation
 * (`caching.md` §6).
 */
export type Band = {
  depth: number;
  prefix: bigint;
  /**
   * The tile's `(x, y)` index, de-interleaved once when the band is built.
   *
   * **Every region query needs it and none of them should pay for it.** Recovering it from the
   * prefix is a per-bit `BigInt` loop, and `bandsForRegion` runs over every held band on every
   * redraw — so at 2.4 × 10^4 bands that was ~2.4 × 10^5 `BigInt` allocations per frame, measured
   * at 14–20 ms of redraw for a view drawing as few as 5,700 marks. It scaled with what the cache
   * *held* rather than with what was drawn, which is why a small mark budget did not help.
   */
  x: number;
  y: number;
  ids: BigUint64Array;
  /**
   * Interleaved x,y in **deck.gl world space**, `f32` — two entries per point.
   *
   * Converted once when the band is built rather than on every assembly. The wire carries `f64`
   * cell space and the renderer wants `f32` world space; doing that per point per redraw is the
   * single most expensive thing in the render path, and it produces the same numbers every time.
   * Storing the converted form also halves the bytes: 8 per point rather than 16.
   */
  positions: Float32Array;
  scalars: Record<string, ScalarColumn>;
  /**
   * The session ordinal per point, per layer the response was asked for (design §5.10): `0` for
   * a point under no served artifact of that layer. Named on the main thread as the band was
   * built, from the decoder's response-local index. A layer absent here was not named when the
   * band was fetched, which is what makes the band colour-stale for it; a layer turned off keeps
   * its column until eviction, so turning it back on is free.
   *
   * `distinct` is the band's reference on the table — one per ordinal it carries — released when
   * the band is evicted or truncated, and the list colour coverage is checked over (a dozen
   * entries, never the points).
   */
  membership: Record<string, BandMembership>;
  /** `m(T)` as the server reported it: how many points the definition serves for this tile. */
  served: number;
  /** `min(k, k_max_marks)` in force when this band was fetched — see {@link isComplete}. */
  capUsed: number;
  visible: bigint;
  matched: bigint;
  /** The declaration: every held identity is strictly below this. `0n` for an empty band. */
  heldBelow: bigint;
  identityKey: string;
  contentKey: string;
  bytes: number;
  touchedAt: number;
};

export type BandMembership = {ordinals: Uint32Array; distinct: Uint32Array};

/** The distinct non-zero ordinals of a slice, ascending — a band's reference on the table. */
export function distinctOrdinals(ordinals: Uint32Array): Uint32Array {
  const seen = new Set<number>();
  for (let i = 0; i < ordinals.length; i++) {
    const o = ordinals[i]!;
    if (o !== NO_ORDINAL) seen.add(o);
  }
  return Uint32Array.from(seen).sort();
}

/**
 * Does this band hold the whole of `served(T)`, and will it still at `k`?
 *
 * θ is viewport-invariant — design §7.2's threshold depends on the mask, the generation and the
 * view, never on the bounding box or the zoom — so at a fixed content key and fixed depth `m(T)`
 * does not move, and the `served` the server already reported is still current. Two clauses:
 *
 * 1. the band holds every point the definition serves; and
 * 2. `served` is below the cap that was in force, so the **cap was not the binding clause** and a
 *    larger `k` cannot grow `m(T)`. Where `served` equals that cap the cap *was* binding, and a
 *    larger `k` yields more — so the tile must be requested again.
 *
 * A tile passing both need not appear in a request at all, which is the only mechanism that makes
 * server work scale with novelty rather than with viewport area (`delta-serving.md` §1).
 */
export function isComplete(band: Band, contentKey: string, k: number): boolean {
  if (band.contentKey !== contentKey) return false;
  if (band.ids.length !== band.served) return false;
  return band.served < band.capUsed || band.capUsed >= k;
}

/** How many bytes a band's buffers occupy, for the ledger eviction runs against. */
function bandBytes(
  ids: BigUint64Array,
  positions: Float32Array,
  scalars: Record<string, ScalarColumn>,
  membership: Record<string, BandMembership>
): number {
  let bytes = ids.byteLength + positions.byteLength;
  for (const column of Object.values(scalars)) {
    bytes += scalarBytes(column);
  }
  // The ordinal column is 4 B a point per layer on (§5.10's table); the ledger counts it.
  for (const m of Object.values(membership)) bytes += m.ordinals.byteLength + m.distinct.byteLength;
  return bytes;
}

function scalarBytes(column: ScalarColumn): number {
  // `bool` and `utf8` decode to boxed arrays rather than typed ones. Their true cost is a heap
  // object per value; the estimates here are deliberately generous rather than accurate, because
  // undercounting them is what would let the ledger drift above the bound it exists to hold.
  if (column.arrowType === 'bool') return column.values.length * 4;
  if (column.arrowType === 'utf8') {
    let bytes = 0;
    for (const value of column.values) bytes += 40 + value.length * 2;
    return bytes;
  }
  return column.values.byteLength;
}

function sliceScalars(
  scalars: Record<string, ScalarColumn>,
  from: number,
  to: number
): Record<string, ScalarColumn> {
  const out: Record<string, ScalarColumn> = {};
  for (const [name, column] of Object.entries(scalars)) {
    out[name] = {arrowType: column.arrowType, values: column.values.slice(from, to)} as ScalarColumn;
  }
  return out;
}

/**
 * Split a response into bands, one per tile it reports.
 *
 * The wire orders points by tile in the order the tile stream lists them, and `served` gives each
 * tile's length — so this is a prefix-sum walk, and `served` is the *only* way to recover the
 * grouping without recomputing the selection (contracts §3.2).
 *
 * Tiles reporting no served points yield no band: an empty band would declare a bound of zero,
 * which is what a client with nothing declares anyway.
 */
/**
 * A resumable split, so a large response need not block the thread that draws.
 *
 * Splitting is the last big block of main-thread work per response — measured at 18.5 ms mean and
 * 49 ms max per response, landing in the same frame as deriving and uploading, which is the p95
 * hitch. The work itself cannot move (bands must be copies, and 10^4 of them will not transfer to a
 * worker cheaply), but it slices: each {@link BandSplitter.step} builds bands until its deadline
 * and returns, and the caller yields to the frame loop between steps.
 */
export type BandSplitter = {
  done(): boolean;
  /** Build bands until `performance.now()` passes `deadline`. Never returns an empty array early. */
  step(deadline: number): Band[];
};

/**
 * The main thread's half of naming (design §5.10): the decoder's distinct-id list maps to
 * session ordinals through the table — a few thousand lookups, once per response — and each
 * band's points are remapped from local index to ordinal with a tight loop as the band is
 * built. Parent links come from the same response's artifacts frame, which is the only place a
 * `parentId` is ever named (decision 0087).
 *
 * The response holds one temporary reference per distinct ordinal while its bands are being
 * built, so an ordinal named by the distinct list cannot be recycled between two slices; each
 * band then takes its own references, and the temporary ones go when the split completes.
 */
type ResponseNaming = {
  layer: string;
  index: Uint16Array | Uint32Array;
  /** Local index → session ordinal; `map[0] = 0`. */
  map: Uint32Array;
  /** Scratch over local indices, for collecting a band's distinct set without a `Set` per band. */
  mark: Uint8Array;
};

function nameResponse(result: ViewportResult, table: SessionArtifactTable): {naming: ResponseNaming[]; release: () => void} {
  const parentOf = new Map<string, bigint | null>();
  for (const a of result.artifacts) parentOf.set(`${a.layer} ${a.tesseraId}`, a.parentId);
  const naming: ResponseNaming[] = [];
  const held: Uint32Array[] = [];
  for (const [layer, column] of Object.entries(result.membership)) {
    const refs: ArtifactRef[] = [];
    for (let d = 0; d < column.ids.length; d++) {
      const id = column.ids[d]!;
      refs.push({tesseraId: id, layer, parentId: parentOf.get(`${layer} ${id}`) ?? null});
    }
    const ordinals = table.take(refs);
    held.push(ordinals);
    const map = new Uint32Array(column.ids.length + 1);
    map.set(ordinals, 1);
    naming.push({layer, index: column.index, map, mark: new Uint8Array(column.ids.length + 1)});
  }
  return {
    naming,
    release: () => {
      for (const ordinals of held) table.release(ordinals);
    }
  };
}

/** One band's membership for one layer: the remap loop, and its distinct list, retained. */
function remapBand(n: ResponseNaming, from: number, to: number, table: SessionArtifactTable): BandMembership {
  const ordinals = new Uint32Array(to - from);
  const {index, map, mark} = n;
  let distinctCount = 0;
  for (let i = from; i < to; i++) {
    const local = index[i]!;
    ordinals[i - from] = map[local]!;
    if (local !== 0 && mark[local] === 0) {
      mark[local] = 1;
      distinctCount++;
    }
  }
  const distinct = new Uint32Array(distinctCount);
  let d = 0;
  for (let i = from; i < to; i++) {
    const local = index[i]!;
    if (mark[local] === 1) {
      mark[local] = 0;
      distinct[d++] = map[local]!;
    }
  }
  distinct.sort();
  table.retain(distinct);
  return {ordinals, distinct};
}

export function bandSplitter(
  result: ViewportResult,
  depth: number,
  meta: {identityKey: string; contentKey: string; capUsed: number; now: number; table?: SessionArtifactTable; onRemap?: (ms: number) => void}
): BandSplitter {
  let offset = 0;
  let i = 0;
  const table = meta.table;
  const named = table && Object.keys(result.membership).length > 0 ? nameResponse(result, table) : null;
  let remapMs = 0;
  return {
    done: () => i >= result.tiles.length,
    step(deadline: number): Band[] {
      const bands: Band[] = [];
      // The clock every 64 tiles, not every tile: a `performance.now()` per band would be a
      // meaningful share of the work being sliced.
      while (i < result.tiles.length) {
        if ((i & 63) === 0 && bands.length > 0 && performance.now() >= deadline) break;
        const tile = result.tiles[i++]!;
        const served = Number(tile.served);
        if (served === 0) continue;
        const end = offset + served;
        const ids = result.ids.slice(offset, end);
        // **Ascending order asserted at the one gate every band passes.** Every prefix operation
        // in the client — eviction truncation, density-matched subsets, the declaration bound —
        // rests on the wire's ascending-identity contract (contracts §3.2); until here it was an
        // unchecked premise (review, Question E). O(n) over bytes already being copied.
        for (let p = 1; p < ids.length; p++) {
          if (ids[p]! <= ids[p - 1]!) {
            throw new Error(
              `band ${tile.tile}: ids out of ascending order at ${p} — every client subset rule ` +
                `rests on this, so a violation must refuse loudly rather than serve quietly.`
            );
          }
        }
        // Already in world space — the decoder produced it, which in a browser means a worker did.
        const positions = result.world.slice(offset * 2, end * 2);
        const scalars = sliceScalars(result.scalars, offset, end);
        const membership: Record<string, BandMembership> = {};
        if (named) {
          const started = performance.now();
          for (const n of named.naming) membership[n.layer] = remapBand(n, offset, end, table!);
          remapMs += performance.now() - started;
        }
        const {x, y} = tileXY(tile.tile, depth);
        bands.push({
          depth,
          prefix: tile.tile,
          x,
          y,
          ids,
          positions,
          scalars,
          membership,
          served,
          capUsed: meta.capUsed,
          visible: tile.visible,
          matched: tile.matched,
          heldBelow: ids.length === 0 ? 0n : ids[ids.length - 1]! + 1n,
          identityKey: meta.identityKey,
          contentKey: meta.contentKey,
          bytes: bandBytes(ids, positions, scalars, membership),
          touchedAt: meta.now
        });
        offset = end;
      }
      if (i >= result.tiles.length && named) {
        named.release();
        meta.onRemap?.(remapMs);
      }
      return bands;
    }
  };
}

/** {@link bandSplitter}, drained in one call — the form every synchronous caller wants. */
export function bandsOfResult(
  result: ViewportResult,
  depth: number,
  meta: {identityKey: string; contentKey: string; capUsed: number; now: number}
): Band[] {
  return bandSplitter(result, depth, meta).step(Infinity);
}

/** What a band contributes to a render, and on what authority. */
export type Provenance = 'exact' | 'ancestor' | 'descendants';

export type Resolved = {
  provenance: Provenance;
  bands: Band[];
  /**
   * False where the points came from anywhere but this tile at this depth, in which case the drawn
   * marks are a superset of `served(T)`. Presentation only: no number-channel value may be shown
   * against such a tile, and the drawn-count assertion does not range over it
   * (`delta-serving.md` §7).
   */
  exact: boolean;
};

export type PlannedRequest = {
  /** The novel regions, in tile-index space at the planned depth. Empty means nothing to ask for. */
  fetch: TileRect[];
  /** Tiles the wanted region spans, and how many of them the request covers — for reporting only. */
  wanted: number;
  novel: number;
};

export type EvictionFocus = {depth: number; prefix: bigint};

/**
 * The held bands for one principal, under one byte budget.
 *
 * Partitioned by identity key: a change of principal drops the whole partition rather than
 * filtering it, so cross-principal reuse is impossible by construction rather than by discipline
 * (`client-interaction.md` §10). A cache keyed too loosely here serves one principal's authorised
 * data to another — a disclosure, not a staleness bug (decision 0029).
 */
export class BandCache {
  private bands = new Map<BandKey, Band>();
  /**
   * The same bands, grouped by depth.
   *
   * **A frame needs one depth's bands and a walk over every band was finding them.** Deriving a
   * frame scanned the whole store — measured at 2.3 x 10^5 held bands, 0.09 us each rising to
   * 0.43 us as the store filled, so 7 ms early in a session and 98–148 ms late in one, on every
   * derive. It scaled with what is *held*, which a mark budget cannot bound and which panning only
   * makes worse: the second time this exact shape of fault has been measured in this file.
   *
   * Stand-ins still need the other depths, so this does not remove the walk — it removes the
   * majority of it, because the working depth holds the bulk of the store, and it makes a frame
   * whose region is fully held cost one depth's bands rather than all of them.
   */
  private byDepth = new Map<number, Map<BandKey, Band>>();
  /**
   * Bumped by every change to what is held or covered.
   *
   * **A frame derived from this cache stays valid exactly as long as this does not move**, which is
   * what lets a redraw skip re-deriving one. Deriving a frame is per-band work — restricting stand-in
   * bands, concatenating them, folding their columns — and at 3.9 × 10^4 stand-in bands it measured
   * ~35 ms, on every animation frame of a drag, for a result that could not have changed. A counter
   * is the whole of the fix: it is exact rather than heuristic, and a missed bump would draw a
   * stand-in over ground that has since been covered, which is a fault this client has had once
   * already.
   */
  private changes = 0;
  /**
   * The regions this client has asked for and absorbed the answer to.
   *
   * **This is how emptiness is cached, and caching emptiness is what makes any of the rest work.**
   * A response omits a tile whose visible count is zero, so without a record of having asked, every
   * empty tile is re-requested on every view — and a viewport is overwhelmingly empty tiles:
   * measured on the 2.4M demo corpus, 454 of 16,524 tiles in a settled view carry any data at all.
   * The other 16,070 would put a request on the wire forever and no revisit would ever be free.
   *
   * **Rectangles rather than one entry per tile**, which an earlier version of this did. That
   * version was unbounded — one map entry per empty tile ever looked at, ~16k per viewport, never
   * evicted and never counted against `budgetBytes` — and it forced planning to enumerate every
   * tile in the viewport to consult it. A covered rectangle asserts the same thing over its whole
   * area in four integers: *we asked here and absorbed the answer, so anything we were not sent is
   * empty*.
   */
  private covered: Coverage[] = [];
  private identityKey: string | null = null;
  private held = 0;
  private heldPoints = 0;

  constructor(
    private readonly budgetBytes: number,
    /** The session table each band's membership holds references on; absent, nothing is named. */
    private readonly table: SessionArtifactTable | null = null
  ) {}

  /** Give back every reference a band's membership holds. */
  private releaseMembership(band: Band): void {
    if (!this.table) return;
    for (const m of Object.values(band.membership)) this.table.release(m.distinct);
  }

  get bytes(): number {
    return this.held;
  }

  /**
   * Points held, which is the figure to size a replica against — bytes hide how much of the budget
   * is per-band overhead rather than payload, and bands here are small (`m_target` is single
   * digits, so a band is ~10 points) so that overhead is not a rounding error.
   *
   * A maintained counter, not a walk: this is read on every store update, and a walk scaled with
   * the 10^5 bands a long session holds rather than with the update that asked.
   */
  get points(): number {
    return this.heldPoints;
  }

  get bandCount(): number {
    return this.bands.size;
  }

  /** Keep {@link byDepth} in step with a `bands` write. The only place either is inserted into. */
  private index(band: Band, key: BandKey): void {
    let atDepth = this.byDepth.get(band.depth);
    if (!atDepth) {
      atDepth = new Map();
      this.byDepth.set(band.depth, atDepth);
    }
    atDepth.set(key, band);
  }

  /** Held bands at one depth, or nothing — never the whole store. */
  private atDepth(depth: number): Iterable<Band> {
    return this.byDepth.get(depth)?.values() ?? [];
  }

  /**
   * The exact bands inside a region — the fast half of {@link bandsForRegion} on its own.
   *
   * For the caller that already has a drawn frame and needs only to fold a fresh arrival into it:
   * the stand-in walk is the expensive half, and a frame whose stand-ins are one arrival stale is
   * drawable while the full derivation waits for a quiet moment.
   */
  exactIn(want: TileRect, depth: number): Band[] {
    const exact: Band[] = [];
    for (const band of this.atDepth(depth)) {
      if (rectContainsTile(want, band.x, band.y)) exact.push(band);
    }
    return exact;
  }

  /** See {@link changes}. Opaque and monotonic — compare for equality, never for order. */
  get version(): number {
    return this.changes;
  }

  get size(): number {
    return this.bands.size;
  }

  get(depth: number, prefix: bigint): Band | undefined {
    return this.bands.get(bandKey(depth, prefix));
  }

  /**
   * Admit a band, dropping every other principal's first.
   *
   * A band whose content key differs from the held one **replaces** it rather than merging into it.
   * Unioning would let an item suppressed since the held band was fetched survive into a band the
   * client now marks fresh — a client-side fail-open, and against the property that the server
   * names the complete served set, so an item it does not name is one the client drops
   * (`delta-serving.md` §7).
   */
  put(band: Band): void {
    if (this.identityKey !== band.identityKey) {
      this.dropIdentity();
      this.identityKey = band.identityKey;
    }
    const key = bandKey(band.depth, band.prefix);
    const previous = this.bands.get(key);
    if (previous) {
      // **A layer's column survives a refetch that did not name the layer.** A band refetched
      // for another layer's column carries the same served set (same content key and length),
      // so the columns it lacks are carried over from the band it replaces with their references
      // — which is what makes switching a layer back on free (§5.10). A replacement under a
      // moved content key carries nothing over: the served set may differ.
      const sameSet = previous.contentKey === band.contentKey && previous.ids.length === band.ids.length;
      for (const [layer, held] of Object.entries(previous.membership)) {
        if (sameSet && !(layer in band.membership)) {
          band.membership[layer] = held;
          band.bytes += held.ordinals.byteLength + held.distinct.byteLength;
        } else if (this.table) {
          this.table.release(held.distinct);
        }
      }
      this.held -= previous.bytes;
      this.heldPoints -= previous.ids.length;
    }
    this.bands.set(key, band);
    this.index(band, key);
    this.held += band.bytes;
    this.heldPoints += band.ids.length;
    this.changes++;
  }

  /**
   * Record that a region was asked for and its answer absorbed.
   *
   * Called only *after* the response's bands are in, never before: a region marked covered before
   * its points are held would let the next plan omit tiles whose data never arrived.
   */
  markCovered(rect: TileRect, depth: number, contentKey: string, capUsed: number): void {
    this.covered = coverageAdd(this.covered, {rect, depth, contentKey, capUsed});
    this.changes++;
  }

  /** Regions held at this depth, content key and cap — the holes a plan subtracts. */
  coverageFor(depth: number, contentKey: string, k: number): TileRect[] {
    return coverageAt(this.covered, depth, contentKey, k);
  }

  /**
   * Withdraw the coverage claim over each of `bands`' tiles, so the next plan fetches them
   * again — the colour-stale refetch (§5.10): a band whose ordinals no longer resolve to
   * anything served, or that lacks the column for a layer now on, is asked for again after novel
   * ground, centre-first, by the same path a stale-content band takes. The band stays held and
   * drawn meanwhile; the arrival replaces it.
   */
  retract(bands: readonly Band[]): void {
    for (const band of bands) {
      if (this.bands.get(bandKey(band.depth, band.prefix)) === band) this.retractCoverage(band.depth, band.x, band.y);
    }
  }

  /** Drop everything. Called on a token change, where the whole partition becomes unrenderable. */
  dropIdentity(): void {
    for (const band of this.bands.values()) this.releaseMembership(band);
    this.bands.clear();
    this.byDepth.clear();
    this.covered = [];
    this.changes++;
    this.identityKey = null;
    this.held = 0;
    this.heldPoints = 0;
  }

  /**
   * Decide, for a tile set at one depth, what need not be asked for and what must be.
   *
   * Declarations are computed **from the cache**, never from a record of what the server named:
   * eviction is normal operation, and a client that declared a band it had since evicted would get
   * a silent hole (`caching.md` §7.1). Understating is safe in the other direction — it costs
   * bytes, never correctness.
   */
  planRegion(want: TileRect, depth: number, contentKey: string, k: number): PlannedRequest {
    const wanted = rectArea(want);

    // A counts-only request (`k = 0`) subtracts nothing. Its whole purpose is to refresh the number
    // channel and the content key over ground the client already holds — which is what keeps the
    // staleness bound reachable once look-ahead has emptied the request (`delta-serving.md` §8).
    if (k === 0) return {fetch: [want], wanted, novel: wanted};

    // **At most two pieces, because each piece is a request.** The per-request floor is ~170 µs and
    // a tile the client already holds costs the server essentially nothing to be asked for again —
    // so past two, one slightly-too-large request beats four exact ones. Measured the wrong way
    // round first: unbounded subtraction turned a single pan into six requests.
    const fetch = rectSubtractAll(want, this.coverageFor(depth, contentKey, k), 2);
    return {fetch, wanted, novel: fetch.reduce((n, r) => n + rectArea(r), 0)};
  }

  /**
   * The bands to draw for a region, and on what authority.
   *
   * **Iterates what is held, not what is wanted**, which is the whole reason this is affordable: a
   * settled viewport spans ~16.5k tiles of which ~450 carry any data, so walking the held bands is
   * two orders of magnitude cheaper than walking the viewport and asking about each tile.
   *
   * Exact bands come from the region the client has covered at this depth. Fallbacks — an ancestor
   * band restricted by prefix, or held descendants — are admitted **only over the part of the
   * region that is not covered**, which is what keeps them from double-drawing ground an exact band
   * already answers. Both are supersets of `served(T)` and are presentation, never selection
   * (`caching.md` §6, I7): the caller stale-marks them and shows no count against them.
   */
  bandsForRegion(
    want: TileRect,
    depth: number,
    contentKey: string,
    k: number
  ): {exact: Band[]; fallback: {band: Band; clip: TileRect}[]} {
    const uncovered = rectSubtractAll(want, this.coverageFor(depth, contentKey, k));
    // The requested depth, by index rather than by scan.
    const exact = this.exactIn(want, depth);
    /** Candidate stand-ins, bucketed by how far their depth is from the one being drawn. */
    const byRank: {band: Band; clip: TileRect}[][] = [];

    // **Nothing else is needed when the region is wholly held**, which is the settled case and now
    // costs one depth's bands rather than every band in the store.
    if (uncovered.length === 0) return {exact, fallback: []};

    for (const band of this.bands.values()) {
      if (band.depth === depth) continue;
      // Project the band's tile onto this depth's grid and admit it only where the view is not
      // already answered. An ancestor covers a block; a descendant collapses to a single tile.
      const shift = Math.abs(band.depth - depth);
      const {x, y} = band;
      const box: TileRect =
        band.depth < depth
          ? {x0: x << shift, y0: y << shift, x1: ((x + 1) << shift) - 1, y1: ((y + 1) << shift) - 1}
          : {x0: x >> shift, y0: y >> shift, x1: x >> shift, y1: y >> shift};
      // **Clipped to the uncovered part, not to the whole region.** A stand-in exists to fill
      // ground that has no exact band; drawn across the rest it overlays coarse marks on fine ones,
      // and a frame that is mostly stand-in reads as a lower-density patch that never refines —
      // because as far as the plan is concerned that ground is answered, and it is.
      // Ordered coarsest-first by depth distance, bucketed rather than sorted because a settled
      // broad view offers up to 1.5 x 10^5 candidates. **Bounding the total was tried and
      // reverted**: capping marks drops whole bands, and a stand-in exists to cover ground, so what
      // the cap produced was bare background in the shape of the coverage subtraction that asked
      // for it — black rectangles that filled in only when real data arrived. Density is not what a
      // stand-in spends marks on.
      const rank = band.depth < depth ? depth - band.depth : MAX_DEPTH + (band.depth - depth);
      for (const r of uncovered) {
        const clip = rectIntersection(r, box);
        if (clip) (byRank[rank] ??= []).push({band, clip});
      }
    }

    // Appended rather than spread: `push(...bucket)` passes one argument per entry, and a settled
    // broad view offers upwards of 10^5 of them — which is a `RangeError`, not a slow path.
    const fallback: {band: Band; clip: TileRect}[] = [];
    for (const bucket of byRank) {
      if (!bucket) continue;
      for (const entry of bucket) fallback.push(entry);
    }
    return {exact, fallback};
  }

  /**
   * The best available answer for a tile: its own band, else the nearest ancestor holding one, else
   * whatever descendants are held.
   *
   * Both fallbacks draw a superset of `served(T)` and are presentation, never selection
   * (`caching.md` §6, I7). The caller must stale-mark them and show no count against them.
   */
  resolve(depth: number, prefix: bigint): Resolved | null {
    const exact = this.get(depth, prefix);
    if (exact) return {provenance: 'exact', bands: [exact], exact: true};

    for (let d = depth - 1; d >= 0; d--) {
      const ancestor = this.get(d, prefix >> BigInt(2 * (depth - d)));
      if (ancestor) return {provenance: 'ancestor', bands: [ancestor], exact: false};
    }

    const descendants: Band[] = [];
    for (const [held, atDepth] of this.byDepth) {
      if (held <= depth) continue;
      for (const band of atDepth.values()) {
        if (tileContains(prefix, depth, band.prefix, band.depth)) descendants.push(band);
      }
    }
    if (descendants.length > 0) return {provenance: 'descendants', bands: descendants, exact: false};
    return null;
  }

  /**
   * The same, to a *region* rather than a single tile — what a zoom-in fallback actually needs.
   *
   * **Tested in world space, not in identity space.** A tile rectangle at a depth is a world-space
   * box, and the band already holds each point's world position, so containment is four `f32`
   * comparisons. Deriving each point's tile instead — a `BigInt` shift and a per-bit `BigInt` loop
   * — is around twenty `BigInt` allocations per point, which at 10^6 points is the difference
   * between a redraw and a two-second freeze. This is the zoom path, so it runs on exactly the
   * interaction least able to afford it.
   */
  static restrictToRect(band: Band, depth: number, rect: TileRect): number[] | null {
    const span = WORLD_SIZE / 2 ** depth;
    const x0 = rect.x0 * span;
    const x1 = (rect.x1 + 1) * span;
    const y0 = rect.y0 * span;
    const y1 = (rect.y1 + 1) * span;

    // **A band wholly inside the rectangle needs no restriction at all**, and saying so is the
    // difference between a memcpy and a per-point loop with an index array behind it. It is also
    // the common case rather than an optimisation for a corner: a descendant band drawn on zoom-out
    // occupies a tile far smaller than the region, and an ancestor drawn on zoom-in is clipped to
    // ground that is uncovered precisely because nothing finer has arrived, so the parent's whole
    // tile usually falls inside it. Measured with 39,121 stand-in bands carrying 203,547 marks
    // between them — five marks each — where the per-band overhead, not the per-mark work, was the
    // whole 29.8 ms of a frame.
    const own = WORLD_SIZE / 2 ** band.depth;
    if (
      band.x * own >= x0 &&
      (band.x + 1) * own <= x1 &&
      band.y * own >= y0 &&
      (band.y + 1) * own <= y1
    ) {
      return null;
    }

    const indices: number[] = [];
    const p = band.positions;
    for (let i = 0; i < band.ids.length; i++) {
      const x = p[i * 2]!;
      const y = p[i * 2 + 1]!;
      if (x >= x0 && x < x1 && y >= y0 && y < y1) indices.push(i);
    }
    return indices;
  }

  /**
   * Evict to the budget by **truncating band tails**, deepest / least-recently-touched /
   * farthest-from-focus first.
   *
   * **Never the head.** Coarse points are the low-identity head of every band, so keeping heads is
   * what keeps overview rendering from blanking — and it delivers "top-level points never get
   * bumped" without any per-point priority bookkeeping (`caching.md` §6). Truncating rather than
   * dropping is why: a band reduced to its head still answers for the zoomed-out view and still
   * declares a sound, lower bound.
   *
   * Runs to a low-water mark rather than to the budget exactly, so a steady stream of `put`s does
   * not re-sort the whole cache on each one.
   */
  evict(focus: EvictionFocus, lowWaterFraction = 0.9): void {
    if (this.held <= this.budgetBytes) return;
    const target = this.budgetBytes * lowWaterFraction;

    const order = [...this.bands.values()].sort((a, b) => {
      if (a.depth !== b.depth) return b.depth - a.depth;
      if (a.touchedAt !== b.touchedAt) return a.touchedAt - b.touchedAt;
      return Number(distance(b, focus) - distance(a, focus));
    });

    for (const band of order) {
      if (this.held <= target) return;
      const keep = Math.max(1, Math.floor(band.ids.length / 2));
      if (keep >= band.ids.length) continue;
      this.truncate(band, keep);
    }
  }

  /** Cut a band to its first `keep` points, lowering its bound to match exactly. */
  /**
   * Withdraw the coverage claim over a tile.
   *
   * **Eviction must retract coverage or it becomes a silent hole.** A covered rectangle asserts
   * *we asked here and hold the answer*; once a band inside it has been truncated that is no longer
   * true, and a plan would go on subtracting the region so the discarded points were never fetched
   * again. The client would draw short for the rest of the session and nothing would say so.
   *
   * The whole containing rectangle goes, not the tile's share of it — a rectangle minus a point is
   * not a rectangle, and the alternative is to start storing the holes. It is self-healing and it
   * costs a refetch of ground the cache had already decided to give up.
   */
  private retractCoverage(depth: number, x: number, y: number): void {
    this.covered = this.covered.filter(
      (c) => c.depth !== depth || !rectContainsTile(c.rect, x, y)
    );
    this.changes++;
  }

  private truncate(band: Band, keep: number): void {
    this.retractCoverage(band.depth, band.x, band.y);
    const ids = band.ids.slice(0, keep);
    const positions = band.positions.slice(0, keep * 2);
    const scalars = sliceScalars(band.scalars, 0, keep);
    // The head's membership, re-referenced: the distinct list may shrink with the tail.
    const membership: Record<string, BandMembership> = {};
    for (const [layer, held] of Object.entries(band.membership)) {
      const ordinals = held.ordinals.slice(0, keep);
      const distinct = distinctOrdinals(ordinals);
      this.table?.retain(distinct);
      this.table?.release(held.distinct);
      membership[layer] = {ordinals, distinct};
    }
    const bytes = bandBytes(ids, positions, scalars, membership);
    this.held += bytes - band.bytes;
    this.heldPoints += keep - band.ids.length;
    this.changes++;
    const truncated = bandKey(band.depth, band.prefix);
    const kept: Band = {...band, ids, positions, scalars, membership, heldBelow: ids[keep - 1]! + 1n, bytes};
    this.bands.set(truncated, kept);
    this.index(kept, truncated);
  }
}

/** Morton distance from a band to the focus tile, at the focus's depth. Ties are broken by depth. */
function distance(band: Band, focus: EvictionFocus): bigint {
  const at = band.depth >= focus.depth ? band.prefix >> BigInt(2 * (band.depth - focus.depth)) : band.prefix;
  const delta = at - focus.prefix;
  return delta < 0n ? -delta : delta;
}
