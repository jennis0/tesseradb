/**
 * Rectangles of tiles, and the subtraction that makes planning O(1) in tile count.
 *
 * **Why the client's coverage is a rectangle set and not a tile set.** A viewport is a rectangle;
 * a pan produces a rectangle overlapping the last one; the novel part is therefore
 * rectangle-minus-rectangle, which is at most four rectangles. Enumerating every tile in the
 * viewport to rediscover that shape costs O(tiles) — measured at 181 ms to enumerate and 56 ms to
 * diff a 262 144-tile ring, on the thread that also draws. Subtracting rectangles costs O(rects),
 * which is single digits.
 *
 * It also removes a leak. Recording emptiness per tile meant one map entry for every empty tile
 * ever looked at — ~97% of a viewport, unbounded, and not counted against the cache's byte budget.
 * A covered rectangle asserts the same thing for its whole area in four integers: *we asked here
 * and absorbed the answer, so anything we were not sent is empty*.
 *
 * **Integer tile indices, never world coordinates.** Containment and subtraction have to be exact
 * or a rectangle "covers" a region it does not; floats drift and would make that test lie.
 * Everything here is inclusive-bounds integer arithmetic at one depth.
 */

/** An inclusive rectangle in tile-index space at one depth. `x1 >= x0`, `y1 >= y0`. */
export type TileRect = {x0: number; y0: number; x1: number; y1: number};

/**
 * A rectangle the client has asked for and absorbed the answer to.
 *
 * `capUsed` is `min(k, k_max_marks)` in force when it was fetched. Coverage is only reusable at a
 * `k` no larger than that: where the cap was the binding clause of the selection rule, a larger `k`
 * yields more points for the same tiles, so the region is no longer fully held. The per-*tile* test
 * could be sharper — a tile whose served count came in under the cap is unaffected by raising it —
 * but that distinction cannot be made for a rectangle as a whole, and being conservative here costs
 * bytes rather than correctness.
 */
export type Coverage = {rect: TileRect; depth: number; contentKey: string; capUsed: number};

export function rectArea(r: TileRect): number {
  return (r.x1 - r.x0 + 1) * (r.y1 - r.y0 + 1);
}

export function rectContains(outer: TileRect, inner: TileRect): boolean {
  return (
    inner.x0 >= outer.x0 && inner.y0 >= outer.y0 && inner.x1 <= outer.x1 && inner.y1 <= outer.y1
  );
}

export function rectContainsTile(r: TileRect, x: number, y: number): boolean {
  return x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1;
}

export function rectsIntersect(a: TileRect, b: TileRect): boolean {
  return a.x0 <= b.x1 && a.x1 >= b.x0 && a.y0 <= b.y1 && a.y1 >= b.y0;
}

/** The overlap, or null when they do not touch. */
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
 * `want` minus `hole`, as up to four disjoint rectangles.
 *
 * Cut in bands — above, below, then the left and right of what remains — so the pieces never
 * overlap. Overlapping pieces would be re-requested twice and, worse, absorbed twice.
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
 * `want` minus every hole, as a disjoint set.
 *
 * **Bounded rather than exact, and the bound is the point.** Subtracting *n* holes can in principle
 * fragment into O(n) pieces; if it does, the caller is better served by one slightly-too-large
 * request than by a hundred small ones — the per-request floor is ~170 µs and a tile it already
 * holds costs the server essentially nothing to be asked for again. So past `maxPieces` this stops
 * subtracting and returns what it has, which is a superset of the novel region: more bytes, never
 * a hole.
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
 * The union of two rectangles, when that union is itself exactly a rectangle.
 *
 * True in three cases: one contains the other, or they span the same rows and touch or overlap
 * horizontally, or they span the same columns and touch or overlap vertically. Anything else has a
 * union that is L-shaped or disjoint, and returning a bounding box for it would claim ground
 * neither rectangle covered.
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
 * Absorb `rect` into a coverage list, fusing where that is exact.
 *
 * **Fusion is what keeps a pan sequence to one rectangle**, and without it the structure defeats
 * itself: each pan adds a strip, subtraction then shatters the next request against a chain of
 * overlapping pieces, and one request per pan becomes three or four. Measured that way round
 * before this existed.
 *
 * Only *exactly* rectangular unions are fused — see {@link rectFuse}. A bounding box over an
 * L-shaped union would claim emptiness for tiles nobody asked about, which is the one failure this
 * structure must not have. Re-run to a fixed point, because fusing two can make a third fusable.
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

/** Coverage entries usable at this depth and content key. */
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

/** The union of `rect`'s tiles, as `(x, y)` pairs. Only for callers that genuinely need each one. */
export function* rectTiles(r: TileRect): Generator<{x: number; y: number}> {
  for (let y = r.y0; y <= r.y1; y++) for (let x = r.x0; x <= r.x1; x++) yield {x, y};
}
