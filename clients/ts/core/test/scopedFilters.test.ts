import {describe, expect, it} from 'vitest';
import {composeFilters, emptyDraft} from '../src/filters.js';
import type {FilterOperandSet} from '../src/types.js';

/**
 * A group-scoped attribute as `/v1/meta` publishes it: a numeric operand with a `scope` naming the
 * group whose views its values belong to.
 */
const operands: FilterOperandSet[] = [
  {column: 'author_count', family: 'numeric', operands: ['eq', 'in', 'range']},
  {column: 'sentiment', family: 'numeric', operands: ['eq', 'in', 'range'], scope: {group: 'quarter'}}
];

describe('a group-scoped operand', () => {
  it('draws the same control an unscoped column of its family does', () => {
    // The scope changes which column the server reads, not how a value is entered; it tells the
    // client when the bare leaf would be refused.
    expect(emptyDraft(operands).filter).toEqual({
      author_count: {family: 'numeric', gte: null, lte: null},
      sentiment: {family: 'numeric', gte: null, lte: null}
    });
  });

  it('composes a pinned leaf from a pinned key, verbatim', () => {
    // The pinned spelling is part of the leaf's name, which the server resolves against `quarter`'s
    // roster.
    const draft = emptyDraft(operands);
    delete draft.filter.author_count;
    delete draft.filter.sentiment;
    draft.filter['sentiment@2026-Q3'] = {family: 'numeric', gte: 0.5, lte: null};
    expect(composeFilters(draft)).toEqual({'sentiment@2026-Q3': {range: {gte: 0.5}}});

    // By ordinal, the alias `views` publishes beside each key.
    const byOrdinal = {filter: {'sentiment@#3': {family: 'numeric', gte: 0.5, lte: null} as const}, highlight: {}};
    expect(composeFilters(byOrdinal)).toEqual({'sentiment@#3': {range: {gte: 0.5}}});
  });
});
