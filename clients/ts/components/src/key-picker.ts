import {css, html, nothing, type PropertyValues} from 'lit';
import {stepView, viewLabel, viewsOfGroup, type Meta, type ViewInfo} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {switchView} from './view-switch.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-key-picker>` — which view of the current group (`view-switching.md` §6.2): a select
 * over the group's roster with previous and next beside it, under the view picker in the toolbar.
 *
 * **Creation order, never key order** (`views.md` §3.2, decision 0113): a key is the caller's own
 * string and means nothing to a client, so the roster is `viewsOfGroup`'s list and the two buttons
 * are `stepView(meta, current, ∓1)` — disabled at the ends and **never wrapping**. Left and right
 * arrow keys on the focused select are the native behaviour and are the slider; §4 makes a run of
 * them cost one request.
 *
 * A view's label is `viewLabel`'s, which reads the roster metadata by the rule in
 * `@tesseradb/client` and resolves a `members` group's views through `membersOf`. The key is drawn
 * with it: the key is the address a link or a request carries, and a user should be able to read
 * it off the screen.
 *
 * Renders nothing when the current view is plain: a plain view is in no group and has no
 * neighbours.
 *
 * Restyling is the parts a host already knows: `part="select"` in the legend's own chrome,
 * `part="label"` its caption, `part="entry"` the row, `part="step"` each button — with
 * `data-direction="prev"` and `"next"`, so the two are addressable apart.
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

  /** See `<tessera-view-picker>`: a re-rendered `selected` attribute does not move a dirty select. */
  protected override updated(_changed: PropertyValues<this>): void {
    const select = this.renderRoot.querySelector('select');
    if (select && select.value !== this.chosen) select.value = this.chosen;
  }

  /**
   * What one roster entry reads as: the label the group's metadata makes, and the key with it —
   * `Jul – Sep 2026 · 2026-Q3`.
   *
   * The key follows the label as **text inside the option** rather than as a muted run beside it,
   * which is what the board draws: an `<option>` holds text and no element, so a native select
   * cannot carry a second style inside its closed state. Everything else about the control — the
   * chrome, the keyboard, the platform's own list — is what a native select gives, and the legend's
   * selects directly beneath it are the same control.
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
    // **The caption is the group's `name`, not its title** (owner ruling, 2026-09-01): the layout
    // picker directly above already shows the title, and a caption repeating it reads as the same
    // words twice. The name is the key's namespace — `quarter` over `2026-Q3` — which is what a
    // reader needs to know the key beneath it belongs to.
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
