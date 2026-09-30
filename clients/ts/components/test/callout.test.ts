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

  it('hides while its point is off the map', () => {
    expect(placeCallout([-1, 300], CARD, MAP)).toBeNull();
    expect(placeCallout([300, 901], CARD, MAP)).toBeNull();
    expect(placeCallout([300, 900], CARD, MAP)).not.toBeNull();
  });
});
