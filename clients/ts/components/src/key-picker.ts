import {css, html, nothing, type PropertyValues} from 'lit';
import {stepView, viewLabel, viewsOfGroup, type Meta, type ViewInfo} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {switchView} from './view-switch.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-key-picker>`: which view of the current group, as a select over the group's roster with
 * previous and next buttons. Renders nothing when the current view is in no group.
 *
 * The roster is in creation order (`viewsOfGroup`), since a key is the caller's own string and its
 * sort order means nothing. The buttons are `stepView(meta, current, ∓1)`, disabled at the ends
 * without wrapping. Each entry shows `viewLabel`'s label and the key, which is the address a link
 * or request carries.
 *
 * `part="select"` is the control, `part="label"` its caption, `part="entry"` the row and
 * `part="step"` each button, with `data-direction="prev"` or `"next"`.
 */
export class TesseraKeyPicker extends TesseraElement {
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
        padding: 10px 16px 0;
      }
      [part='entry'] {
        gap: 6px;
      }
      [part='step'] {
        width: 30px;
        padding: 0;
        justify-content: center;
        flex: none;
      }
      [part='step'] span {
        display: inline-flex;
      }
      [part='step'][data-direction='prev'] span {
        transform: rotate(180deg);
      }
      [part='select'] {
        flex: 1 1 0;
        min-width: 0;
      }
    `
  ];

  private chosen = '';

  /** Writes the chosen option after the options exist; a re-rendered `selected` does not move a dirty select. */
  protected override updated(_changed: PropertyValues<this>): void {
    const select = this.renderRoot.querySelector('select');
    if (select && select.value !== this.chosen) select.value = this.chosen;
  }

  /**
   * One roster entry's text: the label and the key, as in `Jul – Sep 2026 · 2026-Q3`. An `<option>`
   * holds only text, so the key cannot be styled apart.
   */
  private option(meta: Meta, view: ViewInfo): string {
    const {label, key} = viewLabel(meta, view);
    if (key === null) return view.displayName;
    return label === null ? key : `${label} · ${key}`;
  }

  private step(meta: Meta, from: string, by: -1 | 1): void {
    const s = this.resolvedStore;
    const next = stepView(meta, from, by);
    if (s && next) switchView(this, s, meta, next.id);
  }

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return nothing;
    const current = meta.views.find((v) => v.id === s.get('view').id) ?? null;
    const roster = current?.roster ?? null;
    if (!current || !roster) return nothing;
    const views = viewsOfGroup(meta, roster.group);
    // The caption is the group's name, the key's namespace (`quarter` over `2026-Q3`); the view
    // picker above already shows the title.
    const heading = roster.group;
    const previous = stepView(meta, current.id, -1);
    const next = stepView(meta, current.id, 1);
    this.chosen = current.id;
    const step = (direction: 'prev' | 'next', to: ViewInfo | null, by: -1 | 1, label: string) =>
      html`<button part="step" data-direction=${direction} class="btn" type="button" aria-label=${label} ?disabled=${to === null} @click=${() => this.step(meta, current.id, by)}>
        <span>${icon('chevr', 14)}</span>
      </button>`;
    return html`<div part="field">
      <span part="label" class="xs muted">${heading}</span>
      <div part="entry" class="row">
        ${step('prev', previous, -1, 'Previous')}
        <select part="select" aria-label=${heading} @change=${(e: Event) => switchView(this, s, meta, (e.target as HTMLSelectElement).value)}>
          ${views.map((v) => html`<option value=${v.id} ?selected=${v.id === current.id}>${this.option(meta, v)}</option>`)}
        </select>
        ${step('next', next, 1, 'Next')}
      </div>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-key-picker', TesseraKeyPicker);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-key-picker': TesseraKeyPicker;
  }
}
