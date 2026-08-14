import type {FilterExpr, FilterOperandSet, FilterOperator, Meta} from '@tessera/client';

/**
 * The filter controls' draft state, and how it becomes the expression `/v1/viewport` takes.
 *
 * **The draft and the expression are deliberately different shapes.** A control holds a half-typed
 * query, an empty range and a category set nobody has ticked yet; an expression holds only
 * predicates. Keeping the two apart is what lets an empty control mean *no constraint* rather than
 * *match nothing* — `{range: {}}` and `{in: []}` are both refused or empty-matching at the server,
 * and a form that sent them as the user cleared a box would blank the map on the way to a new query.
 *
 * **Every leaf is conjoined.** The composed expression is `all_of` over the populated controls, which
 * is the only reading of several filled-in boxes a user expects. Within one category control the
 * values are `in` — a disjunction — for the same reason: ticking two archives means either.
 */

/** What a `text` control asks for. The three modes are the operand surface, not a spelling of one. */
export type TextMode =
  /** `match`: every analysed token must appear, in any order and any position. */
  | 'all'
  /** `match` with `minimum_should_match: 1` — any one token is enough. */
  | 'any'
  /** `phrase`: the tokens adjacent and in order. */
  | 'phrase';

export type ColumnDraft =
  | {family: 'text'; query: string; mode: TextMode}
  | {family: 'string' | 'keyword'; needle: string; op: 'eq' | 'prefix' | 'contains'}
  /** Selected category **keys**, not codes — the wire takes keys and resolves them server-side. */
  | {family: 'category'; keys: string[]}
  /** Inclusive bounds, as the control's own units; `null` for an open side. */
  | {family: 'numeric'; gte: number | null; lte: number | null};

export type FilterDraft = Record<string, ColumnDraft>;

/** Whether a control carries a predicate, as against merely existing. */
export function isPopulated(draft: ColumnDraft): boolean {
  switch (draft.family) {
    case 'text':
      return draft.query.trim().length > 0;
    case 'string':
    case 'keyword':
      return draft.needle.length > 0;
    case 'category':
      return draft.keys.length > 0;
    case 'numeric':
      return draft.gte !== null || draft.lte !== null;
  }
}

/**
 * The operator one populated control becomes.
 *
 * A category control with exactly one key still sends `in` rather than `eq`. The two are the same
 * question and `in` keeps the control's own arity — one code path, one thing to be wrong about — and
 * a server that answered them differently would be a bug rather than an optimisation.
 */
function operatorOf(draft: ColumnDraft): FilterOperator {
  switch (draft.family) {
    case 'text': {
      const query = draft.query.trim();
      if (draft.mode === 'phrase') return {phrase: query};
      if (draft.mode === 'any') return {match: {query, minimum_should_match: 1}};
      return {match: query};
    }
    case 'string':
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
 * Compose the draft into an expression, or `null` for the unfiltered request.
 *
 * Null rather than an empty `all_of`: an unfiltered request and a request whose filter constrains
 * nothing must be the *same* request, or the two states differ in every byte ledger and cache key
 * downstream while meaning the same thing.
 *
 * A single populated control is sent as a bare leaf rather than wrapped in a one-element `all_of` —
 * the shorter form is what a reader of a captured request expects to see, and the server treats
 * them identically.
 */
export function composeFilters(draft: FilterDraft): FilterExpr | null {
  const leaves: FilterExpr[] = [];
  // Object key order is insertion order, and the draft is seeded in `/v1/meta`'s declaration order,
  // so the composed expression's leaves read in schema order rather than in the order a user
  // happened to fill the boxes in. That makes two sessions filtering the same way produce the same
  // request body, which is what a request log has to be able to assume.
  for (const [column, control] of Object.entries(draft)) {
    if (!isPopulated(control)) continue;
    leaves.push({[column]: operatorOf(control)} as FilterExpr);
  }
  if (leaves.length === 0) return null;
  if (leaves.length === 1) return leaves[0]!;
  return {all_of: leaves};
}

/** How many controls carry a predicate — the count the panel heading shows. */
export function activeCount(draft: FilterDraft): number {
  return Object.values(draft).filter(isPopulated).length;
}

/**
 * A draft with one control per filterable column, every one empty.
 *
 * Seeded from `/v1/meta`'s `filter_operands` rather than from the declared columns, so a bundle
 * without an `abstract` simply has no abstract control — the client special-cases nothing, and a
 * column that gains a filter placement in a rebuild gains its control with no code change.
 *
 * A column whose published operator set contains none this viewer draws a control for is skipped
 * rather than given a control that cannot be sent. The panel reports the skip; silently drawing a
 * box whose operator the column refuses would produce a 422 on the first keystroke.
 */
export function emptyDraft(operands: FilterOperandSet[]): FilterDraft {
  const draft: FilterDraft = {};
  for (const {column, family, operands: ops} of operands) {
    switch (family) {
      case 'text':
        // `match` is the operand every text column has; `phrase` rides the same index and is
        // offered only when published, so a column indexed without positions keeps its box.
        if (ops.includes('match')) draft[column] = {family: 'text', query: '', mode: 'all'};
        break;
      case 'string':
      case 'keyword':
        if (ops.includes('contains')) draft[column] = {family, needle: '', op: 'contains'};
        else if (ops.includes('prefix')) draft[column] = {family, needle: '', op: 'prefix'};
        else if (ops.includes('eq')) draft[column] = {family, needle: '', op: 'eq'};
        break;
      case 'category':
        if (ops.includes('in')) draft[column] = {family: 'category', keys: []};
        break;
      case 'numeric':
        if (ops.includes('range')) draft[column] = {family: 'numeric', gte: null, lte: null};
        break;
    }
  }
  return draft;
}

/** Whether a text column's published operands include `phrase`, so the mode may offer it. */
export function offersPhrase(operands: FilterOperandSet[], column: string): boolean {
  return operands.find((o) => o.column === column)?.operands.includes('phrase') ?? false;
}

/**
 * A numeric column's control units.
 *
 * `timestamp_us` gets date inputs, because a microsecond epoch count is not a thing anyone types.
 * The conversion is here rather than in the panel so that the value the draft holds is always the
 * column's own unit — the panel formats, and nothing downstream has to know which columns are dates.
 */
export function isDateColumn(meta: Meta | null, column: string): boolean {
  return meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us';
}

/** `yyyy-mm-dd` to microseconds since the epoch, or null for an unparseable or empty box. */
export function dateToMicros(value: string): number | null {
  if (!value) return null;
  const ms = Date.parse(`${value}T00:00:00Z`);
  return Number.isFinite(ms) ? ms * 1000 : null;
}

/** Microseconds since the epoch to the `yyyy-mm-dd` an `<input type="date">` wants. */
export function microsToDate(micros: number | null): string {
  if (micros === null) return '';
  return new Date(micros / 1000).toISOString().slice(0, 10);
}
