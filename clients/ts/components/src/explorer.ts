import {ContextProvider} from '@lit/context';
import {css, html, nothing} from 'lit';
import {property, state} from 'lit/decorators.js';
import type {Store} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import type {TesseraMap} from './map.js';
import {chrome, tokens} from './tokens.js';
import './map.js';
import './status.js';
import './filter-panel.js';
import './item-card.js';
import './selection.js';
import './layer-picker.js';
import './artifact-list.js';
import './artifact-card.js';
import './legend.js';

/**
 * `<tessera-explorer>` — the map with its status strip, filters, the selection panel and the
 * detail card, in a default layout (design client-components §5.3 tier 0, §5.5). Constructs its
 * own store from `viewer-url` and `token` or an `authorise` property, or takes a `.store`; it is
 * itself the context provider for its pieces, so a host puts an element in a slot and it reads
 * the same store.
 *
 * `layout="docked"` (map beside a sidebar — the default, D2a) or `layout="overlay"` (map
 * full-bleed, panels floating — the demo's look). Under a narrow container the sidebar becomes a
 * sheet behind a button, by container query. `panels="filters selection detail"` chooses which
 * appear; every region is a named slot with default content.
 *
 * The `toolbar` slot holds the layer picker and the selectable legend, `legend` the legend's
 * readout, `artifacts` the artifact list, and `detail` whichever of the item card and the
 * artifact card changed last (§5.3) — a point inside a cluster makes both meaningful, and a host
 * placing the two separately shows both.
 */
const ALL_PANELS = ['toolbar', 'legend', 'filters', 'artifacts', 'selection', 'detail'] as const;
type Panel = (typeof ALL_PANELS)[number];

export class TesseraExplorer extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        container-type: inline-size;
        container-name: explorer;
        background: var(--tessera-bg);
        --tessera-sidebar-width: 320px;
        height: var(--tessera-explorer-height, 100%);
        min-height: 320px;
      }
      [part='frame'] {
        position: relative;
        display: grid;
        height: 100%;
        min-height: inherit;
      }
      :host([layout='docked']) [part='frame'] {
        grid-template-columns: minmax(0, 1fr) var(--tessera-sidebar-width);
      }
      :host([layout='overlay']) [part='frame'] {
        grid-template-columns: minmax(0, 1fr);
      }
      tessera-map,
      ::slotted(tessera-map) {
        --tessera-map-height: 100%;
        height: 100%;
        min-height: 320px;
      }
      [part='sidebar'] {
        display: flex;
        flex-direction: column;
        gap: var(--tessera-space);
        padding: var(--tessera-space);
        overflow-y: auto;
        border-left: 1px solid var(--tessera-border);
        background: var(--tessera-bg);
      }
      :host([layout='overlay']) [part='sidebar'] {
        position: absolute;
        top: var(--tessera-space);
        right: var(--tessera-space);
        bottom: var(--tessera-space);
        width: var(--tessera-sidebar-width);
        max-width: calc(100% - 2 * var(--tessera-space));
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
        background: transparent;
        pointer-events: none;
        z-index: 5;
      }
      :host([layout='overlay']) [part='sidebar'] > * {
        pointer-events: auto;
      }
      [part='sheet-toggle'] {
        display: none;
        position: absolute;
        z-index: 6;
        bottom: var(--tessera-space);
        right: var(--tessera-space);
      }
      /* A narrow container: the sidebar becomes a sheet behind a button. */
      @container explorer (max-width: 720px) {
        :host([layout='docked']) [part='frame'],
        :host([layout='overlay']) [part='frame'] {
          grid-template-columns: minmax(0, 1fr);
        }
        [part='sidebar'] {
          position: absolute !important;
          inset: auto 0 0 0 !important;
          width: auto !important;
          max-width: none !important;
          max-height: 70%;
          border-radius: var(--tessera-radius) var(--tessera-radius) 0 0;
          background: var(--tessera-bg) !important;
          pointer-events: auto !important;
          transform: translateY(100%);
          transition: transform 160ms ease-out;
        }
        [part='sidebar'][data-open] {
          transform: none;
        }
        [part='sheet-toggle'] {
          display: inline-block;
        }
      }
    `
  ];

  protected override canBuildOwn = true;
  @property({reflect: true}) accessor layout: 'docked' | 'overlay' = 'docked';
  @property() accessor panels = ALL_PANELS.join(' ');
  @property({attribute: 'colour-by'}) accessor colourBy = '';
  @property() accessor layers = '';
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  @property({type: Number}) accessor budget = 0;
  @state() accessor sheetOpen = false;

  private provider = new ContextProvider(this, {context: storeContext, initialValue: null});
  /** Which of the two selections changed last — what the detail region shows. */
  private lastDetail: 'item' | 'artifact' = 'item';
  private seenItem: object | null = null;
  private seenArtifact: object | null = null;

  protected override onStoreAdopted(store: Store): void {
    this.provider.setValue(store);
  }

  protected override onStoreChange(): void {
    const sel = this.resolvedStore?.get('selection');
    if (sel) {
      if (sel.item && sel.item !== this.seenItem) this.lastDetail = 'item';
      if (sel.artifact && sel.artifact !== this.seenArtifact) this.lastDetail = 'artifact';
      if (sel.artifactRefusal && !sel.artifact) this.lastDetail = 'artifact';
      this.seenItem = sel.item;
      this.seenArtifact = sel.artifact;
    }
    super.onStoreChange();
  }

  override dispose(): void {
    this.map?.dispose();
    super.dispose();
    this.provider.setValue(null);
  }

  /** The map this explorer renders, for a host that wants `fit`, `fitTo`, `select` or the probe. */
  get map(): TesseraMap | null {
    return this.renderRoot?.querySelector<TesseraMap>('tessera-map') ?? null;
  }

  private has(panel: Panel): boolean {
    return this.panels.split(/[\s,]+/).includes(panel);
  }

  override render() {
    const s = this.resolvedStore;
    const region = s?.get('region') ?? null;
    const selection = s?.get('selection');
    // The detail region shows whichever changed last.
    const showArtifact = this.lastDetail === 'artifact' && (selection?.artifact || selection?.artifactRefusal);
    const detail = html`<slot name="detail">${showArtifact ? html`<tessera-artifact-card></tessera-artifact-card>` : html`<tessera-item-card .pick=${this.map?.lastPick ?? null}></tessera-item-card>`}</slot>`;
    const toolbar = html`<slot name="toolbar"><tessera-layer-picker></tessera-layer-picker><tessera-legend selectable></tessera-legend></slot>`;
    const sidebar = html`<aside part="sidebar" ?data-open=${this.sheetOpen}>
      ${this.has('toolbar') && this.layout === 'docked' ? toolbar : nothing}
      ${this.has('legend') && this.layout === 'docked' ? html`<slot name="legend"><tessera-legend></tessera-legend></slot>` : nothing}
      ${this.has('filters') ? html`<slot name="filters"><tessera-filter-panel></tessera-filter-panel></slot>` : nothing}
      ${this.has('artifacts') ? html`<slot name="artifacts"><tessera-artifact-list></tessera-artifact-list></slot>` : nothing}
      ${this.has('selection') && region ? html`<slot name="selection"><tessera-selection></tessera-selection></slot>` : nothing}
      ${this.has('detail') ? detail : nothing}
    </aside>`;
    return html`<div part="frame" @tessera-artifactselect=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))} @tessera-artifactfit=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))}>
      <tessera-map
        colour-by=${this.colourBy || nothing}
        layers=${this.layers || nothing}
        tooltip-fields=${this.tooltipFields}
        budget=${this.budget || nothing}
        @tessera-pick=${() => this.requestUpdate()}
        @tessera-hover=${() => nothing}
        @click=${() => this.requestUpdate()}
      >
        <div slot="top-left">
          ${this.has('toolbar') && this.layout === 'overlay' ? toolbar : nothing}
        </div>
        <div slot="bottom-left"><slot name="status"><tessera-status></tessera-status></slot></div>
        <div slot="bottom-right">
          ${this.has('legend') && this.layout === 'overlay' ? html`<slot name="legend"><tessera-legend></tessera-legend></slot>` : nothing}
        </div>
        <slot name="tooltip" slot="tooltip"></slot>
      </tessera-map>
      ${sidebar}
      <button part="sheet-toggle" type="button" aria-expanded=${this.sheetOpen} @click=${() => (this.sheetOpen = !this.sheetOpen)}>
        ${this.sheetOpen ? 'close' : 'panels'}${selection?.item ? ' ·' : ''}
      </button>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-explorer', TesseraExplorer);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-explorer': TesseraExplorer;
  }
}
