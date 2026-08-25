import {css, html, nothing} from 'lit';
import {layerEntries, type LayerEntry} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-layer-picker>` — which annotation layers the map draws, from `meta.layers` (design
 * §5.3 tier 2, decision 0096). One entry per layer **with its closure**: a clustering's labels
 * are a second layer that `depends_on` it, so the entry names both and the store names every
 * layer in it in the request. Usually one is on; several only for different kinds of feature.
 *
 * The list is gate-filtered by the server, so it is the whole of what this principal may know
 * exists; a layer they cannot reach is absent by the route a never-registered one takes. Never
 * a count of a layer's artifacts: the wire carries none.
 */
export class TesseraLayerPicker extends TesseraElement {
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
      [part='entry'] {
        display: flex;
        align-items: baseline;
        gap: var(--tessera-space);
        cursor: pointer;
      }
      [part='entry'] input {
        margin: 0;
      }
      [part='closure'] {
        color: var(--tessera-fg-muted);
        font-size: var(--tessera-font-size-small);
      }
    `
  ];

  private entries(): LayerEntry[] {
    return layerEntries(this.resolvedStore?.get('meta')?.layers ?? []);
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
    if (!s || !meta) return html`${heading}${renderState(stateOf(s?.get('status')), s?.get('status'))}`;
    const entries = this.entries();
    if (entries.length === 0) {
      return html`${heading}<span part="state" data-state="empty"><span class="muted">this principal reaches no layer here — which is also what a deployment with none looks like</span></span>`;
    }
    const on = new Set(s.get('artifacts').layers);
    return html`${heading}
      <span part="state" data-state="shown"></span>
      ${entries.map((e) => {
        const rest = e.closure.filter((n) => n !== e.root.name);
        return html`<label part="entry" data-layer=${e.root.name}>
          <input type="checkbox" .checked=${on.has(e.root.name)} @change=${(ev: Event) => this.toggle(e, (ev.target as HTMLInputElement).checked)} />
          <span part="name">${e.root.title || e.root.name}</span>
          ${rest.length > 0 ? html`<span part="closure">with ${rest.map((n) => meta.layers.find((l) => l.name === n)?.title || n).join(', ')}</span>` : nothing}
        </label>`;
      })}
      <div class="muted">how many artifacts a layer holds is never published: what you reach of one is answered artifact by artifact, by the viewport</div>`;
  }
}

attachContextRoot();
defineOnce('tessera-layer-picker', TesseraLayerPicker);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-layer-picker': TesseraLayerPicker;
  }
}
