import type {Device, Texture} from '@luma.gl/core';
import {NEUTRAL, NO_ORDINAL, type ArtifactsProjection, type Rgba} from '@tesseradb/client';

/**
 * The lookup texture (design §5.10, decision 0100): one RGBA entry per session ordinal, sized to
 * the table's range in powers of two, read by the point shader as `colour = lut[ordinal]` when
 * colouring by cluster.
 *
 * **Every colouring interaction is a rewrite of this texture and never a per-point pass.** A
 * palette change, the level chosen to colour at, highlighting the opened artifact and dimming the
 * rest, and the switch between cluster and column colour (a uniform, not even a rewrite) — each
 * is O(artifacts): the table's range, which is bounded by resident marks through the ordinals'
 * refcounts, not by the layer. A per-point colour rewrite at several million marks is tens of
 * milliseconds and a 12 MB upload per interaction, and is the construction refused.
 *
 * `lut[o]` is the colour of `resolve(o)`: the walk up `o`'s parent links to the artifact served
 * at the chosen level (`SessionArtifactTable.resolve`), neutral where the walk fails — an edge
 * never seen, or the cut moved finer. Nothing here is a geometric guess (decision 0099).
 *
 * The texture is a fixed 1,024 texels wide and grows in rows, so an ordinal's texel is
 * `(o & 1023, o >> 10)` with no division in the shader.
 */

/** Texels per row — a power of two, so the shader masks and shifts. */
export const LUT_WIDTH = 1024;
export const LUT_SHIFT = 10;

export type LutInputs = {
  artifacts: Pick<ArtifactsProjection, 'table' | 'servedOrdinals' | 'colours'>;
  /** The level to colour at — `undefined` colours at the deepest served (§5.10). */
  level?: number;
  /** The opened artifact's ordinal: full colour for it and what resolves to it, the rest dimmed. */
  highlight?: number;
};

/** The dimmed form of a colour, when another artifact is highlighted: desaturated and faded. */
export function dimmed(c: Rgba): Rgba {
  const grey = (c[0] + c[1] + c[2]) / 3;
  return [Math.round((c[0] + grey) / 2), Math.round((c[1] + grey) / 2), Math.round((c[2] + grey) / 2), Math.round(c[3] * 0.4)];
}

/**
 * The table's bytes, over the ordinal range: `0` is the neutral, every other ordinal the colour
 * of what it resolves to. Returns the rows the texture needs, a power of two.
 */
export function buildLut(inputs: LutInputs): {data: Uint8Array; rows: number; range: number} {
  const {table, servedOrdinals, colours} = inputs.artifacts;
  const range = Math.max(1, table.range);
  let rows = 1;
  while (rows * LUT_WIDTH < range) rows *= 2;
  const data = new Uint8Array(LUT_WIDTH * rows * 4);
  const highlight = inputs.highlight ?? NO_ORDINAL;
  // The resolve is memoised per served ordinal: every ordinal resolving to the same artifact gets
  // the same bytes, and there are at most `served` distinct answers.
  const bytesOf = new Map<number, Rgba>();
  const colourOf = (resolved: number): Rgba => {
    let c = bytesOf.get(resolved);
    if (c) return c;
    const base = colours.get(resolved) ?? NEUTRAL;
    c = highlight !== NO_ORDINAL && resolved !== highlight ? dimmed(base) : base;
    bytesOf.set(resolved, c);
    return c;
  };
  const write = (o: number, c: Rgba) => {
    data[o * 4] = c[0];
    data[o * 4 + 1] = c[1];
    data[o * 4 + 2] = c[2];
    data[o * 4 + 3] = c[3];
  };
  write(NO_ORDINAL, highlight !== NO_ORDINAL ? dimmed(NEUTRAL) : NEUTRAL);
  for (let o = 1; o < range; o++) {
    if (!table.entry(o)) {
      write(o, NEUTRAL);
      continue;
    }
    const resolved = table.resolve(o, servedOrdinals, inputs.level);
    write(o, resolved === NO_ORDINAL ? (highlight !== NO_ORDINAL ? dimmed(NEUTRAL) : NEUTRAL) : colourOf(resolved));
  }
  return {data, rows, range};
}

/**
 * The GPU half: a texture the size the table needs, rewritten whole on every change — one
 * `writeData` of at most a few megabytes, against the twelve of a per-point pass.
 *
 * Owned by the host beside the slab, because it outlives every frame; attached to a device when
 * the map has one. Without a device it still builds the bytes, which is what a test reads.
 */
export class LookupTexture {
  private device: Device | null = null;
  private texture: Texture | null = null;
  private rows = 0;
  /** What the texture holds, for the caller to skip a rewrite when nothing moved. */
  private key = '';
  /** Texture writes since construction — the count a test asserts against attribute uploads. */
  writes = 0;
  bytes: Uint8Array = new Uint8Array(4);

  attach(device: Device): void {
    this.device = device;
    this.texture?.destroy();
    this.texture = null;
    this.rows = 0;
    // The held bytes go to the fresh device on the next update; force it.
    this.key = '';
  }

  /** The bound texture, if a device is attached and anything has been written. */
  get gpu(): Texture | null {
    return this.texture;
  }

  /**
   * Rewrite for the inputs, unless `key` says they are what the texture already holds. The key
   * is the caller's statement of the inputs' identity — the served set's version, the palette,
   * the level, the highlight — which is cheaper than diffing the bytes.
   */
  update(inputs: LutInputs, key: string): boolean {
    if (key === this.key) return false;
    this.key = key;
    const built = buildLut(inputs);
    this.bytes = built.data;
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

  /** The colour the texture holds for an ordinal — what the harness reads back for a point. */
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
