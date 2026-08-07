import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * The clicked mark's drill-down.
 *
 * **Scalars are named and categories are decoded**, because `/v1/items` returns them positionally
 * against `MANIFEST.declared_scalars` and a bare list of eighteen numbers says nothing. The names
 * come from `/v1/meta`, in declaration order — the same order the tail is stored and read back in,
 * so a mismatch here would be the same positional slip the contract warns about, visible.
 *
 * A category is shown as its **key**, resolved against the values the legend already holds. An
 * unresolved code is labelled as one rather than silently rendered as an integer, since an integer
 * beside a named value reads as a value rather than as a gap.
 */
export function renderItem(state: AppState): string {
  if (!state.selected) {
    return panel('Item', '<div class="muted">click a mark</div>');
  }
  const {id, scalars, externalId} = state.selected;
  const declared = state.meta?.declaredScalars ?? [];

  if (scalars.length === 0 && externalId === null) {
    return panel(
      'Item',
      `${row('tessera_id', id.toString())}
       <div class="muted">this bundle declares no scalars and carries no external id for this
        item, so the round-trip is all there is to see — a resolved item here means the identity
        inverted and this principal may see it</div>`
    );
  }

  const rows = scalars
    .map((value, i) => {
      const column = declared[i];
      // Positional against the manifest. A response carrying more scalars than `/v1/meta` declares
      // means the two disagree, which is worth showing rather than hiding behind a slice.
      if (!column) return row(`scalar ${i} (undeclared)`, String(value));
      if (!column.category) return row(column.name, formatPlain(value));
      return row(column.name, formatCode(state, column.name, value));
    })
    .join('');

  return panel(
    'Item',
    `${row('tessera_id', id.toString())}
     ${scalars.length === 0 ? '<div class="muted">no declared scalars in this bundle</div>' : ''}
     ${rows}
     ${externalId ? row('external id (base64)', externalId) : ''}`
  );
}

function formatPlain(value: unknown): string {
  return value === null || value === undefined ? '—' : String(value);
}

/**
 * A category code as its key.
 *
 * Falls back to naming the code when the legend has not resolved it — which happens when the
 * column is not the one being coloured, or when its vocabulary is `per_viewer` and therefore
 * refused. Both are honest states, and neither is an integer masquerading as a value.
 */
function formatCode(state: AppState, column: string, value: unknown): string {
  const code = Number(value);
  if (!Number.isFinite(code)) return formatPlain(value);
  if (code === 0) return 'absent';
  const resolved = state.categories[column]?.find((v) => v.code === code);
  if (!resolved) return `code ${code} (not resolved)`;
  return resolved.label && resolved.label !== resolved.key
    ? `${resolved.key} — ${resolved.label}`
    : resolved.key;
}

/** A refusal from `/v1/items` is shown as a refusal, never as an empty item. */
export function renderItemError(code: string, detail: string): string {
  return panel('Item', `<div class="bad">${esc(code)}: ${esc(detail)}</div>`);
}
