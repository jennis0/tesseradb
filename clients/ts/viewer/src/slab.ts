/**
 * Persistent mark buffers with stable per-band slots, retained per depth.
 *
 * **What this replaces.** Every redraw used to allocate fresh typed arrays and copy every drawn mark
 * into them, then hand deck.gl new references — a full re-upload on every frame of a drag. Here a
 * band is written **once**, into a slot it keeps, and a frame that adds nothing does nothing at all:
 * the same array references go back to deck.gl and no upload is issued.
 *
 * **Partitioned by `identityKey|depth`, every visited depth retained under one budget.** A wheel
 * notch changes depth, and with a single partition every notch voided the slab and rebuilt it —
 * measured at ~120 ms a flip, in sessions that flip on every notch and traverse three or four
 * depths in a gesture. Retention makes a flip a *swap*: each partition keeps its buffers and its
 * own `visible`-toggled layer, so deck.gl keeps every retained buffer set on the GPU and
 * revisiting a depth uploads nothing. Eviction is by mark budget, least-recently-used whole
 * partitions first, never the active one. An **identity** change still voids everything — a
 * different principal's bands may not be drawn at all, so that is a reset, never a swap.
 *
 * **Exact bands only.** A band at the requested depth under the current identity key is immutable
 * and cumulative. Stand-in bands are the opposite — clipped to whatever ground is not yet held, so
 * their extent changes every time a response lands — and they are rebuilt in `assemble.ts` instead.
 *
 * **Residency is allowed to exceed the frame.** A band that pans out of the render rectangle keeps
 * its slot and keeps being drawn — safely, because the frame's exact set is by construction every
 * band at this depth inside the render rectangle, so a resident band not in the frame is off
 * screen. {@link SLACK} bounds the excess before a partition compacts down to its frame.
 *
 * **What this does NOT do: dirty-span uploads.** deck.gl's binary attribute path compares by
 * reference, so appended marks re-upload the live range. Uploading only the appended span needs a
 * luma.gl `Buffer` owned outside deck's attribute manager — deferred until the arrival upload is
 * shown to be what a user feels.
 */
import type {Band, ScalarColumn} from '@tessera/client';
import {writeColours, type Encoding} from './colour.js';

/** Enough for a first view at the default budget without a growth step on the way. */
const MIN_CAPACITY = 1 << 16;

/**
 * Compact once residency reaches this multiple of what the frame draws.
 *
 * The trade is GPU vertex work against re-copying: every resident mark past the frame's own is
 * drawn for nothing, and every compaction copies the frame's marks again.
 */
const SLACK = 2;

/** A floor under {@link SLACK}, so a small view does not compact on every gesture. */
const MIN_RESIDENT = 200_000;

/**
 * Depths retained at once, and the total marks they may hold between them.
 *
 * One partition per depth the session actually visits, up to the slot count — a zoom traverses
 * three or four depths in one gesture, and each retained depth makes revisiting it free on both
 * CPU and GPU. The binding constraint is memory, not slots: ~20 B per mark CPU-side and ~12-16 B
 * on the GPU per retained depth, so the budget is expressed in marks and eviction frees whole
 * least-recently-used partitions until under it. The active partition is never evicted.
 */
const PARTITIONS = 6;
const SLAB_MARK_BUDGET = 6_000_000;

/**
 * The buffers to draw, and the extent to draw of them.
 *
 * The arrays are **views over the partition's own storage**, not copies. An unchanged partition
 * returns the identical object, and identical references are exactly what deck.gl needs to see to
 * skip an upload — so this must never be rebuilt defensively.
 */
export type SlabDraw = {
  ids: BigUint64Array;
  positions: Float32Array;
  colours: Uint8Array;
  length: number;
};

type Slot = {from: number; length: number; band: Band};

function emptyDraw(): SlabDraw {
  return {
    ids: new BigUint64Array(0),
    positions: new Float32Array(0),
    colours: new Uint8Array(0),
    length: 0
  };
}

/** One depth's resident marks — the whole of what used to be the slab. */
class Partition {
  key = '';
  lastUsed = 0;
  private ids = new BigUint64Array(0);
  private positions = new Float32Array(0);
  private colours = new Uint8Array(0);
  private capacity = 0;
  live = 0;
  frameMarks = 0;
  /**
   * One slot per **tile**, holding the band written into it. Keyed by tile because a refetched
   * tile arrives as a *new* band object; holding the band because that identity is exactly the
   * question "is this the served set I already wrote".
   */
  slots = new Map<bigint, Slot>();
  private encodingKey = '';
  draw: SlabDraw = emptyDraw();

  sync(bands: readonly Band[], encoding: Encoding, encodingKey: string, colourBy: string | null): SlabDraw {
    // A partition reactivated under a changed encoding recolours everything it holds — it was
    // invisible while the palette moved, and two colour scales on one map is not a state to render.
    const recolour = encodingKey !== this.encodingKey;
    this.encodingKey = encodingKey;

    // Three cases per band: already written, a refetch that fits its old slot, or new space needed.
    let wanted = 0;
    let appending = 0;
    let stale = 0;
    const rewrite: Band[] = [];
    for (const band of bands) {
      wanted += band.ids.length;
      const slot = this.slots.get(band.prefix);
      if (slot?.band === band) continue;
      if (slot && slot.length === band.ids.length) {
        rewrite.push(band);
        continue;
      }
      appending += band.ids.length;
      if (slot) stale += slot.length;
    }
    this.frameMarks = wanted;

    // Compact when residency drifts past the frame, when appends will not fit, or when a slot has
    // been superseded by a band that cannot reuse it — superseded marks are a *subset* of their
    // replacement and drawn twice they read as a denser patch, so that compaction is immediate.
    const wouldLive = this.live + appending;
    const compact =
      stale > 0 || wouldLive > Math.max(MIN_RESIDENT, SLACK * wanted) || wouldLive > this.capacity;

    if (compact) {
      this.rebuild(bands, encoding, colourBy);
      return this.publish(true, true, true);
    }
    if (appending === 0 && rewrite.length === 0 && !recolour) return this.draw;

    if (recolour) {
      // Every resident band, not only the frame's: a departed band is still drawn.
      for (const slot of this.slots.values()) {
        writeColours(this.colours, slot.from, slot.length, columnOf(slot.band, colourBy), encoding);
      }
    }
    // A refetch of the same size reuses its slot, so the common re-request neither grows the
    // partition nor moves anything already written.
    for (const band of rewrite) {
      const slot = this.slots.get(band.prefix)!;
      this.write(band, slot.from, encoding, colourBy);
      slot.band = band;
    }
    for (const band of bands) {
      const slot = this.slots.get(band.prefix);
      if (slot?.band === band) continue;
      this.write(band, this.live, encoding, colourBy);
      this.slots.set(band.prefix, {from: this.live, length: band.ids.length, band});
      this.live += band.ids.length;
    }
    const moved = appending > 0 || rewrite.length > 0;
    return this.publish(moved, moved, true);
  }

  private rebuild(bands: readonly Band[], encoding: Encoding, colourBy: string | null): void {
    let total = 0;
    for (const band of bands) total += band.ids.length;
    this.reserve(total);
    this.slots.clear();
    this.live = 0;
    for (const band of bands) {
      if (band.ids.length === 0) continue;
      this.write(band, this.live, encoding, colourBy);
      this.slots.set(band.prefix, {from: this.live, length: band.ids.length, band});
      this.live += band.ids.length;
    }
  }

  /** Geometric growth, so a filling session pays one amortised copy rather than one per band. */
  private reserve(needed: number): void {
    if (needed <= this.capacity) return;
    let capacity = Math.max(this.capacity, MIN_CAPACITY);
    while (capacity < needed) capacity *= 2;
    const ids = new BigUint64Array(capacity);
    const positions = new Float32Array(capacity * 2);
    const colours = new Uint8Array(capacity * 4);
    ids.set(this.ids.subarray(0, this.live));
    positions.set(this.positions.subarray(0, this.live * 2));
    colours.set(this.colours.subarray(0, this.live * 4));
    this.ids = ids;
    this.positions = positions;
    this.colours = colours;
    this.capacity = capacity;
  }

  /** A band's marks into the slot at `at`. Whole-array `set` calls: a memcpy, not a loop. */
  private write(band: Band, at: number, encoding: Encoding, colourBy: string | null): void {
    this.reserve(at + band.ids.length);
    this.ids.set(band.ids, at);
    this.positions.set(band.positions, at * 2);
    writeColours(this.colours, at, band.ids.length, columnOf(band, colourBy), encoding);
  }

  /**
   * Republish the views deck.gl draws from. A fresh `subarray` is how an upload is *requested* —
   * it copies nothing, but it is a new reference, which is deck.gl's only signal that the contents
   * moved. Each array is republished exactly when its own contents changed.
   */
  private publish(ids: boolean, positions: boolean, colours: boolean): SlabDraw {
    const changed = ids || positions || colours || this.draw.length !== this.live;
    if (!changed) return this.draw;
    this.draw = {
      ids: ids || this.draw.ids.length !== this.live ? this.ids.subarray(0, this.live) : this.draw.ids,
      positions:
        positions || this.draw.positions.length !== this.live * 2
          ? this.positions.subarray(0, this.live * 2)
          : this.draw.positions,
      colours:
        colours || this.draw.colours.length !== this.live * 4
          ? this.colours.subarray(0, this.live * 4)
          : this.draw.colours,
      length: this.live
    };
    return this.draw;
  }
}

/** What the viewer renders for one retained partition: a stable layer slot, and whether it shows. */
export type SlabLayer = {slot: number; draw: SlabDraw; active: boolean};

export class MarkSlab {
  /** Slot-stable, so each partition keeps the same deck.gl layer id for its whole life. */
  private parts: (Partition | null)[];

  constructor(private readonly partitions = PARTITIONS, private readonly markBudget = SLAB_MARK_BUDGET) {
    this.parts = Array.from({length: partitions}, () => null);
  }
  private activeSlot = 0;
  private clock = 0;
  private identityKey = '';

  private get active(): Partition | null {
    return this.parts[this.activeSlot] ?? null;
  }

  /** Marks in the active draw range — every one of them uploaded and rasterised. */
  get drawn(): number {
    return this.active?.live ?? 0;
  }

  /** Active marks resident but not in the last frame — what a compaction would reclaim. */
  get departed(): number {
    const p = this.active;
    return p ? p.live - p.frameMarks : 0;
  }

  get residentBands(): number {
    return this.active?.slots.size ?? 0;
  }

  /** Whether this exact band — not merely its tile — is what the active partition holds. */
  holds(band: Band): boolean {
    return this.active?.slots.get(band.prefix)?.band === band;
  }

  /**
   * Bring the right partition up to date for one frame, and return what to draw.
   *
   * Returns the *same* object as last time when nothing changed — the caller passes it straight to
   * deck.gl, which compares references, and no work reaches the GPU.
   */
  sync(bands: readonly Band[], depth: number, encoding: Encoding, colourBy: string | null): SlabDraw {
    // **An empty frame changes nothing.** A view over ground this principal cannot see is not
    // evidence that anything resident has expired; the identity change that does matter arrives as
    // a cleared frame in the viewer, which calls {@link clear}.
    if (bands.length === 0) {
      const p = this.active;
      if (p) p.frameMarks = 0;
      return p?.draw ?? emptyDraw();
    }

    // **An identity change voids every partition.** A different principal or bundle is a different
    // visible set, and a band from the old one may not be drawn at all — a reset, never a swap.
    const identity = bands[0]!.identityKey;
    if (identity !== this.identityKey) {
      this.identityKey = identity;
      this.parts = this.parts.map(() => null);
    }

    const key = `${identity}|${depth}`;
    let slot = this.parts.findIndex((p) => p?.key === key);
    if (slot < 0) {
      // A new depth takes an empty slot, else evicts the least recently used.
      slot = this.parts.findIndex((p) => p === null);
      if (slot < 0) {
        slot = 0;
        for (let i = 1; i < this.parts.length; i++) {
          if (this.parts[i]!.lastUsed < this.parts[slot]!.lastUsed) slot = i;
        }
      }
      const fresh = new Partition();
      fresh.key = key;
      this.parts[slot] = fresh;
    }
    this.activeSlot = slot;
    const partition = this.parts[slot]!;
    partition.lastUsed = ++this.clock;
    const draw = partition.sync(bands, encoding, encodingIdentity(encoding), colourBy);
    this.enforceBudget();
    return draw;
  }

  /** Free whole least-recently-used partitions until residency fits the mark budget. */
  private enforceBudget(): void {
    for (;;) {
      let total = 0;
      let lru = -1;
      for (let i = 0; i < this.parts.length; i++) {
        const p = this.parts[i];
        if (!p) continue;
        total += p.live;
        if (i !== this.activeSlot && (lru < 0 || p.lastUsed < this.parts[lru]!.lastUsed)) lru = i;
      }
      if (total <= this.markBudget || lru < 0) return;
      this.parts[lru] = null;
    }
  }

  /**
   * Every retained partition, for the viewer to render as one `visible`-toggled layer each.
   *
   * Only the active partition shows: the other depth's marks over the same ground would double the
   * density everywhere both hold data. Retention is about what stays resident — in memory and on
   * the GPU — not about what is drawn.
   */
  layers(): SlabLayer[] {
    const out: SlabLayer[] = [];
    for (let i = 0; i < this.parts.length; i++) {
      const p = this.parts[i];
      if (!p) continue;
      out.push({slot: i, draw: p.draw, active: i === this.activeSlot});
    }
    return out;
  }

  /** Drop everything — a principal change, or a view with nothing to draw at all. */
  clear(): void {
    this.parts = this.parts.map(() => null);
    this.identityKey = '';
    this.activeSlot = 0;
  }

  /** Marks held across every retained partition — the figure the budget bounds. */
  get residentMarks(): number {
    let total = 0;
    for (const p of this.parts) if (p) total += p.live;
    return total;
  }
}

function columnOf(band: Band, colourBy: string | null): ScalarColumn | undefined {
  return colourBy ? band.scalars[colourBy] : undefined;
}

/**
 * What makes two encodings the same colouring.
 *
 * The rank map and the numeric domain both grow as marks arrive — stickily, never narrowing — so
 * comparing them by reference would recolour on every response for a palette that did not move.
 * Comparing by size is enough precisely *because* they only ever grow.
 */
function encodingIdentity(encoding: Encoding): string {
  switch (encoding.kind) {
    case 'uniform':
    case 'unmapped':
      return encoding.kind;
    case 'category':
      return `category|${encoding.column}|${Object.keys(encoding.rankOfCode).length}`;
    case 'numeric':
      return `numeric|${encoding.column}|${encoding.domain.min}|${encoding.domain.max}`;
  }
}
