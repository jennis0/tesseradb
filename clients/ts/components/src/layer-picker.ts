import {css, html, nothing, type TemplateResult} from 'lit';
import {isFilterLayer, layerEntries, type LayerEntry} from '@tesseradb/client/internal';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-layer-picker>`: which annotation layers the map draws, from `meta.layers`. One entry
 * per layer with its closure: a clustering's labels are a layer that `depends_on` it, so one entry
 * covers both. No artifact count is shown; the server serves none per layer.
 *
 * A filter layer (one declaring `computed = []`) is listed in its own group without a checkbox,
 * since it has nothing to draw. It is applied as a clause through `<tessera-hierarchy>`.
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
        color: var(--_tessera-ink-2);
        padding-left: 22px;
      }
      [part='name'] {
        font-family: var(--_tessera-font-mono);
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

  override render(): TemplateResult | typeof nothing {
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
                (e) => html`<div part="entry" data-layer=${e.root.name} data-filter-layer title="Listed, not drawn. Reached through the hierarchy panel and applied as a clause">
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
