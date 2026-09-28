/**
 * Label placement: names and counts at each artifact's centroid, placed by priority into a
 * spatial hash in O(K) for K labels, with a leader line when a label moved off its centroid.
 *
 * A label moves at most `MAX_DISPLACEMENT` pixels. One that fits nowhere within that is dropped
 * until a zoom in makes room, so a leader never crosses the map to a shape the name does not sit on.
 *
 * Everything is in screen pixels relative to the centroids, so a pan changes no overlap and the
 * caller re-places only when the zoom bucket or the served set changes.
 */

export type LabelCandidate = {
  id: bigint;
  /** Anchor, in pixels. */
  x: number;
  y: number;
  /** The label's box, in pixels. */
  width: number;
  height: number;
  /** Larger is placed first; the caller passes the masked count. */
  priority: number;
};

export type PlacedLabel = {id: bigint; dx: number; dy: number; leader: boolean};

/** How far a label may sit from its centroid, in pixels. */
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
 * The offsets tried, in order: the centroid, a ring one label away, then a wider ring, keeping
 * only those within `maxDisplacement`. A wide label's sideways tries fall outside it.
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

/** The ends of the label size band, in pixels. */
export const LABEL_SIZE_MIN = 13;
export const LABEL_SIZE_MAX = 16;

/**
 * The pixel size of an artifact's name, from its masked count on a logarithmic band between the
 * smallest and largest counts on the drawn frontier. Depth in the tree is not encoded: a condensed
 * tree is unbalanced, so depth does not track size. Only the frontier is labelled and it
 * partitions the view, so the counts on screen are comparable.
 *
 * The band is logarithmic because the counts on one screen span three or four orders of
 * magnitude. With no range (one artifact, or equal counts) every name takes {@link LABEL_SIZE_MAX}.
 */
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
 * take. Short lines pack better into the spatial hash, which sees the wrapped box.
 */
export const MAX_LABEL_LINE_CHARS = 14;
export const MAX_LABEL_LINES = 3;
/** The line box as a multiple of the font size, for the wrapped block's height. */
export const LABEL_LINE_HEIGHT = 1.15;

/**
 * A name as up to {@link MAX_LABEL_LINES} lines of at most `maxChars` each, greedy over words and
 * without hyphenation, so a longer word takes a line of its own. What does not fit is cut from
 * the last line and marked with an ellipsis, so a shortened name does not read as another name.
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
      line = `${line} ${word}`;
      if (line.length > maxChars + 2) return [...lines, `${line.slice(0, maxChars + 1).trimEnd()}…`];
    }
  }
  lines.push(line);
  return lines;
}
