import {describe, expect, it} from 'vitest';
import type {Band, ScalarColumn} from '@tesseradb/client';
import {MarkSlab} from '../src/slab.js';
import {UNIFORM, UNMAPPED, colourOfRank, type Encoding} from '../src/colour.js';

const UNIFORM_ENCODING: Encoding = {kind: 'uniform'};

/**
 * A band of `n` marks whose ids and positions are derived from `tag`, so a slot can be checked to
 * hold the band it claims rather than merely the right number of marks.
 */
function band(tag: number, n: number, identityKey = 'ik', depth = 2): Band {
  const scalars: Record<string, ScalarColumn> = {
    c: {arrowType: 'u32', values: Uint32Array.from({length: n}, () => tag)}
  };
  return {
    depth,
    prefix: BigInt(tag),
    x: tag,
    y: 0,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(tag * 1000 + i)),
    positions: Float32Array.from({length: n * 2}, (_, i) => tag * 100 + i),
    scalars,
    served: n,
    capUsed: 500,
    visible: BigInt(n * 3),
    matched: BigInt(n * 3),
    heldBelow: BigInt(tag * 1000 + n),
    identityKey,
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0
  };
}

/** The marks the slab would draw, read back out of its buffers. */
function drawnIds(slab: MarkSlab, draw: {ids: BigUint64Array; length: number}): bigint[] {
  expect(draw.ids.length).toBe(draw.length);
  expect(slab.drawn).toBe(draw.length);
  return [...draw.ids];
}

describe('MarkSlab', () => {
  it('writes a band once and keeps the same buffers when nothing changes', () => {
    const slab = new MarkSlab();
    const a = band(1, 3);
    const first = slab.sync([a], 2, UNIFORM_ENCODING, null);
    const second = slab.sync([a], 2, UNIFORM_ENCODING, null);

    // Identical object, so deck.gl compares references, finds them equal and uploads nothing. This
    // is the whole mechanism — a defensive copy here would silently undo it.
    expect(second).toBe(first);
    expect(drawnIds(slab, first)).toEqual([1000n, 1001n, 1002n]);
  });

  it('appends an arriving band without rewriting the resident ones', () => {
    const slab = new MarkSlab();
    const a = band(1, 3);
    const first = slab.sync([a], 2, UNIFORM_ENCODING, null);
    const positionsBefore = [...first.positions];

    const next = slab.sync([a, band(2, 2)], 2, UNIFORM_ENCODING, null);

    expect(next).not.toBe(first);
    expect(drawnIds(slab, next)).toEqual([1000n, 1001n, 1002n, 2000n, 2001n]);
    // The first band's marks sit exactly where they did: its slot is stable, which is what makes
    // "write once" true rather than merely "copy in a different order".
    expect([...next.positions.subarray(0, positionsBefore.length)]).toEqual(positionsBefore);
  });

  it('retains a band that has left the frame, so panning back draws it with no work', () => {
    const slab = new MarkSlab();
    const a = band(1, 3);
    const b = band(2, 2);
    slab.sync([a, b], 2, UNIFORM_ENCODING, null);

    // b leaves the render rectangle. It keeps its slot and keeps being drawn — off screen, since
    // the frame's exact set is by construction everything at this depth inside the rectangle.
    const away = slab.sync([a], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, away)).toEqual([1000n, 1001n, 1002n, 2000n, 2001n]);
    expect(slab.departed).toBe(2);
    expect(slab.holds(b)).toBe(true);

    // And coming back costs nothing at all.
    expect(slab.sync([a, b], 2, UNIFORM_ENCODING, null)).toBe(away);
  });

  it('compacts to the frame once residency runs too far past it', () => {
    const slab = new MarkSlab();
    const wanted = band(1, 200_000);
    slab.sync([wanted, band(2, 200_000), band(3, 200_000)], 2, UNIFORM_ENCODING, null);
    expect(slab.residentBands).toBe(3);

    // 600k resident against a 200k frame is past both the floor and the slack multiple.
    const after = slab.sync([wanted], 2, UNIFORM_ENCODING, null);
    expect(slab.residentBands).toBe(1);
    expect(after.length).toBe(200_000);
    expect(after.ids[0]).toBe(1000n);
    expect(slab.departed).toBe(0);
  });

  it('keeps what is resident across a frame with nothing in it', () => {
    const slab = new MarkSlab();
    const a = band(1, 3);
    const held = slab.sync([a], 2, UNIFORM_ENCODING, null);

    // Panning onto ground this principal cannot see is not evidence that anything resident has
    // expired. Wiping here would make coming back cost a full rewrite.
    expect(slab.sync([], 2, UNIFORM_ENCODING, null)).toBe(held);
    expect(slab.holds(a)).toBe(true);
    expect(slab.drawn).toBe(3);
  });

  it('voids every slot when the identity key changes', () => {
    const slab = new MarkSlab();
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    // A different principal is a different visible set, so a band from the old one may not be
    // drawn at all. A reset, never a merge.
    const next = slab.sync([band(2, 2, 'other')], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, next)).toEqual([2000n, 2001n]);
  });

  it('retains the previous depth across a flip, so flipping back is free', () => {
    const slab = new MarkSlab();
    const shallow = band(1, 3);
    const first = slab.sync([shallow], 2, UNIFORM_ENCODING, null);

    // A wheel notch: the new depth draws, the old one keeps its buffers unseen.
    const deeper = slab.sync([band(2, 2, 'ik', 3)], 3, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, deeper)).toEqual([2000n, 2001n]);
    expect(slab.holds(shallow)).toBe(false); // holds() answers for the active partition

    // Flipping back returns the identical draw object — nothing rebuilt, nothing re-uploaded.
    expect(slab.sync([shallow], 2, UNIFORM_ENCODING, null)).toBe(first);
    expect(slab.holds(shallow)).toBe(true);
  });

  it('renders every retained partition, only the active one visible', () => {
    const slab = new MarkSlab();
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    slab.sync([band(2, 2, 'ik', 3)], 3, UNIFORM_ENCODING, null);
    const layers = slab.layers();
    expect(layers).toHaveLength(2);
    expect(layers.filter((l) => l.active)).toHaveLength(1);
    expect(layers.find((l) => l.active)!.draw.length).toBe(2);
    expect(layers.find((l) => !l.active)!.draw.length).toBe(3);
    // Slots are stable: the same partition keeps the same slot across a swap.
    const before = layers.map((l) => [l.slot, l.draw.length]);
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    const after = slab.layers().map((l) => [l.slot, l.draw.length]);
    expect(after.map((x) => x[0])).toEqual(before.map((x) => x[0]));
  });

  it('evicts the least-recently-used partition when past its slots', () => {
    const slab = new MarkSlab(2);
    const shallow = band(1, 3);
    slab.sync([shallow], 2, UNIFORM_ENCODING, null); // depth 2
    slab.sync([band(2, 2, 'ik', 3)], 3, UNIFORM_ENCODING, null); // depth 3
    slab.sync([band(3, 1, 'ik', 4)], 4, UNIFORM_ENCODING, null); // depth 4 — depth 2 goes
    // Coming back to depth 2 is now a rebuild, not a reuse.
    const back = slab.sync([shallow], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, back)).toEqual([1000n, 1001n, 1002n]);
    expect(slab.layers()).toHaveLength(2);
  });

  it('frees least-recently-used partitions when past the mark budget', () => {
    // Three depths of three marks each against a budget of five: the two oldest inactive
    // partitions must go, whole, and the active one must survive.
    const slab = new MarkSlab(6, 5);
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    slab.sync([band(2, 3, 'ik', 3)], 3, UNIFORM_ENCODING, null);
    slab.sync([band(3, 3, 'ik', 4)], 4, UNIFORM_ENCODING, null);
    expect(slab.residentMarks).toBeLessThanOrEqual(5);
    expect(slab.drawn).toBe(3); // the active partition is never evicted
  });

  it('an identity change voids every partition, not only the active one', () => {
    const slab = new MarkSlab();
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    slab.sync([band(2, 2, 'ik', 3)], 3, UNIFORM_ENCODING, null);
    const next = slab.sync([band(4, 1, 'other')], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, next)).toEqual([4000n]);
    expect(slab.layers()).toHaveLength(1);
  });

  it('colours a band as it is written, from its own values', () => {
    const slab = new MarkSlab();
    const encoding: Encoding = {kind: 'category', column: 'c', rankOfCode: {1: 0, 2: 1}};
    const draw = slab.sync([band(1, 2), band(2, 1)], 2, encoding, 'c');

    expect([...draw.colours.subarray(0, 4)]).toEqual([...colourOfRank(0)]);
    expect([...draw.colours.subarray(8, 12)]).toEqual([...colourOfRank(1)]);
  });

  it('recolours every resident band, not only the frame, when the encoding changes', () => {
    const slab = new MarkSlab();
    const away = band(2, 1);
    slab.sync([band(1, 1), away], 2, UNIFORM_ENCODING, null);

    // `away` has left the frame but is still drawn: leaving it on the old encoding would put two
    // colour scales on one map.
    const draw = slab.sync([band(1, 1)], 2, {kind: 'unmapped'}, null);
    expect([...draw.colours.subarray(0, 4)]).toEqual([...UNMAPPED]);
    expect([...draw.colours.subarray(4, 8)]).toEqual([...UNMAPPED]);
  });

  it('does not recolour when a sticky domain has merely been re-supplied', () => {
    const slab = new MarkSlab();
    const a = band(1, 1);
    const first = slab.sync([a], 2, {kind: 'numeric', column: 'c', domain: {min: 0, max: 9}}, 'c');
    const second = slab.sync([a], 2, {kind: 'numeric', column: 'c', domain: {min: 0, max: 9}}, 'c');
    // Same colouring, freshly constructed object — comparing by reference would re-upload the
    // whole colour buffer on every response.
    expect(second).toBe(first);
  });

  it('gives every mark a colour even where the band lacks the column', () => {
    const slab = new MarkSlab();
    const draw = slab.sync([band(1, 2)], 2, {kind: 'category', column: 'absent', rankOfCode: {}}, 'absent');
    expect([...draw.colours.subarray(0, 8)]).toEqual([...UNMAPPED, ...UNMAPPED]);
  });

  it('grows without losing what is resident', () => {
    const slab = new MarkSlab();
    const bands: Band[] = [];
    for (let i = 1; i <= 40; i++) {
      bands.push(band(i, 2000));
      slab.sync(bands, 2, UNIFORM_ENCODING, null);
    }
    const draw = slab.sync(bands, 2, UNIFORM_ENCODING, null);
    expect(draw.length).toBe(80_000);
    expect(draw.ids[0]).toBe(1000n);
    expect(draw.ids[79_999]).toBe(BigInt(40 * 1000 + 1999));
    // The last mark written before the final growth step still carries its colour, so the copy
    // that grows the buffers carries all three arrays and not only the ones under test above.
    expect([...draw.colours.subarray(79_999 * 4, 80_000 * 4)]).toEqual([...UNIFORM]);
  });

  it('replaces a refetched tile in place rather than drawing it twice', () => {
    const slab = new MarkSlab();
    const first = band(1, 3);
    slab.sync([first, band(2, 2)], 2, UNIFORM_ENCODING, null);

    // The same tile, refetched: a new band object with the same served set. Its slot is reused, so
    // the tile is drawn once — keying slots by band object would have drawn it twice.
    const refetched = {...first, ids: BigUint64Array.from([7n, 8n, 9n])};
    const draw = slab.sync([refetched, band(2, 2)], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, draw)).toEqual([7n, 8n, 9n, 2000n, 2001n]);
    expect(slab.residentBands).toBe(2);
  });

  it('rehouses a refetched tile whose served set has grown', () => {
    const slab = new MarkSlab();
    const first = band(1, 2);
    slab.sync([first], 2, UNIFORM_ENCODING, null);

    // A larger band cannot reuse the slot, so the slab compacts rather than leaving the old marks
    // in the draw range — they are a subset of the new ones and would be drawn twice.
    const bigger = {...first, ids: BigUint64Array.from([7n, 8n, 9n]), served: 3};
    const draw = slab.sync([bigger], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, draw)).toEqual([7n, 8n, 9n]);
    expect(slab.holds(bigger)).toBe(true);
    expect(slab.residentBands).toBe(1);
  });

  it('draws nothing after a clear', () => {
    const slab = new MarkSlab();
    slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    slab.clear();
    expect(slab.drawn).toBe(0);
    // And a band written before the clear no longer counts as resident.
    const draw = slab.sync([band(1, 3)], 2, UNIFORM_ENCODING, null);
    expect(drawnIds(slab, draw)).toEqual([1000n, 1001n, 1002n]);
  });

  it('colours uniformly by default', () => {
    const slab = new MarkSlab();
    const draw = slab.sync([band(1, 1)], 2, UNIFORM_ENCODING, null);
    expect([...draw.colours.subarray(0, 4)]).toEqual([...UNIFORM]);
  });
});
