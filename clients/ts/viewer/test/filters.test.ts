import {describe, expect, it} from 'vitest';
import type {FilterOperandSet} from '@tessera/client';
import {
  activeCount,
  composeFilters,
  dateToMicros,
  emptyDraft,
  isPopulated,
  microsToDate,
  offersPhrase,
  type FilterDraft
} from '../src/filters.js';

const operands = (...sets: FilterOperandSet[]) => sets;
const category = (column: string): FilterOperandSet => ({
  column,
  family: 'category',
  operands: ['eq', 'in']
});
const text = (column: string, ops = ['match', 'phrase']): FilterOperandSet => ({
  column,
  family: 'text',
  operands: ops
});
const numeric = (column: string): FilterOperandSet => ({
  column,
  family: 'numeric',
  operands: ['eq', 'in', 'range']
});

describe('the draft is seeded from what the server publishes', () => {
  /**
   * The controls are the server's answer, not the client's list. This is what makes the 25M bundle —
   * which indexes no `abstract` — show no abstract box without a line of code knowing that name.
   */
  it('makes one control per published operand set, and none for anything else', () => {
    const draft = emptyDraft(operands(category('archive'), text('title'), numeric('submitted_at')));
    expect(Object.keys(draft)).toEqual(['archive', 'title', 'submitted_at']);
    expect(emptyDraft([])).toEqual({});
    expect(emptyDraft(operands(text('title')))).not.toHaveProperty('abstract');
  });

  /** Declaration order, so two sessions filtering the same way build the same request body. */
  it('keeps the published order', () => {
    const draft = emptyDraft(operands(numeric('b'), category('a'), text('c')));
    expect(Object.keys(draft)).toEqual(['b', 'a', 'c']);
  });

  /**
   * A control this viewer cannot send is worse than no control: it would 422 on the first keystroke.
   * A text column publishing neither `match` nor `phrase` gets nothing.
   */
  it('skips a column whose operators it draws no control for', () => {
    expect(emptyDraft(operands(text('title', [])))).toEqual({});
    expect(emptyDraft(operands({column: 'x', family: 'category', operands: ['eq']}))).toEqual({});
  });

  it('offers phrase only where the column publishes it', () => {
    expect(offersPhrase(operands(text('title')), 'title')).toBe(true);
    expect(offersPhrase(operands(text('title', ['match'])), 'title')).toBe(false);
    expect(offersPhrase(operands(text('title')), 'absent')).toBe(false);
  });
});

describe('an empty control is no constraint, never a constraint matching nothing', () => {
  /**
   * The distinction the whole draft/expression split exists for. `{range: {}}` is refused by the
   * server and `{in: []}` matches nothing, so a form that emitted either while a user cleared a box
   * would blank the map on the way to a new query.
   */
  it('composes an untouched draft to null rather than to an empty expression', () => {
    const draft = emptyDraft(operands(category('archive'), text('title'), numeric('submitted_at')));
    expect(composeFilters(draft)).toBeNull();
    expect(activeCount(draft)).toBe(0);
    expect(Object.values(draft).every((d) => !isPopulated(d))).toBe(true);
  });

  it('treats whitespace as an unfilled prose box', () => {
    const draft: FilterDraft = {title: {family: 'text', query: '   ', mode: 'all'}};
    expect(isPopulated(draft.title!)).toBe(false);
    expect(composeFilters(draft)).toBeNull();
  });

  it('treats a range with neither bound as unfilled', () => {
    const draft: FilterDraft = {at: {family: 'numeric', gte: null, lte: null}};
    expect(composeFilters(draft)).toBeNull();
  });

  /** Zero is a bound. A falsy check here would silently drop `gte: 0`. */
  it('treats a zero bound as a bound', () => {
    const draft: FilterDraft = {n: {family: 'numeric', gte: 0, lte: null}};
    expect(composeFilters(draft)).toEqual({n: {range: {gte: 0}}});
  });
});

describe('composition', () => {
  it('sends one populated control as a bare leaf, not wrapped in all_of', () => {
    const draft: FilterDraft = {title: {family: 'text', query: 'quantum', mode: 'all'}};
    expect(composeFilters(draft)).toEqual({title: {match: 'quantum'}});
  });

  it('conjoins several populated controls', () => {
    const draft: FilterDraft = {
      archive: {family: 'category', keys: ['quant-ph']},
      title: {family: 'text', query: 'quantum', mode: 'all'},
      submitted_at: {family: 'numeric', gte: 1_000, lte: null},
      author_count: {family: 'numeric', gte: null, lte: null}
    };
    expect(composeFilters(draft)).toEqual({
      all_of: [
        {archive: {in: ['quant-ph']}},
        {title: {match: 'quantum'}},
        {submitted_at: {range: {gte: 1_000}}}
      ]
    });
    expect(activeCount(draft)).toBe(3);
  });

  /**
   * The three text modes are the operand surface, not three spellings of one: `all` and `any` are
   * `match` with and without `minimum_should_match`, and `phrase` is a different operand whose answer
   * depends on word order.
   */
  it('maps each prose mode to its own operand', () => {
    const of = (mode: 'all' | 'any' | 'phrase') =>
      composeFilters({title: {family: 'text', query: 'quantum entanglement', mode}});
    expect(of('all')).toEqual({title: {match: 'quantum entanglement'}});
    expect(of('any')).toEqual({
      title: {match: {query: 'quantum entanglement', minimum_should_match: 1}}
    });
    expect(of('phrase')).toEqual({title: {phrase: 'quantum entanglement'}});
  });

  /** One key or several, a category sends `in`: one code path, one thing to be wrong about. */
  it('sends a single category key as `in`', () => {
    expect(composeFilters({a: {family: 'category', keys: ['x']}})).toEqual({a: {in: ['x']}});
    expect(composeFilters({a: {family: 'category', keys: ['x', 'y']}})).toEqual({
      a: {in: ['x', 'y']}
    });
  });

  it('sends each string predicate under its own operator', () => {
    const of = (op: 'eq' | 'prefix' | 'contains') =>
      composeFilters({s: {family: 'keyword', needle: 'ab', op}});
    expect(of('eq')).toEqual({s: {eq: 'ab'}});
    expect(of('prefix')).toEqual({s: {prefix: 'ab'}});
    expect(of('contains')).toEqual({s: {contains: 'ab'}});
  });

  it('sends only the bounds a range actually has', () => {
    expect(composeFilters({n: {family: 'numeric', gte: 1, lte: 9}})).toEqual({
      n: {range: {gte: 1, lte: 9}}
    });
    expect(composeFilters({n: {family: 'numeric', gte: null, lte: 9}})).toEqual({
      n: {range: {lte: 9}}
    });
  });
});

describe('date bounds', () => {
  /** A `timestamp_us` column is microseconds; nobody types those, so the control is a date. */
  it('round-trips a date through the column unit', () => {
    const micros = dateToMicros('2015-01-01');
    expect(micros).toBe(Date.UTC(2015, 0, 1) * 1000);
    expect(microsToDate(micros)).toBe('2015-01-01');
  });

  it('reads an empty or unparseable box as an open side', () => {
    expect(dateToMicros('')).toBeNull();
    expect(dateToMicros('not-a-date')).toBeNull();
    expect(microsToDate(null)).toBe('');
  });
});
