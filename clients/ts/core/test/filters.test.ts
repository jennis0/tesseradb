import {describe, expect, it} from 'vitest';
import {emptyDraft} from '../src/filters.js';

describe('an empty draft', () => {
  it('seeds a keyword control with the first operator the column publishes, in the order published', () => {
    const draft = emptyDraft([
      {column: 'doi', family: 'keyword', operands: ['eq', 'in', 'prefix', 'contains']},
      {column: 'venue', family: 'keyword', operands: ['contains', 'prefix']},
      {column: 'code', family: 'keyword', operands: ['in', 'prefix']},
      {column: 'tag', family: 'keyword', operands: ['in']}
    ]);
    expect(draft).toEqual({
      doi: {family: 'keyword', needle: '', op: 'eq', verb: 'filter'},
      venue: {family: 'keyword', needle: '', op: 'contains', verb: 'filter'},
      code: {family: 'keyword', needle: '', op: 'prefix', verb: 'filter'}
    });
  });
});
