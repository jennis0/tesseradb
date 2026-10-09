import {describe, expect, it} from 'vitest';
import {MosaicaError} from '../src/client.js';
import type {Composition} from '../src/compose.js';
import {Legend, type LegendProjection} from '../src/legend.js';
import type {CategoryValue} from '../src/types.js';
import {band, scalar, settle} from './support.js';

const ARCHIVE = scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']});

/** A frame of one band whose three points all carry category code 5. */
function frameOf(): Composition {
  const exact = [band(0, 0n, 3, {scalars: {archive: {arrowType: 'u16', values: Uint16Array.of(5, 5, 5)}}})];
  return {depth: 0, want: {x0: 0, y0: 0, x1: 0, y1: 0}, version: 1, exact, standIn: [], tiles: [], exactDrawn: 3, exactServed: 3, visibleInView: 3, provisional: 0, standInStale: false};
}

describe('the category legend', () => {
  it('does not take names asked for before a clear', async () => {
    const answers: ((values: CategoryValue[]) => void)[] = [];
    let published: LegendProjection | null = null;
    const legend = new Legend(
      () => new Promise((resolve) => answers.push(resolve)),
      (value) => (published = value)
    );
    legend.setColourBy('archive');
    legend.accumulate(frameOf(), [ARCHIVE]);
    expect(answers).toHaveLength(1);

    legend.clear();
    answers[0]!([{code: 5, key: 'cs', title: 'CS'}]);
    await settle();
    expect(published!.categories).toEqual({});
    expect(published!.colourBy).toBe('archive');
  });

  it('does not take a refusal asked for before a clear', async () => {
    const answers: ((error: Error) => void)[] = [];
    let published: LegendProjection | null = null;
    const legend = new Legend(
      () => new Promise((_, reject) => answers.push(reject)),
      (value) => (published = value)
    );
    legend.setColourBy('archive');
    legend.accumulate(frameOf(), [ARCHIVE]);
    expect(answers).toHaveLength(1);

    legend.clear();
    answers[0]!(new MosaicaError(500, 'fail-closed', 'vocabulary unreadable'));
    await settle();
    expect(published!.categoryErrors).toEqual({});
  });
});

const CITATIONS = scalar('citations', 'u32', {render: true, homes: ['rendered']});

/** A frame of one band per list, each point carrying the list's citation counts; `null` is no value. */
function citationFrame(...lists: (number | null)[][]): Composition {
  const exact = lists.map((list, i) =>
    band(2, BigInt(i), list.length, {
      scalars: {citations: {arrowType: 'u32', values: Uint32Array.from(list.map((v) => v ?? 0)), present: Uint8Array.from(list.map((v) => (v === null ? 0 : 1)))}}
    })
  );
  const n = lists.reduce((a, l) => a + l.length, 0);
  return {depth: 2, want: {x0: 0, y0: 0, x1: 0, y1: 0}, version: 1, exact, standIn: [], tiles: [], exactDrawn: n, exactServed: n, visibleInView: n, provisional: 0, standInStale: false};
}

describe('the size legend', () => {
  const sizeLegend = () => {
    let published: LegendProjection | null = null;
    const legend = new Legend(async () => [], (value) => (published = value));
    return {legend, get: () => published!};
  };

  it('takes the size column’s range and, by rank, a sorted sample of its values from the marks drawn, apart from colour', () => {
    const {legend, get} = sizeLegend();
    legend.setColourBy('archive');
    legend.setSizeBy('citations', true);
    legend.accumulate(citationFrame([4, 0, null, 1000, 12]), [ARCHIVE, CITATIONS]);
    expect(get().sizeBy).toBe('citations');
    expect(get().colourBy).toBe('archive');
    expect(get().domains.citations).toEqual({min: 0, max: 1000});
    // A point with no value takes no part in either, and is noted.
    expect(get().samples.citations).toEqual({values: [0, 4, 12, 1000], seen: 4});
    expect(get().missing.citations).toBe(true);
  });

  it('keeps no sample under a linear or log scale, and starts one from the frame when switched to rank', () => {
    const {legend, get} = sizeLegend();
    legend.setSizeBy('citations');
    const frame = citationFrame([4, 0, 7]);
    legend.accumulate(frame, [CITATIONS]);
    expect(get().samples).toEqual({});
    expect(get().domains.citations).toEqual({min: 0, max: 7});
    expect(get().missing.citations).toBeUndefined();
    legend.setSizeBy('citations', true);
    legend.accumulate(frame, [CITATIONS]);
    expect(get().samples.citations).toEqual({values: [0, 4, 7], seen: 3});
  });

  it('notes a point whose value is NaN or infinite as one with no value', () => {
    const {legend, get} = sizeLegend();
    const SCORE = scalar('score', 'f64', {render: true, homes: ['rendered']});
    legend.setSizeBy('score');
    const scores = (values: number[]) => ({...citationFrame([]), exact: [band(2, 1n, values.length, {scalars: {score: {arrowType: 'f64' as const, values: Float64Array.from(values)}}})]});
    legend.accumulate(scores([1, 2]), [SCORE]);
    expect(get().missing.score).toBeUndefined();
    legend.accumulate(scores([1, Infinity, 3]), [SCORE]);
    expect(get().missing.score).toBe(true);
    expect(get().domains.score).toEqual({min: 1, max: 3});
  });

  it('counts each band once, and publishes a new sample only once the marks seen have doubled', () => {
    const {legend, get} = sizeLegend();
    legend.setSizeBy('citations', true);
    const frame = citationFrame([1, 2, 3, 4]);
    legend.accumulate(frame, [CITATIONS]);
    const first = get().samples.citations;
    legend.accumulate(frame, [CITATIONS]);
    expect(get().samples.citations).toBe(first);
    const more = citationFrame([5, 6, 7]).exact[0]!;
    legend.accumulate({...frame, exact: [...frame.exact, {...more, prefix: 9n}]}, [CITATIONS]);
    expect(get().samples.citations).toBe(first);
    legend.accumulate(citationFrame([8]), [CITATIONS]);
    expect(get().samples.citations).toEqual({values: [1, 2, 3, 4, 5, 6, 7, 8], seen: 8});
  });

  it('holds at most 1,024 values, drawn from every mark seen', () => {
    const {legend, get} = sizeLegend();
    legend.setSizeBy('citations', true);
    legend.accumulate(citationFrame(Array.from({length: 10_000}, (_, i) => i)), [CITATIONS]);
    const sample = get().samples.citations!;
    expect(sample.seen).toBe(10_000);
    expect(sample.values).toHaveLength(1024);
    // Drawn across the range, not the first thousand values.
    expect(sample.values.at(-1)!).toBeGreaterThan(9_000);
    expect(sample.values[0]!).toBeLessThan(1_000);
  });

  it('sizes nothing by a category column, and keeps the size column through a clear', () => {
    const {legend, get} = sizeLegend();
    legend.setSizeBy('archive', true);
    legend.accumulate(frameOf(), [ARCHIVE]);
    expect(get().samples).toEqual({});
    expect(get().domains).toEqual({});
    legend.setSizeBy('citations', true);
    legend.accumulate(citationFrame([3]), [CITATIONS]);
    legend.clear();
    expect(get().sizeBy).toBe('citations');
    expect(get().samples).toEqual({});
    expect(get().domains).toEqual({});
  });
});
