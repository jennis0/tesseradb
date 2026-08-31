import {describe, expect, it} from 'vitest';
import {stepView, viewsOfGroup} from '../src/views.js';
import type {Meta, ViewInfo} from '../src/types.js';

const view = (id: string, group?: string, key?: string): ViewInfo => ({
  id,
  displayName: id,
  quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1},
  projection: 'none',
  worldAspect: null,
  tileScheme: null,
  tile: null,
  roster: group === undefined ? null : {group, key: key!, metadata: {}}
});

/**
 * A roster whose **keys sort in a different order than the server lists them** — `2026-Q10` sorts
 * before `2026-Q2` as a string — which is what makes these two functions worth having: the
 * group's list is the order, and a client that compared keys would walk this group backwards
 * through its middle.
 */
const meta = {
  views: [
    view('world'),
    view('quarter:2026-Q2', 'quarter', '2026-Q2'),
    view('quarter:2026-Q10', 'quarter', '2026-Q10'),
    view('quarter:2026-Q11', 'quarter', '2026-Q11')
  ],
  groups: [{name: 'quarter', membersOf: null, views: ['quarter:2026-Q2', 'quarter:2026-Q10', 'quarter:2026-Q11']}]
} as unknown as Meta;

describe('the roster', () => {
  it('orders a group as the server lists it, not by key', () => {
    expect(viewsOfGroup(meta, 'quarter').map((v) => v.id)).toEqual([
      'quarter:2026-Q2',
      'quarter:2026-Q10',
      'quarter:2026-Q11'
    ]);
  });

  it('answers the empty list for a group nobody declared', () => {
    expect(viewsOfGroup(meta, 'no_such_group')).toEqual([]);
  });

  it('steps previous and next along the roster, and stops at both ends', () => {
    expect(stepView(meta, 'quarter:2026-Q10', 1)?.id).toBe('quarter:2026-Q11');
    expect(stepView(meta, 'quarter:2026-Q10', -1)?.id).toBe('quarter:2026-Q2');
    expect(stepView(meta, 'quarter:2026-Q11', 1)).toBeNull();
    expect(stepView(meta, 'quarter:2026-Q2', -1)).toBeNull();
  });

  it('has no neighbours for a plain view, which is in no group', () => {
    expect(stepView(meta, 'world', 1)).toBeNull();
    expect(stepView(meta, 'unknown', 1)).toBeNull();
  });
});
