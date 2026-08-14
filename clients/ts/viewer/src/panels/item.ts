import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * The clicked mark's drill-down: the whole record, from all three of its homes.
 *
 * **Named, not positional.** `/v1/items` answers with an object keyed by declared column name, and a
 * column the item carries no value for is *absent* from it rather than null. Reading it positionally
 * against `/v1/meta` would misattribute every field after the first gap — and would silently miss
 * the fields that have no position at all, which is most of what makes this panel worth having: a
 * text column lives in the record blob and never appears in a viewport response, so **this is the
 * only place its prose is ever seen**. A category arrives already resolved to its vocabulary key,
 * so nothing here decodes a code.
 *
 * Fields are shown in `/v1/meta`'s declaration order, with anything the response carried that the
 * schema does not declare listed after — a disagreement between the two documents being worth
 * showing rather than hiding behind a filter.
 */
export function renderItem(state: AppState): string {
  if (!state.selected) {
    const pick = state.lastPick;
    // **A miss and a broken pick are different, and this panel used to show neither.** Both read
    // "click a mark", so picking could fail silently for a whole session. `index < 0` is deck
    // reporting nothing under the cursor, which is the ordinary case; an index with no identity
    // array behind it, or one past the end of that array, is a defect in the layer and says so.
    if (pick && pick.index >= 0 && (!pick.hasIds || pick.index >= pick.idCount)) {
      return panel(
        'Item',
        `<div class="bad">picked mark ${pick.index} on ${esc(pick.layer ?? 'an unnamed layer')},
           which carried ${pick.hasIds ? `only ${pick.idCount} identities` : 'no identities'}</div>
         <div class="muted">the mark was hit and could not be resolved — a layer fault, not a
           miss.</div>`
      );
    }
    return panel(
      'Item',
      pick
        ? '<div class="muted">nothing under the cursor — click a mark</div>'
        : '<div class="muted">click a mark</div>'
    );
  }
  const {id, fields, externalId} = state.selected;
  const declared = state.meta?.declaredScalars ?? [];
  const names = Object.keys(fields);

  if (names.length === 0 && externalId === null) {
    return panel(
      'Item',
      `${row('tessera_id', id.toString())}
       <div class="muted">this item carries no declared field and no external id, so the round-trip
        is all there is to see — a resolved item here means the identity inverted and this principal
        may see it</div>`
    );
  }

  const ordered = [
    ...declared.map((c) => c.name).filter((name) => name in fields),
    ...names.filter((name) => !declared.some((c) => c.name === name))
  ];
  const rows = ordered.map((name) => field(name, fields[name])).join('');
  // Absence is a fact about the item, not a gap in the response, so it is worth naming — an item
  // with no `abstract` and a bundle with no `abstract` column look identical without this.
  const absent = declared.filter((c) => !(c.name in fields)).map((c) => c.name);

  return panel(
    'Item',
    `${row('tessera_id', id.toString())}
     ${rows}
     ${absent.length > 0 ? `<div class="muted">no value for ${esc(absent.join(', '))}</div>` : ''}
     ${externalId ? row('external id (base64)', externalId) : ''}`
  );
}

/**
 * One field.
 *
 * Prose gets its own block rather than a label/value row: an abstract is ~950 characters and a
 * justified two-column row turns it into a single unreadable line. Long text is clamped with the
 * full value in the `title` attribute, so the panel keeps its shape and nothing is actually lost.
 */
function field(name: string, value: unknown): string {
  const text = value === null || value === undefined ? '—' : String(value);
  if (text.length <= 60) return row(name, text);
  return `<div class="prose"><div class="prose-name">${esc(name)}</div>
    <div class="prose-body" title="${esc(text)}">${esc(text)}</div></div>`;
}

/** A refusal from `/v1/items` is shown as a refusal, never as an empty item. */
export function renderItemError(code: string, detail: string): string {
  return panel('Item', `<div class="bad">${esc(code)}: ${esc(detail)}</div>`);
}
