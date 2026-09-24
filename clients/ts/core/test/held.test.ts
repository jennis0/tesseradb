import {describe, expect, it, vi} from 'vitest';
import {TesseraError} from '../src/client.js';
import {HeldRecords, HeldShapes} from '../src/held.js';
import type {Shape} from '../src/types.js';
import {settle} from './support.js';

describe('shapes are fetched by identifier', () => {
  /**
   * On a pointer move `need` is called every frame, so a second request for a shape already held,
   * or already in flight, is what the part exists to prevent.
   */
  function shapesOf(shape: Shape | null) {
    const fetch = vi.fn(async () => shape);
    let published: ReadonlyMap<bigint, Shape> = new Map();
    const part = new HeldShapes(fetch, () => null, (held) => (published = held));
    return {part, fetch, published: () => published};
  }

  it('asks once per artifact and publishes the parts it gets back', async () => {
    const parts: Shape = [[[[0, 0], [10, 0], [10, 10]]]];
    const {part, fetch, published} = shapesOf(parts);
    part.need(5n);
    part.need(5n);
    await settle();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(published().get(5n)).toEqual(parts);
    part.need(5n);
    await settle();
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('holds nothing for an artifact whose layer draws no shape, and does not ask again', async () => {
    const {part, fetch, published} = shapesOf(null);
    part.need(9n);
    await settle();
    expect(published().has(9n)).toBe(false);
    part.need(9n);
    await settle();
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});

describe('the record a hover names', () => {
  it('asks for a record once, shares a request in flight, holds a refusal as none, and asks again after a forget', async () => {
    const fetch = vi.fn(async (id: bigint) => {
      if (id === 404n) throw new TesseraError(404, 'not-found', 'no such item');
      return {title: `paper ${id}`};
    });
    const part = new HeldRecords(fetch);

    const [first, second] = await Promise.all([part.describe(7n), part.describe(7n)]);
    expect(first).toEqual({title: 'paper 7'});
    expect(second).toEqual({title: 'paper 7'});
    expect(await part.describe(7n)).toEqual({title: 'paper 7'});
    expect(fetch).toHaveBeenCalledTimes(1);

    expect(await part.describe(404n)).toBeNull();
    expect(await part.describe(404n)).toBeNull();
    expect(fetch).toHaveBeenCalledTimes(2);

    part.forget();
    await part.describe(7n);
    expect(fetch).toHaveBeenCalledTimes(3);
  });
});
