/**
 * Label placement (design §5.10): names and counts at each artifact's centroid, placed by
 * priority into a spatial hash — O(K) for K labels, so 10⁴ labels cost what a few hundred
 * do — with a leader line when a label had to move off its centroid to fit.
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

/** The offsets tried, in order: the centroid itself, then a ring one label away, then a wider one. */
function offsetsFor(width: number, height: number): [number, number][] {
  const out: [number, number][] = [[0, 0]];
  for (const ring of [1, 2]) {
    const dx = ring * (width + 6);
    const dy = ring * (height + 6);
    for (const [ux, uy] of [[0, -1], [0, 1], [1, 0], [-1, 0], [1, -1], [-1, -1], [1, 1], [-1, 1]] as [number, number][]) {
      out.push([ux * dx, uy * dy]);
    }
  }
  return out;
}

/** Place `candidates`, highest priority first; each placed label is centred on `(x + dx, y + dy)`. */
export function placeLabels(candidates: readonly LabelCandidate[]): PlacedLabel[] {
  const order = [...candidates].sort((a, b) => b.priority - a.priority);
  const hash = new SpatialHash();
  const placed: PlacedLabel[] = [];
  for (const c of order) {
    for (const [dx, dy] of offsetsFor(c.width, c.height)) {
      const r: Rect = {x0: c.x + dx - c.width / 2, y0: c.y + dy - c.height / 2, x1: c.x + dx + c.width / 2, y1: c.y + dy + c.height / 2};
      if (!hash.free(r)) continue;
      hash.take(r);
      placed.push({id: c.id, dx, dy, leader: dx !== 0 || dy !== 0});
      break;
    }
  }
  return placed;
}

/** The pixel size of an artifact's name, by its masked count within a narrow band (§5.10). */
export function labelSize(count: number, largest: number): number {
  return 10 + 5 * Math.sqrt(Math.max(0, count) / Math.max(1, largest));
}
