import {describe, expect, it} from 'vitest';
import {NO_COUNT, NO_MASKED, type FiltersProjection, type RegionProjection, type ViewProjection} from '@tesseradb/client';
import {washChannel} from '../src/map.js';

/**
 * §5.3's wash reads the count the question actually put, and the interface labels it as that
 * count. What is checked is the **choice**, not the image: the three columns legitimately agree —
 * `highlighted` equals `matched` with no highlight and `matched` equals `visible` with no filter —
 * so nothing about the numbers could tell the three states apart.
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
  draft: {},
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

describe('washChannel', () => {
  it('is visible with nothing set', () => {
    expect(washChannel(filters(), view(), null)).toBe('visible');
  });

  it('is matched under a filter, whichever of the three the request carries it as', () => {
    // A control in the filter position.
    expect(washChannel(filters({expr: {archive: {in: ['cs']}}}), view(), null)).toBe('matched');
    // A `member_of` clause in the filter position — the card's *filter to this*.
    expect(washChannel(filters({members: [{layer: 'l', artifact: 1n, outside: false, verb: 'filter'}]}), view(), null)).toBe('matched');
    // A drawn region, which rides `filters` and nothing else says so.
    expect(washChannel(filters(), view(), region)).toBe('matched');
  });

  it('is visible under a clause that is only a highlight — the request carried no filter', () => {
    // The state the `selection` misreading made unreachable: a `member_of` clause in the highlight
    // position moves no mask, so the wash is `visible` until `highlighting` says otherwise.
    expect(washChannel(filters({members: [{layer: 'l', artifact: 1n, outside: false, verb: 'highlight'}]}), view(), null)).toBe('visible');
  });

  it('is highlighted whenever a highlight was asked, filter or no filter', () => {
    expect(washChannel(filters({highlight: {archive: {in: ['cs']}}}), view(true), null)).toBe('highlighted');
    expect(washChannel(filters({expr: {archive: {in: ['cs']}}, highlight: {archive: {in: ['cs']}}}), view(true), region)).toBe('highlighted');
  });
});
