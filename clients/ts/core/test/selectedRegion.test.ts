import {describe, expect, it} from 'vitest';
import {SelectedRegion, type RegionProjection} from '../src/selectedRegion.js';
import {fakeClock} from './support.js';

const NO_MARKS = {bands: [], standIn: []};

function regionPart() {
  let published: RegionProjection | null = null;
  const part = new SelectedRegion({
    clock: fakeClock(),
    frame: () => ({xMin: 0, xMax: 100, yMin: 0, yMax: 200}),
    extentOf: () => null,
    covered: () => true,
    publish: (value) => (published = value),
    trace: () => {}
  });
  return {part, published: () => published};
}

describe('the selected region', () => {
  it('shows a refusal of its request as refused, with no figures', () => {
    const {part, published} = regionPart();
    part.select({kind: 'box', bbox: [0, 0, 10, 10]}, NO_MARKS);
    expect(published()?.status).toBe('loading');

    part.refuse({code: 'bad-region', detail: 'too many vertices'});
    expect(published()).toMatchObject({status: 'refused', refusal: {code: 'bad-region'}, visible: null, matched: {value: 0}});
  });

  it('has nothing to refuse without a selection', () => {
    const {part, published} = regionPart();
    part.refuse({code: 'bad-region', detail: 'too many vertices'});
    expect(published()).toBeNull();
  });
});
