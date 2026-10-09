import type {Device, Texture} from '@luma.gl/core';
import {NO_ORDINAL, type ArtifactsProjection, type ArtifactTableChange, type Rgba} from '@mosaica/client';
import {NEUTRAL} from '@mosaica/client/internal';

/**
 * The lookup texture: one RGBA texel per session ordinal, read by the mark shader as
 * `colour = lut[ordinal]` when colouring by cluster. A palette change, the level coloured at and
 * a highlight each rewrite this texture in O(artifacts) instead of recolouring every mark.
 *
 * `lut[o]` is the colour of `resolve(o)`: the walk up `o`'s parent links to the nearest artifact
 * the colour map has a colour for at the chosen level, neutral where the walk finds none. The
 * colour map covers every artifact the session table holds, including those a points response
 * named before the artifact channel caught up, so a mark wears the colour of an artifact the
 * server said it belongs to.
 *
 * The texture is 1,024 texels wide and grows in rows, so an ordinal's texel is
 * `(o & 1023, o >> 10)`.
 *
 * An update patches only the rows of the changed ordinals when every table change since the last
 * build was an ordinal being named and the colour map is the same object. Anything else rebuilds
 * whole. A texel is `colour(resolve(o))`, so a new colour, a new link or a freed slot can move
 * texels that resolve through the changed entry. A newly named ordinal cannot: nothing points at
 * it yet. The store extends the colour map in place while the palette holds, so a new map object
 * means every texel may have moved.
 */

/** Texels per row, a power of two so the shader masks and shifts. */
export const LUT_WIDTH = 1024;
export const LUT_SHIFT = 10;

export type LutInputs = {
  artifacts: Pick<ArtifactsProjection, 'table' | 'colours'>;
  /** The level to colour at; `undefined` colours at the deepest served. */
  level?: number;
  /** The opened artifact's ordinal: full colour for it and what resolves to it, the rest dimmed. */
  highlight?: number;
};

/**
 * The ordinals of a change list when every change is an ordinal being named, else null. A null
 * change list, from a reader further behind than the table's journal, gives null.
 */
function namedOnly(changes: readonly ArtifactTableChange[] | null): number[] | null {
  if (!changes) return null;
  const out: number[] = [];
  for (const c of changes) {
    if (c.kind !== 'named') return null;
    out.push(c.ordinal);
  }
  return out;
}

/** The dimmed form of a colour, when another artifact is highlighted: desaturated and faded. */
export function dimmed(c: Rgba): Rgba {
  const grey = (c[0] + c[1] + c[2]) / 3;
  return [Math.round((c[0] + grey) / 2), Math.round((c[1] + grey) / 2), Math.round((c[2] + grey) / 2), Math.round(c[3] * 0.4)];
}

/** The rows a range of ordinals needs: a power of two, so a grown texture doubles. */
export function lutRows(range: number): number {
  let rows = 1;
  while (rows * LUT_WIDTH < Math.max(1, range)) rows *= 2;
  return rows;
}

/**
 * A writer of one ordinal's texel into `data`: the colour of what the ordinal resolves to, dimmed
 * unless it is the highlight, neutral where the walk fails or the slot is free. Returns false for
 * a live ordinal whose walk found no colour yet. The build and the patch both use it.
 */
function texelWriter(inputs: LutInputs, data: Uint8Array): (ordinal: number) => boolean {
  const {table, colours} = inputs.artifacts;
  const highlight = inputs.highlight ?? NO_ORDINAL;
  const bytesOf = new Map<number, Rgba>();
  const colourOf = (resolved: number): Rgba => {
    let c = bytesOf.get(resolved);
    if (c) return c;
    const base = colours.get(resolved) ?? NEUTRAL;
    c = highlight !== NO_ORDINAL && resolved !== highlight ? dimmed(base) : base;
    bytesOf.set(resolved, c);
    return c;
  };
  const neutral = highlight !== NO_ORDINAL ? dimmed(NEUTRAL) : NEUTRAL;
  return (o: number) => {
    let c: Rgba;
    // Settled means the texel is final for this map: neutral for no ordinal or a freed slot, or
    // coloured. A live ordinal with no colour yet is kept by the caller to try again.
    let settled = true;
    if (o === NO_ORDINAL) c = neutral;
    else if (!table.entry(o)) c = NEUTRAL;
    else {
      const resolved = table.resolve(o, colours, inputs.level);
      if (resolved === NO_ORDINAL) {
        c = neutral;
        settled = false;
      } else c = colourOf(resolved);
    }
    data[o * 4] = c[0];
    data[o * 4 + 1] = c[1];
    data[o * 4 + 2] = c[2];
    data[o * 4 + 3] = c[3];
    return settled;
  };
}

/**
 * The table's bytes, over the ordinal range: `0` is the neutral, every other ordinal the colour
 * of what it resolves to. Returns the rows the texture needs, a power of two.
 */
export function buildLut(inputs: LutInputs): {data: Uint8Array; rows: number; range: number; pending: number[]} {
  const range = Math.max(1, inputs.artifacts.table.range);
  const rows = lutRows(range);
  const data = new Uint8Array(LUT_WIDTH * rows * 4);
  const write = texelWriter(inputs, data);
  const pending: number[] = [];
  for (let o = 0; o < range; o++) if (!write(o)) pending.push(o);
  return {data, rows, range, pending};
}

/**
 * Rewrite `ordinals`' texels in `data` and return the half-open row span they fall in, or null
 * for none. The caller uploads only that span.
 */
export function patchLut(inputs: LutInputs, data: Uint8Array, ordinals: readonly number[]): {from: number; to: number; pending: number[]} | null {
  if (ordinals.length === 0) return null;
  const write = texelWriter(inputs, data);
  const pending: number[] = [];
  let from = Infinity;
  let to = 0;
  for (const o of ordinals) {
    if (o * 4 + 3 >= data.length) continue;
    if (!write(o)) pending.push(o);
    const row = o >> LUT_SHIFT;
    if (row < from) from = row;
    if (row + 1 > to) to = row + 1;
  }
  return to > from ? {from, to, pending} : null;
}

/**
 * The lookup texture on the GPU, written by the rows that changed. `MosaicaLayer` makes one on its
 * deck's device and releases it when finalised; a host may pass its own and attach it. Without a
 * device it still builds the bytes.
 */
export class LookupTexture {
  private device: Device | null = null;
  private texture: Texture | null = null;
  private rows = 0;
  /** The caller's key for the palette, level and highlight; see {@link update}. */
  private key = '';
  /** The table version and colour map the held bytes were built from. */
  private builtAt = -1;
  private builtFrom: ReadonlyMap<number, Rgba> | null = null;
  /** The rows {@link bytes} holds. */
  private builtRows = 0;
  /**
   * Live ordinals written neutral because the colour map had no colour for them yet. The store
   * extends the map in place after the table names an ordinal, and an in-place extension does not
   * change the map's identity, so these are retried on every update.
   */
  private pending = new Set<number>();
  /** Texture writes since construction. */
  writes = 0;
  bytes: Uint8Array = new Uint8Array(4);

  attach(device: Device): void {
    this.device = device;
    this.texture?.destroy();
    this.texture = null;
    this.rows = 0;
    // Force a whole rebuild onto the new device at the next update.
    this.key = '';
    this.builtAt = -1;
    this.builtFrom = null;
  }

  /** The bound texture, if a device is attached and anything has been written. */
  get gpu(): Texture | null {
    return this.texture;
  }

  /**
   * Rewrite for the inputs, unless the texture already holds them. Returns whether anything was
   * written.
   *
   * `key` covers everything about the inputs except the table and the colour map: the palette, the
   * level, the highlight. The table is compared by version and the colour map by identity, which
   * decides between a patch and a whole rebuild. A colour scheme change arrives as a new colour
   * map.
   */
  update(inputs: LutInputs, key: string): boolean {
    const {table, colours} = inputs.artifacts;
    if (key === this.key && table.version === this.builtAt && colours === this.builtFrom) return this.settle(inputs);
    const rows = lutRows(table.range);
    const patch =
      key === this.key && colours === this.builtFrom && rows === this.builtRows && this.bytes.length === LUT_WIDTH * rows * 4
        ? namedOnly(table.changesSince(this.builtAt))
        : null;
    this.key = key;
    this.builtAt = table.version;
    this.builtFrom = colours;
    if (patch) {
      const span = patchLut(inputs, this.bytes, patch);
      if (span) {
        for (const o of span.pending) this.pending.add(o);
        this.upload(span);
      }
      // Ordinals waiting from earlier patches may have been coloured by the same extension.
      const settled = this.settle(inputs);
      return span !== null || settled;
    }
    const built = buildLut(inputs);
    this.bytes = built.data;
    this.builtRows = built.rows;
    this.pending = new Set(built.pending);
    if (this.device) {
      if (!this.texture || this.rows !== built.rows) {
        this.texture?.destroy();
        this.texture = this.device.createTexture({
          format: 'rgba8unorm',
          width: LUT_WIDTH,
          height: built.rows,
          sampler: {minFilter: 'nearest', magFilter: 'nearest'}
        });
        this.rows = built.rows;
      }
      this.texture.writeData(built.data, {width: LUT_WIDTH, height: built.rows});
      this.writes += 1;
    }
    return true;
  }

  /** Upload one row span of the held bytes, where a device is attached. */
  private upload(span: {from: number; to: number}): void {
    if (!this.texture) return;
    this.texture.writeData(this.bytes.subarray(span.from * LUT_WIDTH * 4, span.to * LUT_WIDTH * 4), {y: span.from, width: LUT_WIDTH, height: span.to - span.from});
    this.writes += 1;
  }

  /** Write and upload the pending ordinals that now resolve to a colour; the rest wait. */
  private settle(inputs: LutInputs): boolean {
    if (this.pending.size === 0) return false;
    const {table, colours} = inputs.artifacts;
    const ready: number[] = [];
    for (const o of this.pending) {
      if (!table.entry(o)) {
        this.pending.delete(o);
        continue;
      }
      if (table.resolve(o, colours, inputs.level) !== NO_ORDINAL) ready.push(o);
    }
    if (ready.length === 0) return false;
    const span = patchLut(inputs, this.bytes, ready);
    for (const o of ready) this.pending.delete(o);
    if (span) this.upload(span);
    return span !== null;
  }

  /** The colour the texture holds for an ordinal. */
  colourOf(ordinal: number): Rgba {
    const at = ordinal * 4;
    if (at + 3 >= this.bytes.length) return NEUTRAL;
    return [this.bytes[at]!, this.bytes[at + 1]!, this.bytes[at + 2]!, this.bytes[at + 3]!];
  }

  destroy(): void {
    this.texture?.destroy();
    this.texture = null;
    this.rows = 0;
    this.key = '';
  }
}
