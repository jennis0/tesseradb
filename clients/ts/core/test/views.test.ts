import {describe, expect, it} from 'vitest';
import {enterGroup, hasOneLayout, stepView, viewLabel, viewPickerEntries, viewsOfGroup} from '../src/views.js';
import type {Meta, ViewInfo, ViewMetadataValue} from '../src/types.js';

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

/**
 * The picker's rules (`view-switching.md` §6.1–§6.2), here rather than in an element so a host
 * drawing its own control gets the same answers.
 *
 * The fixture is two plain views and two groups over one key set — an owner and a `members` layout
 * of it — with a different label arm on each of the owner's four views, so one roster exercises the
 * whole rule.
 */
const at = (y: number, m: number, d: number) => Date.UTC(y, m - 1, d, 12) * 1000;

const rostered = (id: string, group: string, key: string, metadata: Record<string, ViewMetadataValue>): ViewInfo => ({
  ...view(id),
  roster: {group, key, metadata}
});

const KEYED: {key: string; metadata: Record<string, ViewMetadataValue>}[] = [
  {key: '2026-Q2', metadata: {starts: {type: 'timestamp_us', value: at(2026, 4, 1)}, ends: {type: 'timestamp_us', value: at(2026, 6, 30)}}},
  {key: '2026-Q10', metadata: {label: {type: 'text', value: 'Long quarter'}}},
  {key: '2026-Q3', metadata: {title: {type: 'text', value: 'Third quarter'}}},
  {key: '2026-Q4', metadata: {}}
];

const owner = KEYED.map((q) => rostered(`quarter:${q.key}`, 'quarter', q.key, q.metadata));
const members = KEYED.map((q) => rostered(`world:${q.key}`, 'world', q.key, {}));

const rich = {
  views: [{...view('knn'), displayName: 'knn'}, {...view('pca64'), displayName: 'pca64'}, ...owner, ...members],
  groups: [
    {name: 'quarter', title: 'Quarterly embedding', membersOf: null, views: owner.map((v) => v.id)},
    {name: 'world', title: null, membersOf: 'quarter', views: members.map((v) => v.id)}
  ]
} as unknown as Meta;

const labelOf = (id: string) => viewLabel(rich, rich.views.find((v) => v.id === id)!);

describe('a view\u2019s label', () => {
  it('reads a text label, else a text title, else a starts/ends range, else nothing but the key', () => {
    expect(labelOf('quarter:2026-Q10')).toEqual({label: 'Long quarter', key: '2026-Q10'});
    expect(labelOf('quarter:2026-Q3')).toEqual({label: 'Third quarter', key: '2026-Q3'});
    expect(labelOf('quarter:2026-Q2')).toEqual({label: 'Apr – Jun 2026', key: '2026-Q2'});
    expect(labelOf('quarter:2026-Q4')).toEqual({label: null, key: '2026-Q4'});
  });

  it('draws a range by its months only where the endpoints are whole ones, and the year once', () => {
    const span = (from: number, to: number | null) => {
      const one = rostered('g:k', 'g', 'k', {starts: {type: 'timestamp_us', value: from}, ...(to === null ? {} : {ends: {type: 'timestamp_us', value: to} as ViewMetadataValue})});
      return viewLabel({views: [one], groups: []} as unknown as Meta, one).label;
    };
    expect(span(at(2026, 4, 1), at(2026, 6, 30))).toBe('Apr – Jun 2026');
    expect(span(at(2026, 4, 5), at(2026, 6, 20))).toBe('5 Apr – 20 Jun 2026');
    expect(span(at(2025, 11, 1), at(2026, 1, 31))).toBe('Nov 2025 – Jan 2026');
    expect(span(at(2026, 4, 5), null)).toBe('5 Apr 2026');
  });

  it('takes a members group\u2019s label from the owning group, through membersOf', () => {
    // A `members` group's views carry no metadata of their own (`views.md` §3.3).
    expect(labelOf('world:2026-Q10')).toEqual({label: 'Long quarter', key: '2026-Q10'});
    expect(labelOf('world:2026-Q4')).toEqual({label: null, key: '2026-Q4'});
  });

  it('has neither label nor key for a plain view, which is in no roster', () => {
    expect(labelOf('knn')).toEqual({label: null, key: null});
  });
});

describe('the layout entries', () => {
  it('are the plain views then the groups, in the meta\u2019s order, titled where a group has one', () => {
    expect(viewPickerEntries(rich, 'knn')).toEqual([
      {kind: 'view', id: 'knn', text: 'knn', current: true},
      {kind: 'view', id: 'pca64', text: 'pca64', current: false},
      {kind: 'group', id: 'quarter', text: 'Quarterly embedding', current: false},
      {kind: 'group', id: 'world', text: 'world', current: false}
    ]);
  });

  it('mark the group, not the view, when the current view is in one', () => {
    expect(viewPickerEntries(rich, 'world:2026-Q3').filter((e) => e.current)).toEqual([
      {kind: 'group', id: 'world', text: 'world', current: true}
    ]);
  });

  it('are one layout — and the layout picker draws nothing — for one plain view and no groups', () => {
    expect(hasOneLayout({views: [view('s0')], groups: []} as unknown as Meta)).toBe(true);
    expect(hasOneLayout(rich)).toBe(false);
  });
});

describe('entering a group', () => {
  it('keeps the key the user is on, in either direction of membersOf', () => {
    expect(enterGroup(rich, 'world', 'quarter:2026-Q3')).toBe('world:2026-Q3');
    expect(enterGroup(rich, 'quarter', 'world:2026-Q10')).toBe('quarter:2026-Q10');
  });

  it('else the key it was last left on, else the first view in creation order', () => {
    expect(enterGroup(rich, 'quarter', 'knn', '2026-Q4')).toBe('quarter:2026-Q4');
    expect(enterGroup(rich, 'quarter', 'knn')).toBe('quarter:2026-Q2');
    // A remembered key the group no longer holds is not an error: the first view answers.
    expect(enterGroup(rich, 'quarter', 'knn', '2019-Q1')).toBe('quarter:2026-Q2');
  });

  it('is null for a group with no views this session may reach', () => {
    expect(enterGroup(rich, 'no_such_group', 'knn')).toBeNull();
  });
});
