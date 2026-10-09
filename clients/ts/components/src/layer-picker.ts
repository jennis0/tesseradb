import {css, html, nothing, type TemplateResult} from 'lit';
import {isFilterLayer, layerEntries, type LayerEntry} from '@mosaicajs/client';
import {MosaicaElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * A checkbox per annotation layer in `meta.layers`, choosing which layers the map draws. A layer
 * that others depend on (a clustering and its labels) is one entry that turns on the whole group;
 * the entry's tooltip names the layers in it. A filter layer has nothing to draw and gets no
 * checkbox: a quiet note beneath the list names the filter layers, which are applied as clauses
 * through `<mosaica-hierarchy>`.
 *
 * @summary Which annotation layers the map draws.
 * @tagname mosaica-layer-picker
 * @category Elements
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-layerchange']>} mosaica-layerchange - A checkbox
 *   changed, with the layers now drawn.
 * @csspart title - The heading.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart entry - One layer that draws, with `data-layer`.
 * @csspart name - A layer's title, or its name where it declares none.
 * @csspart note - The note naming the filter layers, such as "Venues can be used as a filter
 *   only.", with `data-layers` listing their names.
 */
export class MosaicaLayerPicker extends MosaicaElement {
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
        min-height: 28px;
        cursor: pointer;
      }
      [part='note'] {
        margin: 8px 0 0;
        font-size: 12px;
        color: var(--_mosaica-ink-3);
      }
      [part='name'] {
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
    emit(this, 'mosaica-layerchange', {layers: roots});
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
            <span part="name">${e.root.title || e.root.name}</span>
          </label>`
        )}
      </div>
      ${filters.length > 0
        ? html`<p part="note" data-layers=${filters.map((e) => e.root.name).join(' ')}>${filterNote(filters.map((e) => e.root.title || e.root.name))}</p>`
        : nothing}
    </div>`;
  }
}

/** "Venues can be used as a filter only.", or for several, "Venues and Countries can be used as filters only." */
function filterNote(titles: string[]): string {
  if (titles.length === 1) return `${titles[0]} can be used as a filter only.`;
  return `${titles.slice(0, -1).join(', ')} and ${titles.at(-1)} can be used as filters only.`;
}

attachContextRoot();
defineOnce('mosaica-layer-picker', MosaicaLayerPicker);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-layer-picker': MosaicaLayerPicker;
  }
}
