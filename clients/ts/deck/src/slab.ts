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
 * **Dirty-span uploads, via GPU buffers this slab owns.** deck.gl's binary attribute path compares
 * by reference and re-uploads the whole live range whenever the reference moves — which, once
 * pieces paint as they absorb, is several full re-uploads per response: ~12 MB a paint at 10^6
 * marks, for an append that touched a fraction of it. So when a `Device` is attached, each
 * partition owns a luma.gl `Buffer` per attribute and writes exactly the span a band landed in;
 * deck is handed the buffer itself (`data.attributes.instancePositions` routes to
 * `Attribute.setExternalBuffer`, which binds without copying). The CPU arrays are kept regardless
 * — they are what growth re-writes from, and what picking reads — and remain the whole story
 * where no device is attached (tests, and `?gpu=0`).
 *
 * **The membership ordinal is a fourth attribute** (design §5.10): `u32` per point per band,
 * held here as `float32` for the shader, uploaded through the same dirty-span path as positions,
 * for the one layer the slab is told to carry (`membershipLayer`). The colour attribute stays
 * the column colour; which of the two the shader reads is a uniform on the layer, so the switch
 * between cluster and column colour uploads nothing. Changing the carried layer rewrites the
 * ordinals of every resident band from the columns the bands already hold — O(resident marks),
 * once per switch, and the only per-point pass a colouring interaction ever costs.
 */
import {Buffer as GpuBuffer} from '@luma.gl/core';
import type {Device} from '@luma.gl/core';
import type {Band, ScalarColumn} from '@tesseradb/client';
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
  /** The membership ordinal per mark, `0` for none, as the shader's `float32`. */
  ordinals: Float32Array;
  /**
   * The highlight bit per mark, `1` where the point satisfies the request's `highlight` and `0`
   * where it does not, as the shader's `float32` (`highlight-and-hierarchy.md` §5.3).
   *
   * **Every mark reads `1` where no highlight is set**, so the shader's switch is a uniform and
   * a map with no highlight draws exactly what it drew before this attribute existed. It rides
   * the same dirty-span path as the ordinals and is written when a band is, which is every time
   * it can have changed: the served set does not depend on the highlight, but the *bits* arrive
   * with the points, so a highlight change is a re-fetch and a re-fetch is a write.
   */
  highlights: Float32Array;
  length: number;
  /**
   * The partition's own GPU buffers, when a device is attached — capacity-sized, current to
   * `length`. **Stable across appends**: the object is recreated only when the buffers themselves
   * are (growth), which is what lets the layer memoise its attribute descriptors on it and deck
   * skip `setData` entirely on an unchanged partition.
   */
  gpu: GpuSlab | null;
};

export type GpuSlab = {positions: GpuBuffer; colours: GpuBuffer; picking: GpuBuffer; ordinals: GpuBuffer; highlights: GpuBuffer};

/**
 * deck's picking colour for instance `i` is `i + 1` in three little-endian bytes — a pure function
 * of the index. deck amortises *generating* that sequence in a global cache but still re-uploads
 * `4n` bytes of it whenever a layer's data changes, which at 1.7 × 10^6 drawn marks measured
 * ~210 ms/s of main thread (`bench-pickable` vs `-pickable0`). An owned buffer is written once per
 * growth and never touched again; the pattern is shared across partitions and grows monotonically,
 * so the byte-filling loop runs only over indices no partition has ever reached.
 */
let pickingPattern = new Uint8Array(0);

function pickingColours(capacity: number): Uint8Array {
  if (pickingPattern.length < capacity * 4) {
    const grown = new Uint8Array(capacity * 4);
    grown.set(pickingPattern);
    for (let i = pickingPattern.length / 4; i < capacity; i++) {
      const v = i + 1;
      grown[i * 4] = v & 255;
      grown[i * 4 + 1] = (v >> 8) & 255;
      grown[i * 4 + 2] = (v >> 16) & 255;
    }
    pickingPattern = grown;
  }
  return pickingPattern.subarray(0, capacity * 4);
}

type Slot = {from: number; length: number; band: Band};

function emptyDraw(): SlabDraw {
  return {
    ids: new BigUint64Array(0),
    positions: new Float32Array(0),
    colours: new Uint8Array(0),
    ordinals: new Float32Array(0),
    highlights: new Float32Array(0),
    length: 0,
    gpu: null
  };
}

/** One depth's resident marks — the whole of what used to be the slab. */
class Partition {
  key = '';
  lastUsed = 0;
  device: Device | null = null;
  private gpu: GpuSlab | null = null;
  /**
   * Marks whose GPU copy is behind their CPU copy, flushed as **one write per attribute per
   * sync**. A frame at 10^9 scale carries ~10^5 tiny bands, and a `buffer.write` per band is a
   * driver call per band — measured at 790 ms for one sync of 132k bands, which handed back the
   * entire cost the owned buffers had just removed. The ranges over-upload the gap between two
   * disjoint dirty slots, but appends land at the tail so the common flush is exactly the span
   * that arrived.
   */
  private dirtyPos: {from: number; to: number} | null = null;
  private dirtyCol: {from: number; to: number} | null = null;
  private dirtyOrd: {from: number; to: number} | null = null;
  private dirtyHigh: {from: number; to: number} | null = null;
  private ids = new BigUint64Array(0);
  private positions = new Float32Array(0);
  private colours = new Uint8Array(0);
  private ordinals = new Float32Array(0);
  private highlights = new Float32Array(0);
  /** The layer whose ordinals the partition carries; `''` for none (every ordinal 0). */
  private membershipLayer = '';
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

  sync(bands: readonly Band[], encoding: Encoding, encodingKey: string, colourBy: string | null, membershipLayer: string): SlabDraw {
    // A partition reactivated under a changed encoding recolours everything it holds — it was
    // invisible while the palette moved, and two colour scales on one map is not a state to render.
    const recolour = encodingKey !== this.encodingKey;
    this.encodingKey = encodingKey;
    const reordinal = membershipLayer !== this.membershipLayer;
    this.membershipLayer = membershipLayer;

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
      this.flushGpu();
      return this.publish(true, true, true, true);
    }
    if (appending === 0 && rewrite.length === 0 && !recolour && !reordinal) return this.draw;

    if (recolour) {
      // Every resident band, not only the frame's: a departed band is still drawn.
      for (const slot of this.slots.values()) {
        writeColours(this.colours, slot.from, slot.length, columnOf(slot.band, colourBy), encoding);
        this.uploadColours(slot.from, slot.length);
      }
    }
    if (reordinal) {
      for (const slot of this.slots.values()) this.writeOrdinals(slot.band, slot.from);
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
    this.flushGpu();
    const moved = appending > 0 || rewrite.length > 0;
    return this.publish(moved, moved, true, moved || reordinal);
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
    const ordinals = new Float32Array(capacity);
    const highlights = new Float32Array(capacity);
    ids.set(this.ids.subarray(0, this.live));
    positions.set(this.positions.subarray(0, this.live * 2));
    colours.set(this.colours.subarray(0, this.live * 4));
    ordinals.set(this.ordinals.subarray(0, this.live));
    highlights.set(this.highlights.subarray(0, this.live));
    this.ids = ids;
    this.positions = positions;
    this.colours = colours;
    this.ordinals = ordinals;
    this.highlights = highlights;
    this.capacity = capacity;
    this.reserveGpu();
  }

  /**
   * Fresh capacity-sized GPU buffers, retained marks re-written from the CPU arrays.
   *
   * Also the late-attach path: a device arriving after a partition holds data re-creates from
   * here, which is why the re-write covers `live` rather than assuming empty.
   */
  private reserveGpu(): void {
    if (!this.device) return;
    this.destroy();
    const usage = GpuBuffer.VERTEX | GpuBuffer.COPY_DST;
    this.gpu = {
      positions: this.device.createBuffer({byteLength: this.capacity * 8, usage}),
      colours: this.device.createBuffer({byteLength: this.capacity * 4, usage}),
      picking: this.device.createBuffer({byteLength: this.capacity * 4, usage}),
      ordinals: this.device.createBuffer({byteLength: this.capacity * 4, usage}),
      highlights: this.device.createBuffer({byteLength: this.capacity * 4, usage})
    };
    this.gpu.picking.write(pickingColours(this.capacity), 0);
    // Everything held is now behind the fresh buffers; the next flush rewrites it whole.
    if (this.live > 0) {
      this.dirtyPos = {from: 0, to: this.live};
      this.dirtyCol = {from: 0, to: this.live};
      this.dirtyOrd = {from: 0, to: this.live};
      this.dirtyHigh = {from: 0, to: this.live};
    }
  }

  /** Late device attach: build the buffers now and republish so the next frame binds them. */
  attach(device: Device): void {
    this.device = device;
    if (this.capacity > 0) this.reserveGpu();
    this.flushGpu();
    if (this.draw.length > 0 || this.gpu) this.publish(true, true, true, true);
  }

  destroy(): void {
    this.gpu?.positions.destroy();
    this.gpu?.colours.destroy();
    this.gpu?.picking.destroy();
    this.gpu?.ordinals.destroy();
    this.gpu?.highlights.destroy();
    this.gpu = null;
  }

  private static widen(held: {from: number; to: number} | null, from: number, to: number) {
    return held ? {from: Math.min(held.from, from), to: Math.max(held.to, to)} : {from, to};
  }

  private uploadColours(from: number, count: number): void {
    if (this.gpu) this.dirtyCol = Partition.widen(this.dirtyCol, from, from + count);
  }

  /** The accumulated dirty ranges, as one `write` per attribute. Called once per sync. */
  private flushGpu(): void {
    if (!this.gpu) return;
    if (this.dirtyPos) {
      const {from, to} = this.dirtyPos;
      this.gpu.positions.write(this.positions.subarray(from * 2, to * 2), from * 8);
      this.dirtyPos = null;
    }
    if (this.dirtyCol) {
      const {from, to} = this.dirtyCol;
      this.gpu.colours.write(this.colours.subarray(from * 4, to * 4), from * 4);
      this.dirtyCol = null;
    }
    if (this.dirtyOrd) {
      const {from, to} = this.dirtyOrd;
      this.gpu.ordinals.write(this.ordinals.subarray(from, to), from * 4);
      this.dirtyOrd = null;
    }
    if (this.dirtyHigh) {
      const {from, to} = this.dirtyHigh;
      this.gpu.highlights.write(this.highlights.subarray(from, to), from * 4);
      this.dirtyHigh = null;
    }
  }

  /**
   * A band's highlight bits into the slot at `at` — **ones where the band carries none**, which
   * is a band fetched under no highlight and is what makes the shader's switch a uniform rather
   * than a per-point test of whether the question was put.
   */
  private writeHighlights(band: Band, at: number): void {
    const n = band.ids.length;
    if (band.highlightBits) {
      for (let i = 0; i < n; i++) this.highlights[at + i] = band.highlightBits[i]!;
    } else this.highlights.fill(1, at, at + n);
    if (this.gpu) this.dirtyHigh = Partition.widen(this.dirtyHigh, at, at + n);
  }

  /** A band's ordinals for the carried layer into the slot at `at` — zeros where it has none. */
  private writeOrdinals(band: Band, at: number): void {
    const column = this.membershipLayer ? band.membership[this.membershipLayer] : undefined;
    const n = band.ids.length;
    if (column) this.ordinals.set(column.ordinals, at);
    else this.ordinals.fill(0, at, at + n);
    if (this.gpu) this.dirtyOrd = Partition.widen(this.dirtyOrd, at, at + n);
  }

  /** A band's marks into the slot at `at`. Whole-array `set` calls: a memcpy, not a loop. */
  private write(band: Band, at: number, encoding: Encoding, colourBy: string | null): void {
    this.reserve(at + band.ids.length);
    this.ids.set(band.ids, at);
    this.positions.set(band.positions, at * 2);
    writeColours(this.colours, at, band.ids.length, columnOf(band, colourBy), encoding);
    this.writeOrdinals(band, at);
    this.writeHighlights(band, at);
    if (this.gpu) this.dirtyPos = Partition.widen(this.dirtyPos, at, at + band.ids.length);
    this.uploadColours(at, band.ids.length);
  }

  /**
   * Republish the views deck.gl draws from. A fresh `subarray` is how an upload is *requested* —
   * it copies nothing, but it is a new reference, which is deck.gl's only signal that the contents
   * moved. Each array is republished exactly when its own contents changed.
   */
  private publish(ids: boolean, positions: boolean, colours: boolean, ordinals = false): SlabDraw {
    // The highlight bits are written exactly when a band is, so they republish with the positions.
    const highlights = positions;
    const changed = ids || positions || colours || ordinals || highlights || this.draw.length !== this.live;
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
      ordinals:
        ordinals || this.draw.ordinals.length !== this.live ? this.ordinals.subarray(0, this.live) : this.draw.ordinals,
      highlights:
        highlights || this.draw.highlights.length !== this.live
          ? this.highlights.subarray(0, this.live)
          : this.draw.highlights,
      length: this.live,
      gpu: this.gpu
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
  private device: Device | null = null;

  /**
   * Give the slab the GPU. From here every partition owns its buffers and uploads spans itself;
   * without it, the typed-array path carries everything, which is what tests and `?gpu=0` use.
   */
  attach(device: Device): void {
    this.device = device;
    for (const p of this.parts) p?.attach(device);
  }

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
  /**
   * `encoding` is the **column** colouring the colour attribute holds; cluster colour is the
   * layer's lookup texture and passes through here only as `membershipLayer`, the layer whose
   * ordinals every mark carries (`''` for none).
   */
  sync(bands: readonly Band[], depth: number, encoding: Encoding, colourBy: string | null, membershipLayer = ''): SlabDraw {
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
      for (const p of this.parts) p?.destroy();
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
      fresh.device = this.device;
      this.parts[slot] = fresh;
    }
    this.activeSlot = slot;
    const partition = this.parts[slot]!;
    partition.lastUsed = ++this.clock;
    const draw = partition.sync(bands, encoding, encodingIdentity(encoding), colourBy, membershipLayer);
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
      this.parts[lru]!.destroy();
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
    for (const p of this.parts) p?.destroy();
    this.parts = this.parts.map(() => null);
    this.identityKey = '';
    this.activeSlot = 0;
  }

  /**
   * The band and in-band index behind mark `index` of partition `slot` — what a hover reads its
   * scalars through, so a hint costs no request. A walk over the partition's slots, O(bands),
   * on a pointer move deck already throttles.
   */
  markAt(slot: number, index: number): {band: Band; i: number} | null {
    const p = this.parts[slot];
    if (!p) return null;
    for (const held of p.slots.values()) {
      if (index >= held.from && index < held.from + held.length) return {band: held.band, i: index - held.from};
    }
    return null;
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
