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
 * Which of a request's two expressions a clause joins. Moving a clause between them changes only
 * this field.
 *
 * - `filter`: the clause joins `filters`, and the map and its counts narrow to the matches.
 * - `highlight`: the clause joins `highlight`, and the matches are lit and the rest dulled.
 *
 * @category Filters
 */
export type ClauseVerb = 'filter' | 'highlight';

/**
 * What one filter control asks, without its position: one variant per column family. A control
 * whose predicate is empty asks nothing (see {@link isPopulated}).
 *
 * @category Filters
 */
export type ColumnPredicate =
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
 * One filter control: its predicate, and which of the request's two expressions it joins.
 *
 * @category Filters
 */
export type ColumnDraft = ColumnPredicate & {
  /** The expression the control joins. */
  verb: ClauseVerb;
};

/**
 * A filter panel's controls, keyed by the column name the leaf is sent under. It may hold empty
 * controls, which constrain nothing. {@link emptyDraft} seeds one; {@link composeFilters} turns it
 * into the expressions a request carries.
 *
 * @category Filters
 */
export type FilterDraft = Record<string, ColumnDraft>;

/**
 * Whether a control carries a predicate: a text query that is not blank, a keyword needle that is
 * not empty, at least one category key, or at least one numeric bound.
 *
 * @category Filters
 */
export function isPopulated(draft: ColumnPredicate): boolean {
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
function operatorOf(draft: ColumnPredicate): FilterOperator {
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
 * bare leaf for one, and `all_of` over the leaves for several, in the draft's key order. A draft
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
  for (const [column, control] of Object.entries(draft)) {
    if (control.verb !== verb) continue;
    if (!isPopulated(control)) continue;
    leaves.push({[column]: operatorOf(control)} as FilterExpr);
  }
  if (leaves.length === 0) return null;
  if (leaves.length === 1) return leaves[0]!;
  return {all_of: leaves};
}

/**
 * How many controls carry a predicate, for a panel heading. With `verb`, only those in that
 * position are counted.
 *
 * @category Filters
 */
export function activeCount(draft: FilterDraft, verb?: ClauseVerb): number {
  return Object.values(draft).filter((d) => isPopulated(d) && (verb === undefined || d.verb === verb)).length;
}

/**
 * Returns a draft with `column`'s control moved to `verb`, its predicate kept. Returns `draft`
 * itself where the column has no control or the control is already in that position.
 *
 * @category Filters
 */
export function withVerb(draft: FilterDraft, column: string, verb: ClauseVerb): FilterDraft {
  const control = draft[column];
  if (!control || control.verb === verb) return draft;
  return {...draft, [column]: {...control, verb}};
}

/**
 * A draft with one empty control per filterable column in `operands` (`Meta.filterOperands`), each
 * in the `filter` position. A column gets a control only where it publishes the operator the
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
  const draft: FilterDraft = {};
  for (const {column, family, operands: ops} of operands) {
    switch (family) {
      case 'text':
        if (ops.includes('match')) draft[column] = {family: 'text', query: '', mode: 'all', verb: 'filter'};
        break;
      case 'keyword': {
        const op = ops.find((o): o is 'eq' | 'prefix' | 'contains' => o === 'eq' || o === 'prefix' || o === 'contains');
        if (op) draft[column] = {family, needle: '', op, verb: 'filter'};
        break;
      }
      case 'category':
        if (ops.includes('in')) draft[column] = {family: 'category', keys: [], verb: 'filter'};
        break;
      case 'numeric':
        if (ops.includes('range')) draft[column] = {family: 'numeric', gte: null, lte: null, verb: 'filter'};
        break;
    }
  }
  return draft;
}
