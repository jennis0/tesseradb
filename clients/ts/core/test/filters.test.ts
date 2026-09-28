import {describe, expect, it} from 'vitest';
import {activeCount, composeFilters, emptyDraft, withoutClause, type FilterDraft} from '../src/filters.js';
import {withMember, withMembers, withoutMember, type MemberClause} from '../src/members.js';

describe('an empty draft', () => {
  it('seeds a keyword control with the first operator the column publishes, in the order published', () => {
    const draft = emptyDraft([
      {column: 'doi', family: 'keyword', operands: ['eq', 'in', 'prefix', 'contains']},
      {column: 'venue', family: 'keyword', operands: ['contains', 'prefix']},
      {column: 'code', family: 'keyword', operands: ['in', 'prefix']},
      {column: 'tag', family: 'keyword', operands: ['in']}
    ]);
    expect(draft).toEqual({
      filter: {
        doi: {family: 'keyword', needle: '', op: 'eq'},
        venue: {family: 'keyword', needle: '', op: 'contains'},
        code: {family: 'keyword', needle: '', op: 'prefix'}
      },
      highlight: {}
    });
  });
});

describe('a column with a filter and a highlight', () => {
  const both: FilterDraft = {
    filter: {field: {family: 'category', keys: ['cs.LG', 'cs.CV']}, title: {family: 'text', query: '', mode: 'phrase'}},
    highlight: {field: {family: 'category', keys: ['cs.CV']}}
  };

  it('composes each position from its own clauses and counts both', () => {
    expect(composeFilters(both, 'filter')).toEqual({field: {in: ['cs.LG', 'cs.CV']}});
    expect(composeFilters(both, 'highlight')).toEqual({field: {in: ['cs.CV']}});
    expect(activeCount(both)).toBe(2);
    expect(activeCount(both, 'highlight')).toBe(1);
  });

  it('empties one position and keeps the other', () => {
    const lit = withoutClause(both, 'field', 'filter');
    expect(composeFilters(lit, 'filter')).toBeNull();
    expect(composeFilters(lit, 'highlight')).toEqual({field: {in: ['cs.CV']}});
    expect(lit.filter.field).toEqual({family: 'category', keys: []});

    const typed = withoutClause({...both, filter: {title: {family: 'text', query: 'guidance', mode: 'phrase'}}}, 'title', 'filter');
    expect(typed.filter.title).toEqual({family: 'text', query: '', mode: 'phrase'});
  });

  it('sets one position without changing the other', () => {
    const edited: FilterDraft = {...both, highlight: {field: {family: 'category', keys: ['stat.ML']}}};
    expect(composeFilters(edited, 'filter')).toEqual({field: {in: ['cs.LG', 'cs.CV']}});
    expect(composeFilters(edited, 'highlight')).toEqual({field: {in: ['stat.ML']}});
    const narrowed: FilterDraft = {...both, filter: {...both.filter, field: {family: 'category', keys: ['cs.CV']}}};
    expect(composeFilters(narrowed, 'highlight')).toEqual({field: {in: ['cs.CV']}});
  });

  it('composes the highlight in the filter position\'s column order, whatever order it was set in', () => {
    const seeded = emptyDraft([
      {column: 'field', family: 'category', operands: ['in']},
      {column: 'year', family: 'numeric', operands: ['range']}
    ]);
    const lit: FilterDraft = {...seeded, highlight: {year: {family: 'numeric', gte: 2000, lte: null}, extra: {family: 'category', keys: ['x']}, field: {family: 'category', keys: ['cs.CV']}}};
    expect(composeFilters(lit, 'highlight')).toEqual({all_of: [{field: {in: ['cs.CV']}}, {year: {range: {gte: 2000}}}, {extra: {in: ['x']}}]});
  });
});

describe('member_of clauses on one artifact', () => {
  const filter: MemberClause = {layer: 'topics', artifact: 4n, outside: false, verb: 'filter'};
  const highlight: MemberClause = {...filter, verb: 'highlight'};

  it('keeps one clause in each position', () => {
    const held = withMember(withMember([filter], highlight), {...filter, outside: true});
    expect(withMembers(null, held, 'filter')).toEqual({none_of: [{member_of: {layer: 'topics', artifact: '4'}}]});
    expect(withMembers(null, held, 'highlight')).toEqual({member_of: {layer: 'topics', artifact: '4'}});
    const left = withoutMember(held, 'topics', 4n, 'filter');
    expect(withMembers(null, left, 'filter')).toBeNull();
    expect(withMembers(null, left, 'highlight')).toEqual({member_of: {layer: 'topics', artifact: '4'}});
  });
});
