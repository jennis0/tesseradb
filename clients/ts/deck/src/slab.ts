/**
 * Persistent mark buffers for exact bands, with a stable slot per tile, retained per depth.
 *
 * A band is written once into its slot. A frame that adds nothing returns the same array
 * references, and deck.gl, which compares by reference, uploads nothing.
 *
 * Partitions are keyed by `identityKey|depth`. Each visited depth keeps its buffers and its own
 * `visible`-toggled layer, so returning to a depth uploads nothing. Whole least-recently-used
 * partitions other than the active one are evicted to stay under a mark budget. An identity change
 * (a different principal or bundle) voids every partition, since no band from the old visible set
 * may be drawn.
 *
 * Stand-in bands change extent with every response and are concatenated in `assemble.ts`.
 *
 * A band that pans out of the render rectangle keeps its slot and is still drawn. It is off
 * screen, since the frame's exact set is every band at this depth inside the render rectangle.
 * The partition compacts once residency passes the larger of {@link MIN_RESIDENT} and
 * {@link SLACK} times the frame.
 *
 * With a `Device` attached, each partition owns a luma.gl `Buffer` per attribute and writes only
 * the span a band landed in; deck binds the buffer without copying. deck's own binary attribute
 * path re-uploads the whole live range whenever a reference changes. The CPU arrays are kept for
 * growth and picking, and are the only copy without a device.
 *
 * Each mark also carries its membership ordinal for one layer (`membershipLayer`) as `float32`,
 * alongside the column colour. A uniform on the layer chooses which the shader reads. Changing
 * the carried layer rewrites every resident band's ordinals once.
 */
import {Buffer as GpuBuffer} from '@luma.gl/core';
import type {Device} from '@luma.gl/core';
import type {Band, ScalarColumn} from '@tesseradb/client';
import {encodingSignature, writeColours, type Encoding} from './colour.js';

/** Enough for a first view at the default budget without a growth step on the way. */
const MIN_CAPACITY = 1 << 16;

/**
 * Compact once residency reaches this multiple of what the frame draws. Resident marks outside
 * the frame cost vertex work; each compaction copies the frame's marks again.
 */
const SLACK = 2;

/** A floor under {@link SLACK}, so a small view does not compact on every gesture. */
const MIN_RESIDENT = 200_000;

/**
 * Depths retained at once, and the total marks they may hold between them. A mark costs about
 * 20 bytes on the CPU and 12 to 16 on the GPU, so the budget is in marks.
 */
const PARTITIONS = 6;
const SLAB_MARK_BUDGET = 6_000_000;

/**
 * The buffers to draw, and the extent to draw of them. The arrays are views over the partition's
 * storage. An unchanged partition returns the same object, which is how deck.gl skips an upload,
 * so do not copy or rebuild it.
 */
export type SlabDraw = {
  ids: BigUint64Array;
  positions: Float32Array;
  colours: Uint8Array;
  /** The membership ordinal per mark, `0` for none, as the shader's `float32`. */
  ordinals: Float32Array;
  /**
   * The highlight bit per mark as `float32`: `1` where the point satisfies the request's
   * `highlight`, `0` where it does not, and `1` everywhere when no highlight is set. The bits
   * arrive with the points, so they change only when a band is written.
   */
  highlights: Float32Array;
  length: number;
  /**
   * The partition's GPU buffers when a device is attached: capacity-sized, current to `length`.
   * The object changes only when the buffers grow, so the layer memoises its attribute
   * descriptors on it.
   */
  gpu: GpuSlab | null;
};

export type GpuSlab = {positions: GpuBuffer; colours: GpuBuffer; picking: GpuBuffer; ordinals: GpuBuffer; highlights: GpuBuffer};

/**
 * deck's picking colour for instance `i` is `i + 1` in three little-endian bytes. deck re-uploads
 * that sequence whenever a layer's data changes; an owned buffer is written once per growth
 * instead. The pattern is shared across partitions and only grows.
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

/** One depth's resident marks. */
class Partition {
  key = '';
  lastUsed = 0;
  device: Device | null = null;
  private gpu: GpuSlab | null = null;
  /**
   * Marks whose GPU copy is behind their CPU copy, flushed as one write per attribute per sync,
   * since a frame can carry around 10^5 small bands. A range spans the gap between disjoint dirty
   * slots; appends land at the tail, so the usual flush is the span that arrived.
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
   * One slot per tile, holding the band written into it. A refetched tile arrives as a new band
   * object, so band identity says whether the slot already holds the served set.
   */
  slots = new Map<bigint, Slot>();
  private encodingKey = '';
  draw: SlabDraw = emptyDraw();

  sync(bands: readonly Band[], encoding: Encoding, encodingKey: string, colourBy: string | null, membershipLayer: string): SlabDraw {
    // A partition reactivated under a changed encoding recolours everything it holds.
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

    // Compact when residency drifts past the frame, when appends will not fit, or when a band
    // cannot reuse its tile's slot. A superseded slot's marks are a subset of the new band's and
    // would draw twice.
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
      // Every resident band, since a band outside the frame is still drawn.
      for (const slot of this.slots.values()) {
        writeColours(this.colours, slot.from, slot.length, columnOf(slot.band, colourBy), encoding);
        this.uploadColours(slot.from, slot.length);
      }
    }
    if (reordinal) {
      for (const slot of this.slots.values()) this.writeOrdinals(slot.band, slot.from);
    }
    // A refetch of the same size reuses its slot.
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

  /** Grows by doubling, so the copies amortise. */
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
   * Fresh capacity-sized GPU buffers, with every live mark marked dirty so the next flush writes
   * it. A device attached after the partition holds data comes through here too.
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

  /** A band's highlight bits into the slot at `at`, ones where the band was fetched with no highlight. */
  private writeHighlights(band: Band, at: number): void {
    const n = band.ids.length;
    if (band.highlightBits) {
      for (let i = 0; i < n; i++) this.highlights[at + i] = band.highlightBits[i]!;
    } else this.highlights.fill(1, at, at + n);
    if (this.gpu) this.dirtyHigh = Partition.widen(this.dirtyHigh, at, at + n);
  }

  /** A band's ordinals for the carried layer into the slot at `at`, zeros where it has none. */
  private writeOrdinals(band: Band, at: number): void {
    const column = this.membershipLayer ? band.membership[this.membershipLayer] : undefined;
    const n = band.ids.length;
    if (column) this.ordinals.set(column.ordinals, at);
    else this.ordinals.fill(0, at, at + n);
    if (this.gpu) this.dirtyOrd = Partition.widen(this.dirtyOrd, at, at + n);
  }

  /** A band's marks into the slot at `at`. */
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
   * Republish the views deck.gl draws from. A new `subarray` copies nothing but is a new
   * reference, which is how deck.gl learns the contents changed, so each array is republished only
   * when its contents did.
   */
  private publish(ids: boolean, positions: boolean, colours: boolean, ordinals = false): SlabDraw {
    // The highlight bits are written when a band is, so they republish with the positions.
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
  /** Indexed by slot, so each partition keeps the same deck.gl layer id for its life. */
  private parts: (Partition | null)[];

  constructor(private readonly partitions = PARTITIONS, private readonly markBudget = SLAB_MARK_BUDGET) {
    this.parts = Array.from({length: partitions}, () => null);
  }
  private activeSlot = 0;
  private clock = 0;
  private identityKey = '';
  private device: Device | null = null;

  /** Give the slab the GPU; from here every partition owns its buffers and uploads spans itself. */
  attach(device: Device): void {
    this.device = device;
    for (const p of this.parts) p?.attach(device);
  }

  private get active(): Partition | null {
    return this.parts[this.activeSlot] ?? null;
  }

  /** Marks in the active draw range. */
  get drawn(): number {
    return this.active?.live ?? 0;
  }

  /** Active marks resident but not in the last frame, which a compaction would reclaim. */
  get departed(): number {
    const p = this.active;
    return p ? p.live - p.frameMarks : 0;
  }

  get residentBands(): number {
    return this.active?.slots.size ?? 0;
  }

  /** Whether the active partition holds this band object, not only a band for its tile. */
  holds(band: Band): boolean {
    return this.active?.slots.get(band.prefix)?.band === band;
  }

  /**
   * Bring the partition for `depth` up to date and return what to draw: the same object as last
   * time when nothing changed. `encoding` is the column colouring the colour attribute holds;
   * cluster colour comes from the layer's lookup texture, and reaches here only as
   * `membershipLayer`, the layer whose ordinals every mark carries (`''` for none).
   */
  sync(bands: readonly Band[], depth: number, encoding: Encoding, colourBy: string | null, membershipLayer = ''): SlabDraw {
    // An empty frame changes nothing resident. A principal change reaches here through clear().
    if (bands.length === 0) {
      const p = this.active;
      if (p) p.frameMarks = 0;
      return p?.draw ?? emptyDraw();
    }

    // A different principal or bundle is a different visible set: void every partition.
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
    const draw = partition.sync(bands, encoding, encodingSignature(encoding), colourBy, membershipLayer);
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
   * Every retained partition, for the viewer to render as one `visible`-toggled layer each. Only
   * the active one shows, since two depths over the same ground would double the density.
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

  /** Drop everything, on a principal change or a view with nothing to draw. */
  clear(): void {
    for (const p of this.parts) p?.destroy();
    this.parts = this.parts.map(() => null);
    this.identityKey = '';
    this.activeSlot = 0;
  }

  /**
   * The band and in-band index behind mark `index` of partition `slot`, so a hover reads scalars
   * without a request. O(bands).
   */
  markAt(slot: number, index: number): {band: Band; i: number} | null {
    const p = this.parts[slot];
    if (!p) return null;
    for (const held of p.slots.values()) {
      if (index >= held.from && index < held.from + held.length) return {band: held.band, i: index - held.from};
    }
    return null;
  }

  /** Marks held across every retained partition, which the budget bounds. */
  get residentMarks(): number {
    let total = 0;
    for (const p of this.parts) if (p) total += p.live;
    return total;
  }
}

function columnOf(band: Band, colourBy: string | null): ScalarColumn | undefined {
  return colourBy ? band.scalars[colourBy] : undefined;
}
