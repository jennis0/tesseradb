import {describe, expect, it} from 'vitest';
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
});
