import type {FilterExpr, FilterOperandSet, FilterOperator} from './types.js';

/**
 * A filter draft, and how it becomes the expression `/v1/viewport` takes.
 *
 * A draft holds controls, including empty ones; an expression holds only predicates. An empty
 * control means no constraint: `{range: {}}` and `{in: []}` would be refused or match nothing.
 * Populated controls are joined by `all_of`, and the values within one category control by `in`.
 * The controls and their units are the renderer's.
 */

/** What a `text` control asks for. */
export type TextMode =
  /** `match`: every analysed token must appear, in any order and any position. */
  | 'all'
  /** `match` with `minimum_should_match: 1`: any one token is enough. */
  | 'any'
  /** `phrase`: the tokens adjacent and in order. */
  | 'phrase';

/**
 * Where a clause is sent. Moving a clause changes only this field, so it is not re-entered.
 *
 * - `filter`: the clause joins `filters`; the map narrows to the matches.
 * - `highlight`: the clause joins `highlight`; the matches are lit and the rest dulled.
 */
export type ClauseVerb = 'filter' | 'highlight';

/** What a control asks: the predicate alone. */
export type ColumnPredicate =
  | {family: 'text'; query: string; mode: TextMode}
  | {family: 'keyword'; needle: string; op: 'eq' | 'prefix' | 'contains'}
  /** Selected category keys; the server resolves them to codes. */
  | {family: 'category'; keys: string[]}
  /** Inclusive bounds in the column's own units; `null` for an open side. */
  | {family: 'numeric'; gte: number | null; lte: number | null};

/** One control: what it asks, and which of the request's two expressions it joins. */
export type ColumnDraft = ColumnPredicate & {verb: ClauseVerb};

export type FilterDraft = Record<string, ColumnDraft>;

/** Whether a control carries a predicate. */
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
 * Composes the draft's clauses for one position into an expression, or `null` for none, so an
 * unfiltered request and one whose filter constrains nothing are the same request. A single clause
 * is sent as a bare leaf.
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
 * How many controls carry a predicate, for a panel heading. With a `verb`, how many in that
 * position.
 */
export function activeCount(draft: FilterDraft, verb?: ClauseVerb): number {
  return Object.values(draft).filter((d) => isPopulated(d) && (verb === undefined || d.verb === verb)).length;
}

/** Moves one column's clause to the other position, keeping its predicate. */
export function withVerb(draft: FilterDraft, column: string, verb: ClauseVerb): FilterDraft {
  const control = draft[column];
  if (!control || control.verb === verb) return draft;
  return {...draft, [column]: {...control, verb}};
}

/**
 * A draft with one empty control per filterable column, from `/v1/meta`'s `filter_operands`. A
 * column that publishes no operator a control here can send gets no control. A keyword control
 * starts on whichever of `eq`, `prefix` and `contains` the column publishes first.
 *
 * A key is the leaf's name as sent. A group-scoped column is seeded under its bare name, which the
 * server answers only under a view of its group; a caller drawing it under another view re-keys it
 * as `column@key`.
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
