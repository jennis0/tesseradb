import {describe, expect, it} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import type {LegendProjection, MarksProjection, Meta, ScalarColumn} from '@tesseradb/client';
import {band} from '../../core/test/support.js';
import {TesseraLayer, type TesseraLayerInternalProps} from '../src/layer.js';
import {MarkSlab} from '../src/slab.js';
import {sizeEncodingOf, sizeFraction, valueAtSize, type SizeEncoding} from '../src/size.js';
import {fakeDevice} from './fake-device.js';

const meta = {
  declaredScalars: [
    {name: 'citations', arrowType: 'u32', category: null, render: true},
    {name: 'field', arrowType: 'u16', category: {vocabulary: 'f', kind: 'declared', visibility: 'public'}, render: true},
    {name: 'pages', arrowType: 'u32', category: null, render: false}
  ]
} as unknown as Meta;

function legend(over: Partial<LegendProjection> = {}): LegendProjection {
  return {ranks: {}, domains: {citations: {min: 0, max: 999}}, samples: {citations: {values: [1, 2, 3, 100, 1000], seen: 5}}, missing: {}, categories: {}, categoryErrors: {}, colourBy: null, sizeBy: 'citations', ...over};
}

/** Citation counts as a column; `null` is a point with no value. */
function citations(values: (number | null)[]): ScalarColumn {
  return {arrowType: 'u32', values: Uint32Array.from(values.map((v) => v ?? 0)), present: Uint8Array.from(values.map((v) => (v === null ? 0 : 1)))};
}

const round = (xs: ArrayLike<number>) => Array.from(xs, (x) => Math.round(x * 1000) / 1000);

describe('sizing by a number column', () => {
  it('places a value linearly between the smallest and largest of the range drawn', () => {
    const linear = sizeEncodingOf(meta, legend({domains: {citations: {min: 0, max: 100}}}), 'linear');
    expect(round([0, 25, 50, 100].map((v) => sizeFraction(v, linear)))).toEqual([0, 0.25, 0.5, 1]);
  });

  it('places a count on a log scale that takes zero, so each tenfold step is one step of size', () => {
    const log = sizeEncodingOf(meta, legend(), 'log');
    expect(round([0, 9, 99, 999].map((v) => sizeFraction(v, log)))).toEqual([0, 0.333, 0.667, 1]);
    expect(valueAtSize(1 / 3, log)).toBeCloseTo(9, 6);
  });

  it('places a value by its rank among the sample of values drawn, ties sharing a rank', () => {
    const rank = sizeEncodingOf(meta, legend(), 'rank');
    // Evenly spaced by rank, however far apart the values are.
    expect(round([1, 2, 3, 100, 1000].map((v) => sizeFraction(v, rank)))).toEqual([0, 0.25, 0.5, 0.75, 1]);
    // Outside the sample it takes the nearest end; between two values, the middle of the gap.
    expect(round([0, 5000, 50].map((v) => sizeFraction(v, rank)))).toEqual([0, 1, 0.625]);
    const tied: SizeEncoding = {kind: 'rank', column: 'citations', sample: [0, 0, 0, 7], seen: 4};
    expect(sizeFraction(0, tied)).toBeCloseTo(1 / 3, 6);
    expect(valueAtSize(1, rank)).toBe(1000);
  });

  it('sizes nothing by a category or a column that does not arrive with the points', () => {
    expect(sizeEncodingOf(meta, legend({sizeBy: 'field'}), 'linear').kind).toBe('none');
    expect(sizeEncodingOf(meta, legend({sizeBy: 'pages'}), 'linear').kind).toBe('none');
    expect(sizeEncodingOf(meta, legend({sizeBy: null}), 'linear').kind).toBe('none');
    // A column chosen before any of its values was drawn waits at the smallest size.
    expect(sizeEncodingOf(meta, legend({domains: {}, samples: {}}), 'log').kind).toBe('pending');
  });

  it('writes each mark’s size with its band, -1 for a mark with no value, and rewrites them when the scale changes', () => {
    const slab = new MarkSlab();
    const b = band(2, 1n, 4, {scalars: {citations: citations([0, 999, null, 9])}});
    const log = sizeEncodingOf(meta, legend(), 'log');
    const first = slab.sync([b], 2, {kind: 'uniform'}, null, '', log);
    expect(round(first.sizes)).toEqual([0, 1, -1, 0.333]);
    const linear = slab.sync([b], 2, {kind: 'uniform'}, null, '', sizeEncodingOf(meta, legend(), 'linear'));
    expect(round(linear.sizes)).toEqual([0, 1, -1, 0.009]);
    const none = slab.sync([b], 2, {kind: 'uniform'}, null, '', {kind: 'none'});
    expect([...none.sizes]).toEqual([0, 0, 0, 0]);
  });

  it('rewrites the bands already held when a new sample is published, though its length and ends are the same', () => {
    const slab = new MarkSlab();
    const b = band(2, 1n, 3, {scalars: {citations: citations([5, 50, 500])}});
    const first = sizeEncodingOf(meta, legend({samples: {citations: {values: [5, 6, 7, 500], seen: 4}}}), 'rank');
    expect(round(slab.sync([b], 2, {kind: 'uniform'}, null, '', first).sizes)).toEqual([0, 0.833, 1]);
    const next = sizeEncodingOf(meta, legend({samples: {citations: {values: [5, 60, 70, 500], seen: 8}}}), 'rank');
    expect(round(slab.sync([b], 2, {kind: 'uniform'}, null, '', next).sizes)).toEqual([0, 0.167, 1]);
  });

  it('draws a value NaN or infinite as one with no value', () => {
    const slab = new MarkSlab();
    const b = band(2, 1n, 3, {scalars: {score: {arrowType: 'f64', values: Float64Array.from([1, NaN, Infinity])}}});
    const scored = {declaredScalars: [{name: 'score', arrowType: 'f64', category: null, render: true}]} as unknown as Meta;
    const encoding = sizeEncodingOf(scored, legend({sizeBy: 'score', domains: {score: {min: 0, max: 2}}}), 'linear');
    expect(round(slab.sync([b], 2, {kind: 'uniform'}, null, '', encoding).sizes)).toEqual([0.5, -1, -1]);
  });
});

describe('the marks drawn under sizing', () => {
  const marks: MarksProjection = {bands: [band(2, 1n, 3, {scalars: {citations: citations([0, 999, null])}})], standIn: [], count: {shown: 3, total: 3, exact: true}};

  type MarkProps = {sizing: {min: number; max: number} | null; getRadius: number; pickable: boolean};

  function drawn(props: Partial<TesseraLayerInternalProps>): MarkProps {
    const manager = new LayerManager(fakeDevice(), {});
    manager.setLayers([new TesseraLayer({id: 'tessera', depth: 2, status: 'shown', meta, marks, density: 'none', ...props} as TesseraLayerInternalProps)]);
    const layer = manager.getLayers().find((l) => l.id === 'tessera') as TesseraLayer;
    const sub = (layer.getSubLayers() as Layer[]).find((l) => l.id === 'tessera-marks-p0')!;
    return sub.props as unknown as MarkProps;
  }

  it('draws each mark between the sizes chosen, picking as before, and at one radius with no size column', () => {
    const sized = drawn({legend: legend(), sizing: {min: 3, max: 11, scale: 'log'}});
    expect(sized.sizing).toEqual({min: 3, max: 11, scale: 'log'});
    // The layer's radius is the largest; the shader draws each mark at its own fraction of it.
    expect(sized.getRadius).toBe(11);
    expect(sized.pickable).toBe(true);

    const plain = drawn({legend: legend({sizeBy: null}), radius: 4, sizing: {min: 3, max: 11, scale: 'log'}});
    expect(plain.sizing).toBeNull();
    expect(plain.getRadius).toBe(4);
  });

  it('draws a radius that is not a finite number above zero at the default, and the layer’s radius covers every mark drawn', () => {
    const nonsense = drawn({legend: legend(), sizing: {min: -2, max: Number.NaN, scale: 'linear'}});
    expect(nonsense.sizing).toEqual({min: 2, max: 9, scale: 'linear'});
    // Below a pixel the quad is a pixel, and a mark with no value draws at 3 px.
    expect(drawn({legend: legend(), sizing: {min: 0.2, max: 0.8, scale: 'linear'}}).getRadius).toBe(3);
    expect(drawn({legend: legend(), sizing: {min: 12, max: 4, scale: 'linear'}}).getRadius).toBe(12);
  });
});
