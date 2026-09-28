import type {FilterExpr, FilterOperandSet, FilterOperator} from './types.js';

/**
 * A filter draft, and how it becomes the expression `/v1/viewport` takes.
 *
 * A draft holds controls, including empty ones; an expression holds only predicates. An empty
 * control means no constraint: `{range: {}}` and `{in: []}` would be refused or match nothing.
 * Populated controls are joined by `all_of`, and the values within one category control by `in`.
 * The controls and their units are the renderer's.
 */

/**
 * How a `text` control matches its query. `all` sends `match`: every analysed token must appear, in
 * any order and position. `any` sends `match` with `minimum_should_match: 1`: one token is enough.
 * `phrase` sends `phrase`: the tokens adjacent and in order.
 *
 * @category Filters
 */
export type TextMode =
  /** `match`: every analysed token must appear, in any order and any position. */
  | 'all'
  /** `match` with `minimum_should_match: 1`: any one token is enough. */
  | 'any'
  /** `phrase`: the tokens adjacent and in order. */
  | 'phrase';

/**
 * Which of a request's two expressions a clause joins.
 *
 * - `filter`: the clause joins `filters`, and the map and its counts narrow to the matches.
 * - `highlight`: the clause joins `highlight`, and the matches are lit and the rest dulled.
 *
 * @category Filters
 */
export type ClauseVerb = 'filter' | 'highlight';

/**
 * One filter control: what it asks, one variant per column family. A control whose predicate is
 * empty asks nothing (see {@link isPopulated}).
 *
 * @category Filters
 */
export type ColumnDraft =
  /** A search of `query` by analysed words, matched as `mode` says. The query is trimmed. */
  | {family: 'text'; query: string; mode: TextMode}
  /** `needle` compared with the whole value (`eq`), its start (`prefix`) or any part (`contains`). */
  | {family: 'keyword'; needle: string; op: 'eq' | 'prefix' | 'contains'}
  /**
   * The selected category keys, sent as `in`. The server resolves keys to codes. A key this viewer
   * cannot see matches nothing, as a key that does not exist does, and neither is refused.
   */
  | {family: 'category'; keys: string[]}
  /** Inclusive bounds in the column's own units, sent as `range`; `null` for an open side. */
  | {family: 'numeric'; gte: number | null; lte: number | null};

/**
 * A filter panel's controls in each of the request's two expressions, keyed by the column name the
 * leaf is sent under. A column may have a control in both, so one field can narrow the map to some
 * values and light a few of them. Either record may hold empty controls, which constrain nothing.
 * {@link emptyDraft} seeds one; {@link composeFilters} turns it into the expressions a request
 * carries.
 *
 * @category Filters
 */
export type FilterDraft = {
  /** The controls whose clauses join `filters`: once seeded, one per filterable column. */
  filter: Record<string, ColumnDraft>;
  /** The controls whose clauses join `highlight`. */
  highlight: Record<string, ColumnDraft>;
};

/**
 * Whether a control carries a predicate: a text query that is not blank, a keyword needle that is
 * not empty, at least one category key, or at least one numeric bound.
 *
 * @category Filters
 */
export function isPopulated(draft: ColumnDraft): boolean {
  switch (draft.family) {
    case 'text':
      return draft.query.trim().length > 0;
    case 'keyword':
      return draft.needle.length > 0;
    case 'category':
      return draft.keys.length > 0;
    case 'numeric':
      return draft.gte !== null || draft.lte !== null;
  }
}

/**
 * The operator one populated control becomes. A category control with one key still sends `in`,
 * which asks the same as `eq`.
 */
function operatorOf(draft: ColumnDraft): FilterOperator {
  switch (draft.family) {
    case 'text': {
      const query = draft.query.trim();
      if (draft.mode === 'phrase') return {phrase: query};
      if (draft.mode === 'any') return {match: {query, minimum_should_match: 1}};
      return {match: query};
    }
    case 'keyword':
      return draft.op === 'eq'
        ? {eq: draft.needle}
        : draft.op === 'prefix'
          ? {prefix: draft.needle}
          : {contains: draft.needle};
    case 'category':
      return {in: draft.keys};
    case 'numeric': {
      const range: {gte?: number; lte?: number} = {};
      if (draft.gte !== null) range.gte = draft.gte;
      if (draft.lte !== null) range.lte = draft.lte;
      return {range};
    }
  }
}

/**
 * Composes the populated controls in one position into a filter expression: `null` for none, the
 * bare leaf for one, and `all_of` over the leaves for several, in the position's key order. A draft
 * that constrains nothing therefore sends the same request as no filter.
 *
 * @param verb - The position to compose. Defaults to `filter`.
 *
 * @category Filters
 */
export function composeFilters(draft: FilterDraft, verb: ClauseVerb = 'filter'): FilterExpr | null {
  const leaves: FilterExpr[] = [];
  // The draft is seeded in `/v1/meta`'s declaration order, so leaves follow the schema and two
  // sessions filtering alike send the same body.
  for (const [column, control] of Object.entries(draft[verb])) {
    if (!isPopulated(control)) continue;
    leaves.push({[column]: operatorOf(control)} as FilterExpr);
  }
  if (leaves.length === 0) return null;
  if (leaves.length === 1) return leaves[0]!;
  return {all_of: leaves};
}

/**
 * How many controls carry a predicate, for a panel heading. A column with a clause in both
 * positions counts twice. With `verb`, only those in that position are counted.
 *
 * @category Filters
 */
export function activeCount(draft: FilterDraft, verb?: ClauseVerb): number {
  const verbs: ClauseVerb[] = verb === undefined ? ['filter', 'highlight'] : [verb];
  return verbs.reduce((n, v) => n + Object.values(draft[v]).filter(isPopulated).length, 0);
}

/**
 * Returns a draft with `column`'s control in position `verb` emptied, keeping its family, text
 * mode and keyword operator. The control in the other position is kept. Returns `draft` itself
 * where the column has no control there.
 *
 * @category Filters
 */
export function withoutClause(draft: FilterDraft, column: string, verb: ClauseVerb): FilterDraft {
  const control = draft[verb][column];
  if (!control) return draft;
  const empty: ColumnDraft =
    control.family === 'text'
      ? {...control, query: ''}
      : control.family === 'keyword'
        ? {...control, needle: ''}
        : control.family === 'category'
          ? {family: 'category', keys: []}
          : {family: 'numeric', gte: null, lte: null};
  return {...draft, [verb]: {...draft[verb], [column]: empty}};
}

/**
 * Returns a draft with `column`'s clause moved from the other position to `verb`, and emptied
 * where it was. Where `verb` already holds a clause on the column, the two are merged: category
 * keys are joined, a numeric range widens to span both (an open side stays open), and a text or
 * keyword clause replaces the one there. Returns `draft` itself where the other position holds no
 * clause on the column.
 *
 * @category Filters
 */
export function withVerb(draft: FilterDraft, column: string, verb: ClauseVerb): FilterDraft {
  const from: ClauseVerb = verb === 'filter' ? 'highlight' : 'filter';
  const moved = draft[from][column];
  if (!moved || !isPopulated(moved)) return draft;
  const held = draft[verb][column];
  let merged: ColumnDraft = moved;
  if (held && isPopulated(held)) {
    if (held.family === 'category' && moved.family === 'category') {
      merged = {family: 'category', keys: [...held.keys, ...moved.keys.filter((k) => !held.keys.includes(k))]};
    } else if (held.family === 'numeric' && moved.family === 'numeric') {
      const wider = (a: number | null, b: number | null, pick: (...n: number[]) => number) => (a === null || b === null ? null : pick(a, b));
      merged = {family: 'numeric', gte: wider(held.gte, moved.gte, Math.min), lte: wider(held.lte, moved.lte, Math.max)};
    }
  }
  const emptied = withoutClause(draft, column, from);
  return {...emptied, [verb]: {...emptied[verb], [column]: merged}};
}

/**
 * A draft with one empty control per filterable column in `operands` (`Meta.filterOperands`), each
 * in the `filter` position, and none in the `highlight` position. A column gets a control only where it publishes the operator the
 * control sends: `match` for text (the control starts in mode `all`), `in` for category and `range`
 * for numeric. A keyword control starts on whichever of `eq`, `prefix` and `contains` the column
 * publishes first.
 *
 * A group-scoped column is keyed by its bare name, which the server answers only under a view of
 * its group or a group sharing its views. To filter it under another view, re-key the control as
 * `column@key`, naming one view of the group by its key; the bare name there is refused with `422`.
 *
 * @category Filters
 */
export function emptyDraft(operands: FilterOperandSet[]): FilterDraft {
  const draft: Record<string, ColumnDraft> = {};
  for (const {column, family, operands: ops} of operands) {
    switch (family) {
      case 'text':
        if (ops.includes('match')) draft[column] = {family: 'text', query: '', mode: 'all'};
        break;
      case 'keyword': {
        const op = ops.find((o): o is 'eq' | 'prefix' | 'contains' => o === 'eq' || o === 'prefix' || o === 'contains');
        if (op) draft[column] = {family, needle: '', op};
        break;
      }
      case 'category':
        if (ops.includes('in')) draft[column] = {family: 'category', keys: []};
        break;
      case 'numeric':
        if (ops.includes('range')) draft[column] = {family: 'numeric', gte: null, lte: null};
        break;
    }
  }
  return {filter: draft, highlight: {}};
}
