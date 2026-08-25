import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import type {DeclaredScalar, ItemDetail, Meta, Refusal} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-item-card>` — the selected point (design §5.3 tier 2). Its fields **by name, in
 * declaration order**: `/v1/items` omits a field the item carries no value for, so position lies
 * and a card reading positionally would misattribute every field after the first gap. A text
 * column lives in the record blob and never appears in a viewport response, so this is the only
 * place its prose is ever seen. A category arrives already resolved to its key.
 *
 * A slot per field — `field-<name>` — so a host renders a title as a link into their application
 * without replacing the card, and a `tessera-open` event (the id as a decimal string) for the
 * same purpose.
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
        padding: var(--tessera-space) calc(var(--tessera-space) * 1.6);
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
      }
      [part='field'] {
        display: flex;
        justify-content: space-between;
        gap: calc(var(--tessera-space) * 1.6);
      }
      [part='field'][data-prose] {
        display: block;
      }
      [part='field'][data-prose] [part='value'] {
        display: -webkit-box;
        -webkit-line-clamp: 6;
        line-clamp: 6;
        -webkit-box-orient: vertical;
        overflow: hidden;
        overflow-wrap: anywhere;
      }
      [part='open'] {
        width: 100%;
        margin-top: var(--tessera-space);
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
    const heading = html`<h2 part="title">Item</h2>`;
    if (refusal) {
      // A refusal from `/v1/items` is a refusal, never an empty item.
      return html`${heading}<span part="state" data-state="refused"><span class="badge">refused</span><span part="refusal">${refusal.code}: ${refusal.detail}</span></span>`;
    }
    if (!item) {
      const pick = this.pick;
      if (pick?.kind === 'broken') {
        return html`${heading}<span part="state" data-state="refused"><span class="badge">fault</span
            ><span part="refusal"
              >picked mark ${pick.index} on ${pick.layer ?? 'an unnamed layer'}, which carried
              ${pick.hasIds ? `only ${pick.idCount} identities` : 'no identities'} — a layer fault, not a miss</span
            ></span>`;
      }
      if (pick?.kind === 'miss') return html`${heading}<span part="state" data-state="empty"><span class="muted">nothing under the cursor — click a mark</span></span>`;
      const status = this.resolvedStore?.get('status');
      const state = stateOf(status);
      if (state === 'detached') return html`${heading}${renderState('detached', null)}`;
      return html`${heading}<span part="state" data-state="empty"><span class="muted">click a mark</span></span>`;
    }
    const declared = meta?.declaredScalars ?? [];
    const {fields, externalId} = item.detail;
    const names = Object.keys(fields);
    const ordered = [...declared.map((c) => c.name).filter((n) => n in fields), ...names.filter((n) => !declared.some((c) => c.name === n))];
    const absent = declared.filter((c) => !(c.name in fields)).map((c) => c.name);
    const id = idString(item.id);
    return html`${heading}
      <span part="state" data-state="shown"></span>
      <div part="field" data-name="tessera_id"><span part="label">tessera_id</span><span part="value">${id}</span></div>
      ${ordered.map((name) => this.field(name, fields[name], declared.find((c) => c.name === name)))}
      ${absent.length > 0 ? html`<div class="muted">no value for ${absent.join(', ')}</div>` : nothing}
      ${externalId ? html`<div part="field" data-name="external_id"><span part="label">external id (base64)</span><span part="value">${externalId}</span></div>` : nothing}
      <button part="open" type="button" @click=${() => emit(this, 'tessera-open', {id, fields, externalId})}>open</button>`;
  }

  /** One field, presented by its declared type; prose gets a block rather than a row. */
  private field(name: string, value: unknown, column: DeclaredScalar | undefined) {
    const text = present(value, column);
    const prose = text.length > 60;
    return html`<div part="field" data-name=${name} ?data-prose=${prose}>
      <span part="label">${name}</span>
      <slot name=${`field-${name}`}><span part="value" title=${prose ? text : nothing}>${text}</span></slot>
    </div>`;
  }
}

/** A value as text, by the column's declared type — a category is already its key. */
export function present(value: unknown, column: DeclaredScalar | undefined): string {
  if (value === null || value === undefined) return '—';
  if (column?.arrowType === 'timestamp_us' && (typeof value === 'number' || typeof value === 'bigint')) {
    return new Date(Number(value) / 1000).toISOString();
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
