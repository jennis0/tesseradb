import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

export function renderItem(state: AppState): string {
  if (!state.selected) {
    return panel('Item', '<div class="muted">click a mark</div>');
  }
  const {id, scalars, externalId} = state.selected;
  const body =
    scalars.length === 0 && externalId === null
      ? `<div class="muted">this bundle declares no scalars and carries no external id for this
          item, so the round-trip is all there is to see — a resolved item here means the identity
          inverted and this principal may see it</div>`
      : `${scalars.length === 0 ? '<div class="muted">no declared scalars in this bundle</div>' : ''}
         ${scalars.map((v, i) => row(`scalar ${i}`, String(v))).join('')}
         ${externalId ? row('external id (base64)', externalId) : ''}`;
  return panel('Item', `${row('tessera_id', id.toString())}${body}`);
}

/** A refusal from `/v1/items` is shown as a refusal, never as an empty item. */
export function renderItemError(code: string, detail: string): string {
  return panel('Item', `<div class="bad">${esc(code)}: ${esc(detail)}</div>`);
}
