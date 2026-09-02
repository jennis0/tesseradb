import {describe, expect, it} from 'vitest';
import {composeFilters, emptyDraft} from '../src/filters.js';
import type {FilterOperandSet} from '../src/types.js';

/**
 * A group-scoped attribute as `/v1/meta` publishes it (`views.md` §5): an ordinary numeric operand
 * entry with a `scope` naming the group whose views its values are per.
 */
const operands: FilterOperandSet[] = [
  {column: 'author_count', family: 'numeric', operands: ['eq', 'in', 'range']},
  {column: 'sentiment', family: 'numeric', operands: ['eq', 'in', 'range'], scope: {group: 'quarter'}}
];

describe('a group-scoped operand', () => {
  it('draws the same control an unscoped column of its family does', () => {
    // The scope changes which column the server reads, never how the value is entered, so a client
    // that ignored the field entirely would still draw the right box — the field is what tells it
    // when the bare leaf will be refused.
    expect(emptyDraft(operands)).toEqual({
      author_count: {family: 'numeric', gte: null, lte: null, verb: 'filter'},
      sentiment: {family: 'numeric', gte: null, lte: null, verb: 'filter'}
    });
  });

  it('composes a pinned leaf from a pinned key, verbatim', () => {
    // The pin is part of the leaf's name and needs nothing of its own on the wire: a draft keyed
    // by the pinned spelling is the request the server resolves against `quarter`'s roster.
    const draft = emptyDraft(operands);
    delete draft.author_count;
    delete draft.sentiment;
    draft['sentiment@2026-Q3'] = {family: 'numeric', gte: 0.5, lte: null, verb: 'filter'};
    expect(composeFilters(draft)).toEqual({'sentiment@2026-Q3': {range: {gte: 0.5}}});

    // And by ordinal, which is the same alias `views` publishes beside each key.
    const byOrdinal = {'sentiment@#3': {family: 'numeric', gte: 0.5, lte: null, verb: 'filter'} as const};
    expect(composeFilters(byOrdinal)).toEqual({'sentiment@#3': {range: {gte: 0.5}}});
  });
});
