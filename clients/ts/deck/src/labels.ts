/**
 * Label placement (design §5.10): names and counts at each artifact's centroid, placed by
 * priority into a spatial hash — O(K) for K labels, so 10⁴ labels cost what a few hundred
 * do — with a leader line when a label had to move off its centroid to fit.
 *
 * A label moves at most `MAX_DISPLACEMENT` pixels from its centroid. Beyond that a leader
 * would cross the map from a name to a shape it does not sit on, which the boards never show,
 * so the label is dropped instead and waits for a zoom.
 *
 * Everything here is in **screen pixels relative to the centroids**, so the answer is
 * translation-invariant: a pan moves every centroid by the same vector and changes no overlap,
 * and the caller re-places only when the zoom bucket or the served set changes.
 *
 * A label that fits nowhere within its ring of tries is not placed: it waits for a zoom in, as
 * the design says, rather than being drawn over its neighbours.
 */

export type LabelCandidate = {
  id: bigint;
  /** Anchor, in pixels. */
  x: number;
  y: number;
  /** The label's box, in pixels. */
  width: number;
  height: number;
  /** Larger wins the ground — the masked count, in practice. */
  priority: number;
};

export type PlacedLabel = {id: bigint; dx: number; dy: number; leader: boolean};

/** How far a label may sit from its centroid, in pixels — a nudge with a short leader, never a line across the map. */
export const MAX_DISPLACEMENT = 40;

/** The spatial hash's cell, in pixels: a few labels a cell, so an overlap check reads a few. */
const CELL = 64;
/** Space kept between two labels. */
const GAP = 2;

type Rect = {x0: number; y0: number; x1: number; y1: number};

function overlaps(a: Rect, b: Rect): boolean {
  return a.x0 < b.x1 + GAP && b.x0 < a.x1 + GAP && a.y0 < b.y1 + GAP && b.y0 < a.y1 + GAP;
}

class SpatialHash {
  private cells = new Map<string, Rect[]>();

  private keysOf(r: Rect): string[] {
    const keys: string[] = [];
    for (let cx = Math.floor(r.x0 / CELL); cx <= Math.floor(r.x1 / CELL); cx++) {
      for (let cy = Math.floor(r.y0 / CELL); cy <= Math.floor(r.y1 / CELL); cy++) keys.push(`${cx}:${cy}`);
    }
    return keys;
  }

  free(r: Rect): boolean {
    for (const key of this.keysOf(r)) {
      for (const held of this.cells.get(key) ?? []) if (overlaps(held, r)) return false;
    }
    return true;
  }

  take(r: Rect): void {
    for (const key of this.keysOf(r)) (this.cells.get(key) ?? this.cells.set(key, []).get(key)!).push(r);
  }
}

/**
 * The offsets tried, in order: the centroid itself, then a ring one label away, then a wider
 * one — keeping only those within `maxDisplacement` of the centroid, so a wide label's sideways
 * tries (a label's own width away) are never taken.
 */
function offsetsFor(width: number, height: number, maxDisplacement: number): [number, number][] {
  const out: [number, number][] = [[0, 0]];
  for (const ring of [1, 2]) {
    const dx = ring * (width + 6);
    const dy = ring * (height + 6);
    for (const [ux, uy] of [[0, -1], [0, 1], [1, 0], [-1, 0], [1, -1], [-1, -1], [1, 1], [-1, 1]] as [number, number][]) {
      if (Math.hypot(ux * dx, uy * dy) <= maxDisplacement) out.push([ux * dx, uy * dy]);
    }
  }
  return out;
}

/**
 * Place `candidates`, highest priority first; each placed label is centred on `(x + dx, y + dy)`
 * and carries a leader when it moved. A label that fits nowhere within `maxDisplacement` of its
 * centroid is left out.
 */
export function placeLabels(candidates: readonly LabelCandidate[], maxDisplacement = MAX_DISPLACEMENT): PlacedLabel[] {
  const order = [...candidates].sort((a, b) => b.priority - a.priority);
  const hash = new SpatialHash();
  const placed: PlacedLabel[] = [];
  for (const c of order) {
    for (const [dx, dy] of offsetsFor(c.width, c.height, maxDisplacement)) {
      const r: Rect = {x0: c.x + dx - c.width / 2, y0: c.y + dy - c.height / 2, x1: c.x + dx + c.width / 2, y1: c.y + dy + c.height / 2};
      if (!hash.free(r)) continue;
      hash.take(r);
      placed.push({id: c.id, dx, dy, leader: dx !== 0 || dy !== 0});
      break;
    }
  }
  return placed;
}

/**
 * The pixel size of an artifact's name: **its masked count**, on a logarithmic band over the
 * counts the drawn frontier holds.
 *
 * **Level is not encoded, and was for a day.** Stepping the size by depth assumed depth in the
 * tree tracks scale; that holds in a balanced tree and HDBSCAN's condensed tree is not one. On
 * the 2.4M map *image object video* at 29,369 members drew a step larger than *algebras equations
 * spaces* at 380,069 because it sat one level shallower. Size reads as importance and the eye
 * reads the count, so the two encodings were fighting each other. What carries the hierarchy is
 * the frontier rule instead: only the frontier is labelled, a frontier partitions the drawn view,
 * so the counts on screen are directly comparable and the count is the thing worth drawing.
 *
 * The band is **logarithmic** because these counts span three or four orders of magnitude —
 * 380,069 against 176 on one screen. A linear map, and a square-root one, spend the range on the
 * top few and leave everything else on the floor. Its ends are the level ladder's ends,
 * {@link LABEL_SIZE_MIN} to {@link LABEL_SIZE_MAX}, so nothing else about the map's weight moves.
 *
 * A frontier of one artifact, and one whose counts are all equal, have no range to divide by:
 * the name takes the top of the band. The largest count on screen draws at the largest size, and
 * where every count is the largest that holds of all of them.
 */
export const LABEL_SIZE_MIN = 12.5;
export const LABEL_SIZE_MAX = 24;

export function labelSize(count: number, smallest: number, largest: number): number {
  const c = Math.max(1, count);
  const lo = Math.max(1, Math.min(smallest, c));
  const hi = Math.max(lo, largest, c);
  const span = Math.log(hi / lo);
  if (span <= 0) return LABEL_SIZE_MAX;
  return LABEL_SIZE_MIN + (LABEL_SIZE_MAX - LABEL_SIZE_MIN) * (Math.log(c / lo) / span);
}

/**
 * How many characters a line of a name may hold before it wraps, and how many lines a name may
 * take. A name at these sizes ran to three hundred pixels on one line over the map's own marks;
 * two or three short lines read at a glance and pack far better into the spatial hash, which
 * sees the wrapped box.
 */
export const MAX_LABEL_LINE_CHARS = 14;
export const MAX_LABEL_LINES = 3;
/** The line box as a multiple of the font size, for the wrapped block's height. */
export const LABEL_LINE_HEIGHT = 1.15;

/**
 * A name as up to {@link MAX_LABEL_LINES} lines of at most `maxChars` each — greedy over words,
 * never hyphenating, so a word longer than the limit takes a line of its own. What will not fit
 * is elided onto the last line, because a name cut off without a mark reads as a different name.
 */
export function wrapLabel(text: string, maxChars = MAX_LABEL_LINE_CHARS, maxLines = MAX_LABEL_LINES): string[] {
  const words = text.split(/\s+/).filter((w) => w.length > 0);
  if (words.length === 0) return [text];
  const lines: string[] = [];
  let line = '';
  for (const word of words) {
    if (line.length === 0) line = word;
    else if (line.length + 1 + word.length <= maxChars) line += ` ${word}`;
    else if (lines.length + 1 < maxLines) {
      lines.push(line);
      line = word;
    } else {
      // The last line takes what is left, marked as elided rather than silently truncated.
      line = `${line} ${word}`;
      if (line.length > maxChars + 2) return [...lines, `${line.slice(0, maxChars + 1).trimEnd()}…`];
    }
  }
  lines.push(line);
  return lines;
}
