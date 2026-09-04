import {css, html, nothing} from 'lit';
import {isFilterLayer, layerEntries, type LayerEntry} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-layer-picker>` — which annotation layers the map draws, from `meta.layers` (design
 * §5.3 tier 2, decision 0096): the LAYERS checklist of the boards, one entry per layer **with its
 * closure** — a clustering's labels are a second layer that `depends_on` it, so one entry names
 * both and the store names every layer in it in the request. Never a count of a layer's
 * artifacts: the wire carries none.
 *
 * **A filter layer is still a layer** (`highlight-and-hierarchy.md` §5.4, owner ruling
 * 2026-09-02): a layer declaring `computed = []` is listed here, in its own group and with no
 * checkbox, because there is nothing to draw and a draw toggle would offer one. It is reached
 * through `<tessera-hierarchy>` and applied as a clause. It is never named in a viewport
 * request's `layers`, which is the store's own rule and not this element's.
 */
export class TesseraLayerPicker extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='entry'] {
        display: flex;
        align-items: center;
        gap: 8px;
        height: 26px;
        cursor: pointer;
      }
      [part='group'] {
        margin: 10px 0 2px;
      }
      [part='entry'][data-filter-layer] {
        cursor: default;
        color: var(--tessera-ink-2);
        padding-left: 22px;
      }
      [part='name'] {
        font-family: var(--tessera-font-mono);
        font-size: 12px;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
    `
  ];

  private entries(): LayerEntry[] {
    return layerEntries(this.resolvedStore?.get('meta')?.layers ?? []);
  }

  /** The entries that draw, and the filter layers, in `meta` order within each. */
  private split(): {drawn: LayerEntry[]; filters: LayerEntry[]} {
    const entries = this.entries();
    return {
      drawn: entries.filter((e) => !isFilterLayer(e.root)),
      filters: entries.filter((e) => isFilterLayer(e.root))
    };
  }

  private toggle(entry: LayerEntry, on: boolean): void {
    const s = this.resolvedStore;
    if (!s) return;
    const current = new Set(s.get('artifacts').layers);
    const roots = this.entries().filter((e) => (e.root.name === entry.root.name ? on : current.has(e.root.name))).map((e) => e.root.name);
    s.setLayers(roots);
    emit(this, 'tessera-layerchange', {layers: roots});
  }

  override render() {
    const s = this.resolvedStore;
    const heading = html`<h2 part="title">Layers</h2>`;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel">${heading}${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const {drawn, filters} = this.split();
    if (drawn.length + filters.length === 0) return html`<div class="panel">${heading}<span part="state" data-state="empty">No layers</span></div>`;
    const on = new Set(s.get('artifacts').layers);
    return html`<div class="panel">${heading}
      <span part="state" data-state="shown"></span>
      <div class="col">
        ${drawn.map(
          (e) => html`<label part="entry" class="check" data-layer=${e.root.name} title=${e.closure.length > 1 ? e.closure.join(' + ') : nothing}>
            <input type="checkbox" .checked=${on.has(e.root.name)} @change=${(ev: Event) => this.toggle(e, (ev.target as HTMLInputElement).checked)} />
            <span part="name">${e.root.name}</span>
          </label>`
        )}
      </div>
      ${filters.length > 0
        ? html`<div part="group" class="xs muted">Filter layers</div>
            <div class="col">
              ${filters.map(
                (e) => html`<div part="entry" data-layer=${e.root.name} data-filter-layer title="Listed, not drawn — reached through the hierarchy panel and applied as a clause">
                  <span part="name">${e.root.name}</span>
                </div>`
              )}
            </div>`
        : nothing}
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-layer-picker', TesseraLayerPicker);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-layer-picker': TesseraLayerPicker;
  }
}
