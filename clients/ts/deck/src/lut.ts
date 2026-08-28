import type {Device, Texture} from '@luma.gl/core';
import {NEUTRAL, NO_ORDINAL, type ArtifactsProjection, type ArtifactTableChange, type Rgba} from '@tesseradb/client';

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
 * `lut[o]` is the colour of `resolve(o)`: the walk up `o`'s parent links to the nearest artifact
 * a colour is known for at the chosen level (`SessionArtifactTable.resolve`), neutral where the
 * walk fails — an edge never seen. Nothing here is a geometric guess (decision 0099).
 *
 * **The walk stops at what is colourable, not at what the current view was served.** The colour
 * map covers every artifact the session table holds (`store.ts`), which includes the ones a
 * point response's own frame named while the debounced artifact channel was still on the last
 * cut. Resolving against the channel's set alone drew a band neutral for as long as it took the
 * channel to catch up, and again for every band held under a coarser cut once it had — the grey
 * banding on a zoom in. A point coloured this way still wears the colour of an artifact the wire
 * said it belongs to, which is what exact-only asks (§5.10).
 *
 * The texture is a fixed 1,024 texels wide and grows in rows, so an ordinal's texel is
 * `(o & 1023, o >> 10)` with no division in the shader.
 *
 * ## Rewriting is bounded by what changed, not by what the table holds
 *
 * The coverage rule above is about **which ordinals have a colour** and is unchanged by anything
 * here. What is bounded is the *cadence*: the session table holds whole levels once the artifact
 * channel has promoted them — 226k entries on GeoNames — and a settled view at new ground names a
 * few hundred it had not seen. Rebuilding all 226k texels and uploading the megabyte per settle
 * costs the hold rather than the change.
 *
 * So a rewrite is a **patch** — the changed ordinals' texels, and only the texture rows they fall
 * in — when the table's change list (`SessionArtifactTable.changesSince`) says every change since
 * the last build was an ordinal being **named**, and the caller handed in the same colour map
 * object. Anything else rebuilds whole. The rule is that narrow because a texel is
 * `colour(resolve(o))`, so a change to one entry moves every texel resolving *through* it: a
 * colour arriving, a link set on an entry that was already here, a slot freed — each can move a
 * texel that is not its own. A newly named ordinal cannot: nothing points at it yet, and the link
 * that would is reported against the child.
 *
 * A different colour map object is a recolour — the palette or the ground changed, and every texel
 * with it — because the store extends the map in place while the palette holds (`store.ts`).
 * Highlighting an artifact and choosing the level to colour at also move every texel; both are in
 * the key, and both are gestures rather than settles.
 */

/** Texels per row — a power of two, so the shader masks and shifts. */
export const LUT_WIDTH = 1024;
export const LUT_SHIFT = 10;

export type LutInputs = {
  artifacts: Pick<ArtifactsProjection, 'table' | 'colours'>;
  /** The level to colour at — `undefined` colours at the deepest served (§5.10). */
  level?: number;
  /** The opened artifact's ordinal: full colour for it and what resolves to it, the rest dimmed. */
  highlight?: number;
};

/**
 * The ordinals of a change list where **every** change is an ordinal being named, else null: the
 * one shape a patch may serve (see the head of this module). A null change list — a reader
 * further behind than the table's journal — is the same answer.
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
 * Writes one ordinal's texel into `data`, for the inputs given: the colour of what the ordinal
 * resolves to, dimmed unless it is the highlight, neutral where the walk fails or the slot is
 * free. Stated once and used by both the whole build and the patch, so the two cannot disagree.
 *
 * The resolve is memoised per resolved ordinal — every ordinal resolving to the same artifact
 * gets the same bytes, and there are at most `colours.size` distinct answers — so the memo is
 * worth building for a whole pass and is handed back for a patch to share.
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
    // **Settled** means the texel is final for this map: neutral by right (no ordinal, a freed
    // slot) or coloured. A live ordinal whose walk finds no colour is *not* settled — the map
    // simply has not been extended to it yet — and the caller keeps it to try again.
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
 * for none. The caller uploads that span and nothing else; ordinals are named from the top of
 * the range, so on a settle the span is a tail of a row or two.
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
 * The GPU half: a texture the size the table needs, written by the rows that moved — one
 * `writeData` of a row or two on a settle, of at most a few megabytes when everything moved,
 * against the twelve of a per-point pass.
 *
 * Owned by the host beside the slab, because it outlives every frame; attached to a device when
 * the map has one. Without a device it still builds the bytes, which is what a test reads.
 */
export class LookupTexture {
  private device: Device | null = null;
  private texture: Texture | null = null;
  private rows = 0;
  /**
   * The inputs' identity **other than the table and the colours**: the palette, the level, the
   * highlight. Those two are compared directly — the table by its version, the colour map by
   * identity — because they are what a patch is decided from.
   */
  private key = '';
  /** The table version and colour map the held bytes were built from. */
  private builtAt = -1;
  private builtFrom: ReadonlyMap<number, Rgba> | null = null;
  /** The rows {@link bytes} holds, which is the texture's height once one is attached. */
  private builtRows = 0;
  /**
   * Live ordinals whose texel is neutral only because the colour map had not reached them when
   * they were written. **An ordinal is named before it is coloured**: a points frame's own
   * artifacts frame grows the table, the layer draws, and the store extends the colour map *in
   * place* a moment later — the same object, which this texture compares by identity. Without
   * this set that extension was invisible and the ordinal stayed grey until something unrelated
   * rebuilt the texture whole (found on GeoNames, 2026-08-28: the map opened grey and flashed
   * coloured on a zoom). Re-tried on every update that would otherwise write nothing.
   */
  private pending = new Set<number>();
  /** Texture writes since construction — the count a test asserts against attribute uploads. */
  writes = 0;
  bytes: Uint8Array = new Uint8Array(4);

  attach(device: Device): void {
    this.device = device;
    this.texture?.destroy();
    this.texture = null;
    this.rows = 0;
    // The held bytes go to the fresh device on the next update, whole; force it.
    this.key = '';
    this.builtAt = -1;
    this.builtFrom = null;
  }

  /** The bound texture, if a device is attached and anything has been written. */
  get gpu(): Texture | null {
    return this.texture;
  }

  /**
   * Rewrite for the inputs, unless the texture already holds them.
   *
   * `key` is the caller's statement of everything about the inputs **except the table and the
   * colour map** — the palette, the level, the highlight. Those two are compared here: the table
   * by its version, the colours by identity, which is what tells a table that gained a few
   * ordinals from a map whose every colour moved. The first patches the rows those ordinals fall
   * in; the second rebuilds whole (the rule, and why it is that narrow, is at the head of this
   * module). The ground the map is drawn on needs no place in the key for the same reason: a
   * scheme change reaches here as a rebuilt colour map.
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

  /**
   * Re-try the pending ordinals against the current map: the ones whose walk now finds a colour
   * are written and uploaded, the rest wait. Nothing is written when none is ready, so a texture
   * with nothing pending costs a set-size check per update.
   */
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
