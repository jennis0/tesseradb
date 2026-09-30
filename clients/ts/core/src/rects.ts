/**
 * Rectangles of tiles, and their subtraction.
 *
 * Coverage is a set of rectangles because a viewport is a rectangle and a pan's new ground is one
 * rectangle less another, at most four pieces. Enumerating tiles costs time in proportion to the
 * viewport; subtracting rectangles costs time in proportion to a handful of rectangles. A covered
 * rectangle records that the region was asked for and anything not sent is empty, in four integers.
 *
 * Everything is inclusive integer tile indices at one depth, so containment is exact.
 */

/** An inclusive rectangle in tile-index space at one depth. `x1 >= x0`, `y1 >= y0`. @internal */
export type TileRect = {x0: number; y0: number; x1: number; y1: number};

/**
 * A rectangle the client has asked for and absorbed the answer to. `capUsed` is `min(k, k_max_marks)`
 * when it was fetched; the coverage holds only for a `k` no larger, since a larger `k` can serve
 * more points from the same tiles.
 *
 * @internal
 */
export type Coverage = {rect: TileRect; depth: number; contentKey: string; capUsed: number};

/** @internal */
export function rectArea(r: TileRect): number {
  return (r.x1 - r.x0 + 1) * (r.y1 - r.y0 + 1);
}

/** @internal */
export function rectContains(outer: TileRect, inner: TileRect): boolean {
  return (
    inner.x0 >= outer.x0 && inner.y0 >= outer.y0 && inner.x1 <= outer.x1 && inner.y1 <= outer.y1
  );
}

/** @internal */
export function rectContainsTile(r: TileRect, x: number, y: number): boolean {
  return x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1;
}

/** @internal */
export function rectsIntersect(a: TileRect, b: TileRect): boolean {
  return a.x0 <= b.x1 && a.x1 >= b.x0 && a.y0 <= b.y1 && a.y1 >= b.y0;
}

/** The overlap, or null when they do not touch. @internal */
export function rectIntersection(a: TileRect, b: TileRect): TileRect | null {
  if (!rectsIntersect(a, b)) return null;
  return {
    x0: Math.max(a.x0, b.x0),
    y0: Math.max(a.y0, b.y0),
    x1: Math.min(a.x1, b.x1),
    y1: Math.min(a.y1, b.y1)
  };
}

/**
 * `want` minus `hole`, as up to four disjoint rectangles: above, below, then left and right of what
 * remains. Overlapping pieces would be requested and absorbed twice.
 *
 * @internal
 */
export function rectSubtract(want: TileRect, hole: TileRect): TileRect[] {
  const overlap = rectIntersection(want, hole);
  if (!overlap) return [want];
  if (rectContains(hole, want)) return [];

  const out: TileRect[] = [];
  if (overlap.y0 > want.y0) out.push({x0: want.x0, y0: want.y0, x1: want.x1, y1: overlap.y0 - 1});
  if (overlap.y1 < want.y1) out.push({x0: want.x0, y0: overlap.y1 + 1, x1: want.x1, y1: want.y1});
  if (overlap.x0 > want.x0) out.push({x0: want.x0, y0: overlap.y0, x1: overlap.x0 - 1, y1: overlap.y1});
  if (overlap.x1 < want.x1) out.push({x0: overlap.x1 + 1, y0: overlap.y0, x1: want.x1, y1: overlap.y1});
  return out;
}

/**
 * `want` minus every hole, as disjoint rectangles. Past `maxPieces` it stops subtracting and returns
 * what it has, a superset of the new ground: one slightly large request is cheaper than many small
 * ones, and asking again for a held tile costs the server almost nothing.
 *
 * @internal
 */
export function rectSubtractAll(want: TileRect, holes: TileRect[], maxPieces = 8): TileRect[] {
  let pieces = [want];
  for (const hole of holes) {
    if (pieces.length === 0) return [];
    const next: TileRect[] = [];
    for (const piece of pieces) next.push(...rectSubtract(piece, hole));
    if (next.length > maxPieces) return pieces;
    pieces = next;
  }
  return pieces;
}

/**
 * The union of two rectangles where that union is a rectangle: one contains the other, or they
 * share rows or columns and touch or overlap. Otherwise `null`; a bounding box would claim ground
 * neither covered.
 */
export function rectFuse(a: TileRect, b: TileRect): TileRect | null {
  if (rectContains(a, b)) return a;
  if (rectContains(b, a)) return b;
  if (a.y0 === b.y0 && a.y1 === b.y1 && a.x0 <= b.x1 + 1 && b.x0 <= a.x1 + 1) {
    return {x0: Math.min(a.x0, b.x0), y0: a.y0, x1: Math.max(a.x1, b.x1), y1: a.y1};
  }
  if (a.x0 === b.x0 && a.x1 === b.x1 && a.y0 <= b.y1 + 1 && b.y0 <= a.y1 + 1) {
    return {x0: a.x0, y0: Math.min(a.y0, b.y0), x1: a.x1, y1: Math.max(a.y1, b.y1)};
  }
  return null;
}

/**
 * Adds a rectangle to a coverage list, fusing where the union is exactly a rectangle, repeated until
 * nothing more fuses. Fusion keeps a sequence of pans to one rectangle, so each pan subtracts to one
 * request.
 *
 * @internal
 */
export function coverageAdd(list: Coverage[], added: Coverage): Coverage[] {
  const others: Coverage[] = [];
  let merged = added;
  let fusedSomething = true;

  const sameKey = (c: Coverage) =>
    c.depth === merged.depth && c.contentKey === merged.contentKey && c.capUsed === merged.capUsed;

  for (const c of list) if (!sameKey(c)) others.push(c);
  let candidates = list.filter(sameKey);

  while (fusedSomething) {
    fusedSomething = false;
    const rest: Coverage[] = [];
    for (const c of candidates) {
      const union = rectFuse(merged.rect, c.rect);
      if (union) {
        merged = {...merged, rect: union};
        fusedSomething = true;
      } else {
        rest.push(c);
      }
    }
    candidates = rest;
  }

  return [...others, ...candidates, merged];
}

/** Coverage entries usable at this depth and content key. @internal */
export function coverageAt(
  list: Coverage[],
  depth: number,
  contentKey: string,
  k: number
): TileRect[] {
  const out: TileRect[] = [];
  for (const c of list) {
    if (c.depth === depth && c.contentKey === contentKey && c.capUsed >= k) out.push(c.rect);
  }
  return out;
}
