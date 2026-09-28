import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import type {DeclaredScalar, ItemDetail, ItemViewPosition, Meta, Quantisation, Refusal} from '@tesseradb/client';
import {GRID32} from '@tesseradb/client';
import {TesseraElement, columnCaption, emit, idString, timestampText, type PickOutcome} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {refusalText, renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * The selected item's record: a headline, the views it is in, its fields as a label and value
 * grid, its group-scoped values, then Open and Copy id. Fields are listed by name in
 * declaration order, then any the schema does not declare; a field the record has no value for is
 * left out. A declared field is captioned in words (`published_at` as "Published at"). A category
 * field shows its key. A timestamp shows in full, as `14 March 2024, 12:00 UTC`.
 * The close button appears only while the card shows something: an item, a refusal or a click's
 * result.
 *
 * The headline is the field `title-field` names, else the item's `tessera_id`. Each field has a
 * slot, `field-<name>`, so a host can render one as a link. The item is the store's selection
 * (`Store.pick`), or the `item` property.
 *
 * @summary The selected item's fields, with Open and Copy id.
 * @tagname tessera-item-card
 * @category Elements
 * @slot field-<name> - Replaces the value of the field `<name>`, in the grid or the headline.
 * @fires {CustomEvent<TesseraEventDetails['tessera-open']>} tessera-open - Open was pressed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-close']>} tessera-close - The close button was
 *   pressed, with `what` set to `item`.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewfollow']>} tessera-viewfollow - A view chip
 *   was pressed: follow the item into that view, at its position there.
 * @csspart title - The header row: the headline or the state, and the close button.
 * @csspart close - The close button.
 * @csspart state - The state line, with `data-state`: `shown`, `empty` (no item, or nothing under
 *   the cursor), `refused` or `detached`.
 * @csspart refusal - The words "Item unavailable", with `data-code` set to the refusal's code where
 *   the server refused the record. A broken pick's details are on the map's `lastPick`.
 * @csspart headline - The headline, with `data-name` set to the field it shows.
 * @csspart view-chip - One view the item is in, with `data-view` and `aria-current` on the current
 *   view.
 * @csspart field - One field, with `data-name`, and `data-prose` on a value over 60 characters.
 * @csspart label - A field's name, and the heading above the view chips.
 * @csspart value - A field's value.
 * @csspart scoped - The group-scoped values, grouped by key.
 * @csspart key - One key's heading among the group-scoped values, with `data-key`.
 * @csspart open - The Open button.
 * @csspart copy - The Copy id button, which copies the `tessera_id` to the clipboard.
 */
export class TesseraItemCard extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .head[part='title'] {
        display: flex;
        align-items: flex-start;
        justify-content: space-between;
        gap: 10px;
        margin: 0 0 10px;
        font-size: 13px;
        font-weight: 400;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_tessera-ink);
      }
      .head [part='headline'] {
        flex: 1 1 auto;
        min-width: 0;
      }
      .head [part='state'] {
        min-height: 24px;
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
      .field .v {
        font-variant-numeric: tabular-nums;
      }
      .actions {
        margin-top: 12px;
      }
      [part='close'] {
        flex: none;
        width: 24px;
        height: 24px;
        margin: -2px -4px 0 0;
        display: grid;
        place-items: center;
        border-radius: 5px;
        color: var(--_tessera-ink-2);
      }
      [part='close']:hover {
        background: var(--_tessera-surface-2);
      }
      .chips {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin: 4px 0 10px;
      }
      [part='view-chip'][aria-current='true'] {
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      [part='scoped'] {
        margin-top: 12px;
      }
      [part='key'] {
        margin: 10px 0 4px;
        text-transform: none;
        letter-spacing: 0;
        font-size: 12px;
      }
    `
  ];

  /**
   * The item to show, for a host that fetches the record itself. Set with `refusal` or alone, it
   * replaces the store's selection.
   */
  @property({attribute: false}) accessor item: {id: bigint; detail: ItemDetail} | null = null;
  /** A refusal to show in place of an item, for a host that fetches the record itself. */
  @property({attribute: false}) accessor refusal: Refusal | null = null;
  /**
   * What the map's last click resolved to (`<tessera-map>`'s `lastPick`), shown while no item is
   * selected: `{kind: 'miss'}` shows "No item here", and a broken pick shows "Item unavailable".
   */
  @property({attribute: false}) accessor pick: PickOutcome = null;
  /** The schema for field order and view names, where the card has no store to read it from. */
  @property({attribute: false}) accessor meta: Meta | null = null;
  /** The field the headline shows. Unset, or where the item has no value for it, the headline is the `tessera_id`. */
  @property({attribute: 'title-field'}) accessor titleField = '';

  private get shown(): {item: {id: bigint; detail: ItemDetail} | null; refusal: Refusal | null; meta: Meta | null} {
    // A card fed by property still takes the schema from an adopted store: declaration order and
    // view names and frames belong to the bundle.
    if (this.item || this.refusal) return {item: this.item, refusal: this.refusal, meta: this.meta ?? this.resolvedStore?.get('meta') ?? null};
    const s = this.resolvedStore;
    if (!s) return {item: null, refusal: null, meta: this.meta};
    const sel = s.get('selection');
    return {item: sel.item, refusal: sel.itemRefusal, meta: this.meta ?? s.get('meta')};
  }

  override render(): TemplateResult | typeof nothing {
    const {item, refusal, meta} = this.shown;
    const close = html`<button part="close" type="button" aria-label="Close" @click=${() => emit(this, 'tessera-close', {what: 'item'})}>${icon('close', 14)}</button>`;
    // The header row: what the card shows, and the close button where there is something to close.
    const head = (content: unknown, closable: boolean) => html`<div part="title" class="head">${content}${closable ? close : nothing}</div>`;
    const unavailable = (code: string | null) =>
      html`<div class="panel">${head(html`<span part="state" data-state="refused"><span class="dot refuse"></span>${refusalText('Item unavailable', code)}</span>`, true)}</div>`;
    if (refusal) return unavailable(refusal.code);
    if (!item) {
      const pick = this.pick;
      // A broken pick is a fault in the layer; its details stay on the map's `lastPick`.
      if (pick?.kind === 'broken') return unavailable(null);
      if (pick?.kind === 'miss') return html`<div class="panel">${head(html`<span part="state" data-state="empty">No item here</span>`, true)}</div>`;
      const state = stateOf(this.resolvedStore?.get('status'));
      if (state === 'detached') return html`<div class="panel">${head(renderState('detached', null), false)}</div>`;
      return html`<div class="panel">${head(html`<span part="state" data-state="empty">No item selected</span>`, false)}</div>`;
    }
    const declared = meta?.declaredScalars ?? [];
    const {fields} = item.detail;
    const names = Object.keys(fields);
    const ordered = [...declared.map((c) => c.name).filter((n) => n in fields), ...names.filter((n) => !declared.some((c) => c.name === n))];
    const id = idString(item.id);
    const titleName = this.titleField && fields[this.titleField] !== undefined && fields[this.titleField] !== null ? this.titleField : null;
    const rest = ordered.filter((n) => n !== titleName);
    const copy = () => void navigator.clipboard?.writeText(id);
    return html`<div class="panel">
      ${head(
        titleName
          ? html`<div part="headline" class="card-title" data-name=${titleName}><slot name=${`field-${titleName}`}><span part="value">${present(fields[titleName], declared.find((c) => c.name === titleName))}</span></slot></div>`
          : html`<div part="headline" class="card-title mono" data-name="tessera_id">${id}</div>`,
        true
      )}
      <span part="state" data-state="shown"></span>
      ${this.views(item.detail.views, meta)}
      <div class="field">
        ${rest.map((name) => this.field(name, fields[name], declared.find((c) => c.name === name)))}
        ${titleName
          ? html`<div part="field" data-name="tessera_id" style="display:contents"><span part="label" class="k">tessera_id</span><span part="value" class="v mono">${id}</span></div>`
          : nothing}
      </div>
      ${this.scoped(item.detail.scoped)}
      <div class="row actions">
        <button part="open" class="btn" type="button" @click=${() => emit(this, 'tessera-open', {id, fields})}>${icon('open', 14)}Open</button>
        <button part="copy" class="btn" type="button" @click=${copy}>Copy id</button>
      </div>
    </div>`;
  }

  /**
   * The views this item is in that this session may reach, as chips under the title with the
   * current one marked. Clicking another follows the item there; the detail already holds its
   * position, so no request is made. An empty array draws nothing: the server does not distinguish
   * "in no view" from "in no view you can reach".
   */
  private views(positions: ItemViewPosition[], meta: Meta | null) {
    if (positions.length === 0) return nothing;
    const current = this.resolvedStore?.get('view').id ?? '';
    return html`<span part="label" class="xs muted">In views</span>
      <div class="chips">
        ${positions.map((p) => {
          const view = meta?.views.find((v) => v.id === p.id) ?? null;
          const here = p.id === current;
          return html`<button part="view-chip" class="chip" type="button" data-view=${p.id} aria-current=${here ? 'true' : 'false'} @click=${() => this.follow(p, view?.quantisation ?? null)}>
            ${view?.displayName ?? p.id}
          </button>`;
        })}
      </div>`;
  }

  /**
   * Follow the item into another view: emit `tessera-viewfollow` with the position dequantised
   * under that view's frame, in data coordinates.
   */
  private follow(position: ItemViewPosition, frame: Quantisation | null): void {
    if (!frame) return;
    emit(this, 'tessera-viewfollow', {
      view: position.id,
      x: frame.xMin + (position.x / GRID32) * (frame.xMax - frame.xMin),
      y: frame.yMin + (position.y / GRID32) * (frame.yMax - frame.yMin)
    });
  }

  /**
   * The group-scoped attribute values, headed by key in the order served. Two views that share a
   * key through a `members` group share a heading.
   */
  private scoped(scoped: Record<string, Record<string, unknown>>) {
    const keys: string[] = [];
    for (const family of Object.keys(scoped)) for (const key of Object.keys(scoped[family] ?? {})) if (!keys.includes(key)) keys.push(key);
    if (keys.length === 0) return nothing;
    return html`<div part="scoped">
      ${keys.map(
        (key) => html`<div part="key" class="hd" data-key=${key}>${key}</div>
          <div class="field" data-key=${key}>
            ${Object.keys(scoped)
              .filter((family) => key in (scoped[family] ?? {}))
              .map((family) => html`<div part="field" data-name=${family} data-key=${key} style="display:contents"><span part="label" class="k">${columnCaption(family)}</span><span part="value" class="v">${present(scoped[family]![key], undefined)}</span></div>`)}
          </div>`
      )}
    </div>`;
  }

  /** One field, presented by its declared type; prose spans the grid. */
  private field(name: string, value: unknown, column: DeclaredScalar | undefined) {
    const text = present(value, column);
    const prose = text.length > 60;
    return html`<div part="field" data-name=${name} ?data-prose=${prose} style=${prose ? nothing : 'display:contents'}>
      <span part="label" class="k">${column ? columnCaption(name) : name}</span>
      <slot name=${`field-${name}`}><span part="value" class="v" title=${prose ? text : nothing}>${text}</span></slot>
    </div>`;
  }
}

/** A value as text, by the column's declared type; a category is already its key. */
function present(value: unknown, column: DeclaredScalar | undefined): string {
  if (value === null || value === undefined) return '—';
  if (column?.arrowType === 'timestamp_us' && (typeof value === 'number' || typeof value === 'bigint')) return timestampText(value);
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
