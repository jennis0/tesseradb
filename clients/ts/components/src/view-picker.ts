import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {type Meta} from '@tesseradb/client';
import {enterGroup, hasOneLayout, viewPickerEntries} from '@tesseradb/client/internal';
import {TesseraElement} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {switchView} from './view-switch.js';
import {chrome, tokens} from './tokens.js';

/**
 * A select over the views the viewer may reach, one entry per plain view and one per view group,
 * in `/v1/meta`'s order. Choosing a group enters it under the current view's key where the two
 * groups share keys, else at the key this element last left it on, else at its first view. Renders
 * nothing where the bundle offers one entry.
 *
 * @summary Chooses the view.
 * @tagname tessera-view-picker
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewswitch']>} tessera-viewswitch - The view
 *   changed.
 * @csspart field - The caption and the select.
 * @csspart label - The caption.
 * @csspart select - The select.
 */
export class TesseraViewPicker extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='field'] {
        display: flex;
        flex-direction: column;
        gap: 4px;
        padding: 14px 16px 0;
      }
    `
  ];

  /** The key this element last left each group on, which `enterGroup` prefers. */
  private lastKey = new Map<string, string>();
  private chosen = '';

  /**
   * Writes the chosen option to the select after its options exist. Toggling `selected` on a
   * re-render does not move a select whose value has been set once.
   */
  protected override updated(_changed: PropertyValues<this>): void {
    const select = this.renderRoot.querySelector('select');
    if (select && select.value !== this.chosen) select.value = this.chosen;
  }

  private choose(meta: Meta, value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    const currentId = s.get('view').id;
    // Leaving a group records the key it was left on.
    const roster = meta.views.find((v) => v.id === currentId)?.roster ?? null;
    if (roster) this.lastKey.set(roster.group, roster.key);
    const group = value.startsWith('g:') ? value.slice(2) : null;
    const target = group === null ? value.slice(2) : enterGroup(meta, group, currentId, this.lastKey.get(group));
    // A choice that switches nothing (a group with no view this session may reach, or the view
    // already current) puts the select back to the layout the map is drawing.
    if (target === null || target === currentId) {
      this.restoreSelect();
      return;
    }
    switchView(this, s, meta, target);
  }

  /** Put the select back to the entry the store is in. */
  private restoreSelect(): void {
    const select = this.renderRoot.querySelector('select');
    if (select && select.value !== this.chosen) select.value = this.chosen;
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta || hasOneLayout(meta)) return nothing;
    const entries = viewPickerEntries(meta, s.get('view').id);
    const value = (entry: {kind: 'view' | 'group'; id: string}) => `${entry.kind === 'group' ? 'g' : 'v'}:${entry.id}`;
    const current = entries.find((e) => e.current);
    this.chosen = current ? value(current) : '';
    return html`<div part="field">
      <span part="label" class="xs muted">View</span>
      <select part="select" aria-label="View" @change=${(e: Event) => this.choose(meta, (e.target as HTMLSelectElement).value)}>
        ${entries.map(
          (entry) => html`<option value=${value(entry)} data-kind=${entry.kind} ?selected=${entry.current}>${entry.text}</option>`
        )}
      </select>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-view-picker', TesseraViewPicker);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-view-picker': TesseraViewPicker;
  }
}
