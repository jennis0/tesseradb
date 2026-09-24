import {describe, expect, it} from 'vitest';
import {formatCount, formatMasked, type Count, type Masked} from '../src/counts.js';

/**
 * Every branch of the formatters: `Count` shows both figures or neither, `Masked` one figure or
 * none, and nothing renders against a stale view.
 */

describe('formatCount: a served sample shows both figures or neither', () => {
  const shown: Count = {shown: 221, total: 1_994_089, exact: true};

  it('renders both figures when exact and not stale', () => {
    expect(formatCount(shown)).toBe('221 of 1,994,089');
  });

  it('renders nothing when the count is not exact: a superset must not read as a set', () => {
    expect(formatCount({...shown, exact: false})).toBe('');
  });

  it('renders nothing against a stale view, however exact', () => {
    expect(formatCount(shown, {stale: true})).toBe('');
  });

  it('honours a locale', () => {
    expect(formatCount(shown, {locale: 'en-US'})).toBe('221 of 1,994,089');
  });
});

describe('formatMasked: a number-channel scalar shows one figure or none', () => {
  const visible: Masked = {value: 12_465, exact: true};

  it('renders one figure when exact', () => {
    expect(formatMasked(visible)).toBe('12,465');
  });

  it('marks an inexact figure as approximate rather than hiding it', () => {
    // A region counted over a cover is exact for the cells, not for the shape.
    expect(formatMasked({value: 12_465, exact: false})).toBe('≈ 12,465');
  });

  it('renders nothing against a stale view', () => {
    expect(formatMasked(visible, {stale: true})).toBe('');
    expect(formatMasked({value: 1, exact: false}, {stale: true})).toBe('');
  });
});
