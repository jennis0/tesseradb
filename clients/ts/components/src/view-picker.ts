import {css, html, nothing, type PropertyValues} from 'lit';
import {enterGroup, isTrivialViewSet, viewPickerEntries, type Meta} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {switchView} from './view-switch.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-view-picker>` — which **layout** (`view-switching.md` §6.1): one entry per plain view
 * and one per group, in `/v1/meta`'s serving order, at the top of the explorer's toolbar.
 *
 * The rules are `@tesseradb/client`'s: `viewPickerEntries` for the entries and their order,
 * `enterGroup` for which view of a chosen group is entered, `isTrivialViewSet` for the hiding
 * rule. What this element adds is the control the boards draw, the memory of the key it last left
 * each group on, and the switch.
 *
 * **Renders nothing** — not an empty select — where the bundle offers one entry, which is every
 * demo corpus today; the toolbar then looks exactly as it did.
 *
 * Restyling is the parts a host already knows from the legend: `part="select"` is the same control
 * in the same chrome, `part="label"` its caption, `part="field"` the pair.
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

  /** The key this element last left each group on — the second rung of `enterGroup`. */
  private lastKey = new Map<string, string>();
  private chosen = '';

  /**
   * The chosen option, written to the select **after** its options exist.
   *
   * A `selected` attribute toggled on a re-render does not move a select whose value has already
   * been set once — the element's own dirtiness rule — so the closed control would keep showing
   * the layout the user switched away from. The attribute is still rendered, for the first paint.
   */
  protected override updated(_changed: PropertyValues<this>): void {
    const select = this.renderRoot.querySelector('select');
    if (select && select.value !== this.chosen) select.value = this.chosen;
  }

  private choose(meta: Meta, value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    const currentId = s.get('view').id;
    // Leaving a group records the key it was left on, for the day it is chosen again.
    const roster = meta.views.find((v) => v.id === currentId)?.roster ?? null;
    if (roster) this.lastKey.set(roster.group, roster.key);
    if (value.startsWith('v:')) {
      switchView(this, s, meta, value.slice(2));
      return;
    }
    const group = value.slice(2);
    const target = enterGroup(meta, group, currentId, this.lastKey.get(group));
    if (target) switchView(this, s, meta, target);
  }

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta || isTrivialViewSet(meta)) return nothing;
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
