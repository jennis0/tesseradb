/**
 * Where a card sits beside a point on the map: on the side with most room, clear of the cards the
 * host keeps over the map, joined to the point by a short leader.
 */

/** A rectangle in the map's pixels, from its top-left corner. */
export type Rect = {left: number; top: number; width: number; height: number};

/** The side of the point a card sits on. */
export type Side = 'right' | 'left' | 'below' | 'above';

/**
 * Where a card goes, and the leader from the point to its nearest edge. `height` is the height the
 * card is shown at, which is less than its own where it was shortened to fit; what does not fit
 * scrolls inside it. `offset` is the card's position along its side relative to the point: its
 * top less the point's y beside it, its left less the point's x above or below it.
 */
export type Placement = {left: number; top: number; height: number; side: Side; offset: number; leader: {x1: number; y1: number; x2: number; y2: number}};

/**
 * A placement to keep: its side and its offset along that side. `firm` keeps it as long as the card
 * stays inside the map, over whatever it covers, as while the camera moves; otherwise it is kept
 * only where it is still clear.
 */
export type Kept = {side: Side; offset: number; firm: boolean};

/** The gap between a point and its card, which the leader spans. */
export const CALLOUT_GAP = 14;
/** The least distance a card keeps from the map's edges. */
const EDGE = 8;

const overlaps = (a: Rect, b: Rect) => a.left < b.left + b.width && b.left < a.left + a.width && a.top < b.top + b.height && b.top < a.top + a.height;

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)));

/**
 * Place a card of `card`'s size beside `point` in a map of `map`'s size, clear of every rectangle
 * in `avoid`. `null` while the point is outside the map, where the card hides until it is back.
 *
 * A `kept` placement is taken first, slid along its side to stay inside the map: always where it is
 * `firm` and the card stays inside the map across the side, else only where it meets nothing in
 * `avoid`, if need be shortened as below. Otherwise the sides are tried in the order of the room each leaves once the card is in
 * it, most first; along a side the card is centred on the point and slid to stay inside the map, or
 * slid further, still level with the point, to clear a rectangle in the way. Beside the point, a
 * card that fits nowhere at its own height is shortened, down to `minHeight`, to the clear span
 * level with the point. The first side where the card fits inside the map and meets no rectangle
 * in `avoid` wins. Where none does, the card takes the side with most room, kept inside the map.
 */
export function placeCallout(
  point: readonly [number, number],
  card: {width: number; height: number},
  map: {width: number; height: number},
  avoid: readonly Rect[] = [],
  gap = CALLOUT_GAP,
  kept: Kept | null = null,
  minHeight = card.height
): Placement | null {
  const [px, py] = point;
  if (!(px >= 0 && py >= 0 && px <= map.width && py <= map.height)) return null;
  const w = card.width;
  const h = Math.min(card.height, map.height - 2 * EDGE);
  const across = (centre: number, size: number, extent: number) => clamp(centre - size / 2, EDGE, extent - EDGE - size);
  const sides: {side: Side; room: number; rect: Rect}[] = [
    {side: 'right', room: map.width - px - gap - w, rect: {left: px + gap, top: across(py, h, map.height), width: w, height: h}},
    {side: 'left', room: px - gap - w, rect: {left: px - gap - w, top: across(py, h, map.height), width: w, height: h}},
    {side: 'below', room: map.height - py - gap - h, rect: {left: across(px, w, map.width), top: py + gap, width: w, height: h}},
    {side: 'above', room: py - gap - h, rect: {left: across(px, w, map.width), top: py - gap - h, width: w, height: h}}
  ];
  const inside = (r: Rect) => r.left >= EDGE - 0.5 && r.top >= EDGE - 0.5 && r.left + r.width <= map.width - EDGE + 0.5 && r.top + r.height <= map.height - EDGE + 0.5;
  const clear = (r: Rect) => inside(r) && !avoid.some((a) => overlaps(r, a));
  /**
   * The side's card centred on the point, else slid along the side to just past a card in the way,
   * as long as it still spans the point there so the leader stays short.
   */
  const fitted = (s: (typeof sides)[number]): Rect | null => {
    if (clear(s.rect)) return s.rect;
    const along = s.side === 'right' || s.side === 'left' ? 'top' : 'left';
    const size = along === 'top' ? h : w;
    const at = along === 'top' ? py : px;
    const slides = avoid.flatMap((a) => (along === 'top' ? [a.top + a.height + EDGE, a.top - EDGE - size] : [a.left + a.width + EDGE, a.left - EDGE - size]));
    for (const v of slides.sort((x, y) => Math.abs(x + size / 2 - at) - Math.abs(y + size / 2 - at))) {
      const r = {...s.rect, [along]: v};
      if (v <= at && at <= v + size && clear(r)) return r;
    }
    return null;
  };
  /** Beside the point, the card shortened to the clear span level with it, where that is at least `minHeight`. */
  const shortened = (s: (typeof sides)[number]): Rect | null => {
    if (s.side !== 'right' && s.side !== 'left') return null;
    const {left} = s.rect;
    if (left < EDGE - 0.5 || left + w > map.width - EDGE + 0.5) return null;
    let lo = EDGE;
    let hi = map.height - EDGE;
    for (const a of avoid) {
      if (!(left < a.left + a.width && a.left < left + w)) continue;
      if (a.top + a.height <= py) lo = Math.max(lo, a.top + a.height + EDGE);
      else if (a.top >= py) hi = Math.min(hi, a.top - EDGE);
      else return null;
    }
    const height = Math.min(h, hi - lo);
    if (height < minHeight) return null;
    return {left, top: clamp(py - height / 2, lo, hi - height), width: w, height};
  };
  const keptRect = (k: Kept): Rect | null => {
    const s = sides.find((x) => x.side === k.side)!;
    const r = k.side === 'right' || k.side === 'left' ? {...s.rect, top: clamp(py + k.offset, EDGE, map.height - EDGE - h)} : {...s.rect, left: clamp(px + k.offset, EDGE, map.width - EDGE - w)};
    if (k.firm) return inside(r) ? r : null;
    return clear(r) ? r : shortened(s);
  };
  const ranked = [...sides].sort((a, b) => b.room - a.room);
  const pick = (): {side: Side; rect: Rect} => {
    const held = kept ? keptRect(kept) : null;
    if (kept && held) return {side: kept.side, rect: held};
    for (const s of ranked) {
      const r = fitted(s);
      if (r) return {side: s.side, rect: r};
    }
    for (const s of ranked) {
      const r = shortened(s);
      if (r) return {side: s.side, rect: r};
    }
    return ranked[0]!;
  };
  const chosen = pick();
  const height = chosen.rect.height;
  const rect = {...chosen.rect, left: clamp(chosen.rect.left, EDGE, map.width - EDGE - w), top: clamp(chosen.rect.top, EDGE, map.height - EDGE - height)};
  // The leader runs from the point to the nearest point of the card's edge.
  const x2 = clamp(px, rect.left, rect.left + w);
  const y2 = clamp(py, rect.top, rect.top + height);
  const offset = chosen.side === 'right' || chosen.side === 'left' ? rect.top - py : rect.left - px;
  return {left: rect.left, top: rect.top, height, side: chosen.side, offset, leader: {x1: px, y1: py, x2, y2}};
}
