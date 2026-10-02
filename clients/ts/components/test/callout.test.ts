import {describe, expect, it} from 'vitest';
import {CALLOUT_GAP, placeCallout} from '../src/callout.js';

/** Where the card beside a point goes: the side, the edges and the cards it keeps clear of. */

const MAP = {width: 1440, height: 900};
const CARD = {width: 300, height: 200};

describe('placing a card beside its point', () => {
  it('takes the side with most room, centred on the point along it', () => {
    const at = placeCallout([400, 450], CARD, MAP)!;
    expect(at.side).toBe('right');
    expect(at.left).toBe(400 + CALLOUT_GAP);
    expect(at.top).toBe(450 - CARD.height / 2);
    // The leader runs from the point to the card's near edge.
    expect(at.leader).toEqual({x1: 400, y1: 450, x2: 400 + CALLOUT_GAP, y2: 450});
  });

  it('flips to the left near the right edge, and below near the top', () => {
    expect(placeCallout([1300, 450], CARD, MAP)!.side).toBe('left');
    const narrow = {width: 500, height: 900};
    expect(placeCallout([250, 40], CARD, narrow)!.side).toBe('below');
    expect(placeCallout([250, 860], CARD, narrow)!.side).toBe('above');
  });

  it('slides along its side to stay inside the map', () => {
    const at = placeCallout([400, 20], CARD, MAP)!;
    expect(at.side).toBe('right');
    expect(at.top).toBe(8);
    expect(at.leader.y2).toBe(20);
  });

  it('never covers a card it is told to keep clear of, where another side has room', () => {
    const rightCard = {left: 1104, top: 16, width: 320, height: 700};
    const leftCard = {left: 16, top: 16, width: 340, height: 868};
    // Most room is to the right, but the right card is there.
    const at = placeCallout([760, 300], CARD, MAP, [rightCard, leftCard])!;
    expect(at.side).toBe('left');
    const overlaps = (r: {left: number; top: number; width: number; height: number}) =>
      at.left < r.left + r.width && r.left < at.left + CARD.width && at.top < r.top + r.height && r.top < at.top + CARD.height;
    expect(overlaps(rightCard)).toBe(false);
    expect(overlaps(leftCard)).toBe(false);
  });

  it('slides along its side, still level with the point, to clear a card in the way', () => {
    const map = {width: 900, height: 560};
    const leftCard = {left: 12, top: 12, width: 300, height: 336};
    const rightCard = {left: 616, top: 12, width: 272, height: 78};
    const at = placeCallout([333, 150], CARD, map, [leftCard, rightCard])!;
    expect(at.side).toBe('right');
    expect(at.top).toBe(rightCard.top + rightCard.height + 8);
    expect(at.top <= 150 && 150 <= at.top + CARD.height).toBe(true);
    expect(at.leader.y2).toBe(150);
  });

  it('keeps its side and its place along it while the camera moves, until the card would leave the map', () => {
    const first = placeCallout([400, 450], CARD, MAP)!;
    expect(first.side).toBe('right');
    // The point moves right: the left side now has more room, but a firm placement stays.
    const moved = placeCallout([900, 400], CARD, MAP, [], undefined, {side: first.side, offset: first.offset, firm: true})!;
    expect(moved.side).toBe('right');
    expect(moved.top - 400).toBe(first.offset);
    // Near the right edge the card would leave the map, so it is placed afresh.
    expect(placeCallout([1300, 400], CARD, MAP, [], undefined, {side: 'right', offset: first.offset, firm: true})!.side).toBe('left');
  });

  it('at rest keeps its side where that is still clear, and moves where a card is in the way', () => {
    const kept = {side: 'left' as const, offset: -100, firm: false};
    expect(placeCallout([900, 450], CARD, MAP, [], undefined, kept)!.side).toBe('left');
    const leftCard = {left: 500, top: 300, width: 400, height: 300};
    expect(placeCallout([900, 450], CARD, MAP, [leftCard], undefined, kept)!.side).toBe('right');
  });

  it('grows down from where it sat when its content grows', () => {
    const first = placeCallout([400, 450], CARD, MAP)!;
    const taller = placeCallout([400, 450], {width: 300, height: 400}, MAP, [], undefined, {side: first.side, offset: first.offset, firm: false})!;
    expect(taller.top).toBe(first.top);
    expect(taller.height).toBe(400);
  });

  it('is shortened to the clear span beside its point where it fits nowhere whole, clear of the cards over the map', () => {
    const map = {width: 900, height: 560};
    const panel = {left: 12, top: 12, width: 300, height: 176};
    const info = {left: 617, top: 12, width: 271, height: 236};
    const tall = {width: 300, height: 420};
    const at = placeCallout([490, 290], tall, map, [panel, info], undefined, null, 180)!;
    expect(at.height).toBeLessThan(tall.height);
    expect(at.height).toBeGreaterThanOrEqual(180);
    const r = {left: at.left, top: at.top, width: 300, height: at.height};
    for (const a of [panel, info]) expect(r.left < a.left + a.width && a.left < r.left + r.width && r.top < a.top + a.height && a.top < r.top + r.height).toBe(false);
    expect(at.top + at.height).toBeLessThanOrEqual(map.height - 8);
  });

  it('hides while its point is off the map', () => {
    expect(placeCallout([-1, 300], CARD, MAP)).toBeNull();
    expect(placeCallout([300, 901], CARD, MAP)).toBeNull();
    expect(placeCallout([300, 900], CARD, MAP)).not.toBeNull();
  });
});
