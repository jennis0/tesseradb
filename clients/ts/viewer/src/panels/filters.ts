import type {AppState} from '../state.js';
import {esc, panel} from '../html.js';
import {
  activeCount,
  isDateColumn,
  isPopulated,
  microsToDate,
  offersPhrase,
  type ColumnDraft
} from '../filters.js';

/**
 * The filter controls, one per column `/v1/meta` publishes an operand set for.
 *
 * **Which controls exist is the server's answer, not this file's.** `filter_operands` names each
 * column and its family, and the family decides the control: a category has a value set so it gets
 * a list, prose gets a query box, a number gets two bounds. Nothing here keys off a column name, so
 * the 25M bundle — which carries no `abstract` — simply has no abstract box, and a schema that adds
 * a column gains its control on the next reload.
 *
 * ## The two enumerations a viewer performs, and why they are not the same
 *
 * A category **filter** offers the value set `/v1/categories` pages, which the server gates by
 * `listing` before it answers: `public` means the keys are published taxonomy whose existence
 * discloses nothing, and `per_viewer` is refused outright today. A category **legend** offers
 * something quite different — only the codes the marks on screen actually carry — because a legend
 * naming every declared value would name values that exist solely in items this principal cannot
 * see. Both are correct for their purpose and neither may be substituted for the other; the panel
 * says which it is showing.
 *
 * ## What an empty result means here
 *
 * A filter naming a value this principal cannot see and one naming a value that does not exist are
 * one outcome by contract — same status, same body, same counts. So this panel never reports "no
 * such value", and the counts panel's zero must be read as *nothing matched that you may see*.
 */
export function renderFilters(state: AppState): string {
  const operands = state.meta?.filterOperands ?? [];
  if (operands.length === 0) {
    return panel('Filters', '<div class="muted">this bundle declares nothing filterable</div>');
  }

  const active = activeCount(state.filters);
  const heading = active === 0 ? 'Filters' : `Filters · ${active} active`;
  const controls = Object.entries(state.filters)
    .map(([column, draft]) => renderControl(state, column, draft))
    .join('');

  const clear = active > 0 ? `<button id="filters-clear" type="button">clear ${active}</button>` : '';
  return panel(heading, `${controls}${clear}`);
}

function renderControl(state: AppState, column: string, draft: ColumnDraft): string {
  const on = isPopulated(draft) ? ' on' : '';
  const label = `<div class="ctl-name${on}">${esc(column)}</div>`;
  switch (draft.family) {
    case 'text':
      return `<div class="ctl">${label}${textBody(state, column, draft)}</div>`;
    case 'string':
    case 'keyword':
      return `<div class="ctl">${label}${stringBody(column, draft)}</div>`;
    case 'category':
      return `<div class="ctl">${label}${categoryBody(state, column, draft)}</div>`;
    case 'numeric':
      return `<div class="ctl">${label}${numericBody(state, column, draft)}</div>`;
  }
}

/**
 * A prose query box and its mode.
 *
 * The three modes are the operand surface rather than three spellings of one: `all` and `any` are
 * `match` with and without `minimum_should_match`, and `phrase` is a different operand whose answer
 * depends on word *order*. `phrase` is offered only when the column publishes it.
 */
function textBody(state: AppState, column: string, draft: {query: string; mode: string}): string {
  const modes: [string, string][] = [
    ['all', 'all words'],
    ['any', 'any word']
  ];
  if (offersPhrase(state.meta?.filterOperands ?? [], column)) modes.push(['phrase', 'exact phrase']);
  const options = modes
    .map(
      ([value, text]) =>
        `<option value="${value}"${draft.mode === value ? ' selected' : ''}>${text}</option>`
    )
    .join('');
  return `<div class="ctl-row">
      <input class="grow" type="search" id="flt-text-${esc(column)}" value="${esc(draft.query)}"
             placeholder="words to match" autocomplete="off" />
      <select id="flt-mode-${esc(column)}">${options}</select>
    </div>`;
}

/**
 * A free-text box for a `string` or `keyword` column.
 *
 * No value list, and there will never be one: a string column's values are row data rather than a
 * vocabulary, and a keyword's dictionary is storage the server does not serve. A client waiting for
 * an autocomplete endpoint here is waiting for one that is not coming.
 */
function stringBody(column: string, draft: {needle: string; op: string}): string {
  const options = (['contains', 'prefix', 'eq'] as const)
    .map((op) => `<option value="${op}"${draft.op === op ? ' selected' : ''}>${op}</option>`)
    .join('');
  return `<div class="ctl-row">
      <input class="grow" type="search" id="flt-str-${esc(column)}" value="${esc(draft.needle)}"
             placeholder="value" autocomplete="off" />
      <select id="flt-op-${esc(column)}">${options}</select>
    </div>`;
}

/**
 * A category's value list, as ticks.
 *
 * Selected values are hoisted to the top so a long list stays legible once something is chosen —
 * with 171 primary categories, a tick 90 rows down is a tick nobody can see. Keys are the wire
 * identity and are always shown; a label joins the key rather than replacing it, or a renamed value
 * becomes unrecognisable.
 */
function categoryBody(state: AppState, column: string, draft: {keys: string[]}): string {
  const error = state.filterValueErrors[column];
  if (error) {
    // Not listable is not the same as not filterable: the column still takes a key a client knows.
    return `<div class="bad">${esc(error.code)}: not listable</div>`;
  }
  const values = state.filterValues[column];
  if (!values) return '<div class="muted">loading values…</div>';
  if (values.length === 0) return '<div class="muted">no values listable</div>';

  const chosen = new Set(draft.keys);
  const ordered = [
    ...values.filter((v) => chosen.has(v.key)),
    ...values.filter((v) => !chosen.has(v.key))
  ];
  const ticks = ordered
    .map((v) => {
      const text = v.label && v.label !== v.key ? `${v.key} — ${v.label}` : v.key;
      return `<label class="tick"><input type="checkbox" data-cat="${esc(column)}"
        value="${esc(v.key)}"${chosen.has(v.key) ? ' checked' : ''} /> ${esc(text)}</label>`;
    })
    .join('');
  // "any of" rather than "all of": ticks within one column are a disjunction, across columns a
  // conjunction, and the arity is the only place that is visible to a user.
  const count =
    draft.keys.length > 0
      ? `<div class="muted">${draft.keys.length} of ${values.length} — any of</div>`
      : `<div class="muted">${values.length} values</div>`;
  return `<div class="checks" data-checks="${esc(column)}">${ticks}</div>${count}`;
}

/**
 * Two bounds, inclusive on both sides.
 *
 * A `timestamp_us` column gets date inputs, because nobody types a microsecond epoch count. The
 * unit conversion lives in `filters.ts`, so the draft always holds the column's own unit and only
 * the input's `type` differs here.
 */
function numericBody(
  state: AppState,
  column: string,
  draft: {gte: number | null; lte: number | null}
): string {
  if (isDateColumn(state.meta, column)) {
    return `<div class="ctl-row">
        <input class="grow" type="date" id="flt-gte-${esc(column)}"
               value="${microsToDate(draft.gte)}" />
        <span class="muted">to</span>
        <input class="grow" type="date" id="flt-lte-${esc(column)}"
               value="${microsToDate(draft.lte)}" />
      </div>`;
  }
  return `<div class="ctl-row">
      <input class="grow" type="number" id="flt-gte-${esc(column)}"
             value="${draft.gte ?? ''}" placeholder="min" />
      <span class="muted">to</span>
      <input class="grow" type="number" id="flt-lte-${esc(column)}"
             value="${draft.lte ?? ''}" placeholder="max" />
    </div>`;
}
