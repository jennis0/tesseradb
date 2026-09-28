import {describe, expect, it} from 'vitest';
import {NO_COUNT, NO_MASKED, type FiltersProjection, type RegionProjection, type ViewProjection} from '@tesseradb/client';
import {densityChannel} from '../src/density.js';

/**
 * Density reads the count the request asked for. The tests check the choice, not the image: the
 * three counts are equal when nothing narrows them, so the numbers cannot tell the states apart.
 */

const view = (highlighting = false): ViewProjection => ({
  id: 's0',
  composition: null,
  depth: 0,
  visible: NO_MASKED,
  matched: NO_MASKED,
  highlighted: NO_MASKED,
  highlighting,
  served: NO_COUNT,
  provisional: 0
});

const filters = (over: Partial<FiltersProjection> = {}): FiltersProjection => ({
  draft: {filter: {}, highlight: {}},
  expr: null,
  highlight: null,
  members: [],
  suggestions: {},
  suggestErrors: {},
  suggestEpoch: 0,
  ...over
});

/** A drawn region as the store publishes one; `null` is *nothing drawn*. */
const region = {
  shape: {kind: 'box', bbox: [0, 0, 1, 1], outside: false},
  status: 'shown',
  refusal: null,
  visible: NO_MASKED,
  matched: NO_MASKED,
  served: NO_COUNT,
  held: NO_COUNT,
  exact: true,
  ms: null
} as unknown as RegionProjection;

describe('densityChannel', () => {
  it('is visible with nothing set', () => {
    expect(densityChannel(filters(), view(), null)).toBe('visible');
  });

  it('is matched under a filter, whichever of the three the request carries it as', () => {
    // A control in the filter position.
    expect(densityChannel(filters({expr: {archive: {in: ['cs']}}}), view(), null)).toBe('matched');
    // A `member_of` clause in the filter position — the card's *filter to this*.
    expect(densityChannel(filters({members: [{layer: 'l', artifact: 1n, outside: false, verb: 'filter'}]}), view(), null)).toBe('matched');
    // A drawn region, which rides `filters` and nothing else says so.
    expect(densityChannel(filters(), view(), region)).toBe('matched');
  });

  it('is visible under a clause that is only a highlight — the request carried no filter', () => {
    // The state the `selection` misreading made unreachable: a `member_of` clause in the highlight
    // position moves no mask, so density reads `visible` until `highlighting` says otherwise.
    expect(densityChannel(filters({members: [{layer: 'l', artifact: 1n, outside: false, verb: 'highlight'}]}), view(), null)).toBe('visible');
  });

  it('is highlighted whenever a highlight was asked, filter or no filter', () => {
    expect(densityChannel(filters({highlight: {archive: {in: ['cs']}}}), view(true), null)).toBe('highlighted');
    expect(densityChannel(filters({expr: {archive: {in: ['cs']}}, highlight: {archive: {in: ['cs']}}}), view(true), region)).toBe('highlighted');
  });
});
