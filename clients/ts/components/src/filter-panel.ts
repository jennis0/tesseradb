import {css, html, nothing} from 'lit';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, emptyDraft, isPopulated, type ColumnDraft} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';

/**
 * `<tessera-filter-panel>` — every operand `meta` offers, as `<tessera-filter>`s, with
 * applied-filter chips and *clear all* (design §5.3 tier 2). Which controls exist is the
 * server's answer: a column absent from `meta.filterOperands` is absent here, and a schema that
 * adds one gains its control on the next meta.
 *
 * Keyed rendering, so a store tick never rebuilds a control under the user's cursor.
 */
export class TesseraFilterPanel extends TesseraElement {
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
      [part='chips'] {
        display: flex;
        flex-wrap: wrap;
        gap: var(--tessera-space);
        margin-bottom: var(--tessera-space);
      }
      [part='chip'] {
        font-size: var(--tessera-font-size-small);
        border: 1px solid var(--tessera-accent);
        color: var(--tessera-accent);
        border-radius: var(--tessera-radius);
        padding: 0 calc(var(--tessera-space) * 0.8);
      }
      [part='clear'] {
        width: 100%;
      }
    `
  ];

  private chipText(column: string, draft: ColumnDraft): string {
    switch (draft.family) {
      case 'text':
        return `${column}: ${draft.mode === 'phrase' ? '“' + draft.query + '”' : draft.query}`;
      case 'string':
      case 'keyword':
        return `${column} ${draft.op} ${draft.needle}`;
      case 'category':
        return `${column}: ${draft.keys.join(', ')}`;
      case 'numeric':
        return `${column}: ${draft.gte ?? '…'} – ${draft.lte ?? '…'}`;
    }
  }

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<h2 part="title">Filters</h2><span part="state" data-state="empty"><span class="muted">this bundle declares nothing filterable</span></span>`;
    const draft = s.get('filters').draft;
    const active = activeCount(draft);
    const chips = Object.entries(draft).filter(([, d]) => isPopulated(d));
    return html`<h2 part="title">Filters${active > 0 ? ` · ${active} active` : ''}</h2>
      <span part="state" data-state="shown"></span>
      ${chips.length > 0 ? html`<div part="chips">${chips.map(([c, d]) => html`<span part="chip">${this.chipText(c, d)}</span>`)}</div>` : nothing}
      ${repeat(
        operands,
        (o) => o.column,
        (o) => html`<tessera-filter column=${o.column} .store=${s}></tessera-filter>`
      )}
      ${active > 0
        ? html`<button part="clear" type="button" @click=${() => {
            const empty = emptyDraft(meta.filterOperands);
            s.setFilters(empty);
            emit(this, 'tessera-filterchange', {column: null, expr: null});
          }}>clear ${active}</button>`
        : nothing}`;
  }
}

attachContextRoot();
defineOnce('tessera-filter-panel', TesseraFilterPanel);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter-panel': TesseraFilterPanel;
  }
}
