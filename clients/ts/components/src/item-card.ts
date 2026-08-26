import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import type {DeclaredScalar, ItemDetail, Meta, Refusal} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-item-card>` — the selected point (design §5.3 tier 2), as the boards draw it: the
 * title, then the fields **by name, in declaration order** as a label/value grid, then *Open* and
 * *Copy id*. `/v1/items` omits a field the item carries no value for, so position lies and a card
 * reading positionally would misattribute every field after the first gap. A text column lives in
 * the record blob and never appears in a viewport response, so this is the only place its prose
 * is ever seen. A category arrives already resolved to its key.
 *
 * The title is the first declared text column that has a value (`title` by name where there is
 * one); a slot per field — `field-<name>` — lets a host render one as a link into their
 * application without replacing the card, and `tessera-open` (the id as a decimal string) does
 * the same for *Open*.
 *
 * **A miss and a broken pick are different.** Nothing under the cursor is the ordinary case; a
 * hit whose layer carried no identity is a fault in the map and says so, rather than reading as
 * "click a mark" for a whole session. The map passes what its pick resolved to as `pick`.
 */
export type PickOutcome =
  | {kind: 'miss'}
  | {kind: 'broken'; index: number; layer: string | null; hasIds: boolean; idCount: number}
  | null;

export class TesseraItemCard extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .card-title {
        margin-bottom: 10px;
      }
      [part='field'][data-prose] {
        grid-column: 1 / -1;
      }
      [part='field'][data-prose] [part='value'] {
        display: -webkit-box;
        -webkit-line-clamp: 6;
        line-clamp: 6;
        -webkit-box-orient: vertical;
        overflow: hidden;
        overflow-wrap: anywhere;
      }
      .field .v.mono {
        font-size: 12px;
      }
      .actions {
        margin-top: 12px;
      }
      [part='close'] {
        display: inline-flex;
        color: var(--tessera-ink-3);
      }
    `
  ];

  /** Data by property, for a host feeding the card from its own fetch (§5.6 rung 6). */
  @property({attribute: false}) accessor item: {id: bigint; detail: ItemDetail} | null = null;
  @property({attribute: false}) accessor refusal: Refusal | null = null;
  @property({attribute: false}) accessor pick: PickOutcome = null;
  @property({attribute: false}) accessor meta: Meta | null = null;

  private get shown(): {item: {id: bigint; detail: ItemDetail} | null; refusal: Refusal | null; meta: Meta | null} {
    if (this.item || this.refusal) return {item: this.item, refusal: this.refusal, meta: this.meta};
    const s = this.resolvedStore;
    if (!s) return {item: null, refusal: null, meta: this.meta};
    const sel = s.get('selection');
    return {item: sel.item, refusal: sel.itemRefusal, meta: this.meta ?? s.get('meta')};
  }

  override render() {
    const {item, refusal, meta} = this.shown;
    const heading = html`<h2 part="title">Item<button part="close" type="button" aria-label="Close" @click=${() => emit(this, 'tessera-close', {what: 'item'})}>${icon('close', 14)}</button></h2>`;
    if (refusal) {
      // A refusal from `/v1/items` is a refusal, never an empty item.
      return html`<div class="panel">${heading}<span part="state" data-state="refused"><span part="refusal">${refusal.code}: ${refusal.detail}</span></span></div>`;
    }
    if (!item) {
      const pick = this.pick;
      if (pick?.kind === 'broken') {
        return html`<div class="panel">${heading}<span part="state" data-state="refused"><span part="refusal">Layer fault: mark ${pick.index} on ${pick.layer ?? 'an unnamed layer'} carried ${pick.hasIds ? `${pick.idCount} identities` : 'no identity'}</span></span></div>`;
      }
      if (pick?.kind === 'miss') return html`<div class="panel">${heading}<span part="state" data-state="empty">Nothing under the cursor</span></div>`;
      const state = stateOf(this.resolvedStore?.get('status'));
      if (state === 'detached') return html`<div class="panel">${heading}${renderState('detached', null)}</div>`;
      return html`<div class="panel">${heading}<span part="state" data-state="empty">No item selected</span></div>`;
    }
    const declared = meta?.declaredScalars ?? [];
    const {fields, externalId} = item.detail;
    const names = Object.keys(fields);
    const ordered = [...declared.map((c) => c.name).filter((n) => n in fields), ...names.filter((n) => !declared.some((c) => c.name === n))];
    const id = idString(item.id);
    // The title: a declared text column named `title` with a value, else the first with prose.
    const titleName = ordered.find((n) => n === 'title' && typeof fields[n] === 'string') ?? ordered.find((n) => declared.find((c) => c.name === n)?.arrowType === 'utf8' && typeof fields[n] === 'string');
    const rest = ordered.filter((n) => n !== titleName);
    const copy = () => void navigator.clipboard?.writeText(id);
    return html`<div class="panel">${heading}
      <span part="state" data-state="shown"></span>
      ${titleName ? html`<div part="field" class="card-title" data-name=${titleName}><slot name=${`field-${titleName}`}><span part="value">${String(fields[titleName])}</span></slot></div>` : nothing}
      <div class="field">
        ${rest.map((name) => this.field(name, fields[name], declared.find((c) => c.name === name)))}
        <div part="field" data-name="tessera_id" style="display:contents"><span part="label" class="k">tessera_id</span><span part="value" class="v mono">${id}</span></div>
        ${externalId ? html`<div part="field" data-name="external_id" style="display:contents"><span part="label" class="k">external_id</span><span part="value" class="v mono">${externalId}</span></div>` : nothing}
      </div>
      <div class="row actions">
        <button part="open" class="btn" type="button" @click=${() => emit(this, 'tessera-open', {id, fields, externalId})}>${icon('open', 14)}Open</button>
        <button part="copy" class="btn quiet" type="button" @click=${copy}>Copy id</button>
      </div>
    </div>`;
  }

  /** One field, presented by its declared type; prose spans the grid. */
  private field(name: string, value: unknown, column: DeclaredScalar | undefined) {
    const text = present(value, column);
    const prose = text.length > 60;
    const mono = column?.arrowType === 'timestamp_us';
    return html`<div part="field" data-name=${name} ?data-prose=${prose} style=${prose ? nothing : 'display:contents'}>
      <span part="label" class="k">${name}</span>
      <slot name=${`field-${name}`}><span part="value" class=${`v${mono ? ' mono' : ''}`} title=${prose ? text : nothing}>${text}</span></slot>
    </div>`;
  }
}

/** A value as text, by the column's declared type — a category is already its key. */
export function present(value: unknown, column: DeclaredScalar | undefined): string {
  if (value === null || value === undefined) return '—';
  if (column?.arrowType === 'timestamp_us' && (typeof value === 'number' || typeof value === 'bigint')) {
    return new Date(Number(value) / 1000).toISOString().slice(0, 10);
  }
  if (typeof value === 'number') return Number.isInteger(value) ? value.toLocaleString('en-GB') : String(value);
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  return String(value);
}

attachContextRoot();
defineOnce('tessera-item-card', TesseraItemCard);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-item-card': TesseraItemCard;
  }
}
