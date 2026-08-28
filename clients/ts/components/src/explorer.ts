import {ContextProvider} from '@lit/context';
import {css, html, nothing} from 'lit';
import {property, state} from 'lit/decorators.js';
import type {Store} from '@tesseradb/client';
import {activeCount, artifactBudgetFor, emptyDraft, levelForBudget} from '@tesseradb/client';
import {clusterLayerOf} from '@tesseradb/deck';
import {TesseraElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
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
 * `<tessera-explorer>` — the map with its status strip, toolbar, layer picker, filters, artifact
 * list and the detail card, in a default layout (design client-components §5.3 tier 0, §5.5),
 * laid out as the boards draw it. Constructs its own store from `viewer-url` and `token` or an
 * `authorise` property, or takes a `.store`; it is itself the context provider for its pieces.
 *
 * `layout="docked"` (`Main.png`): the map beside a sidebar — *Colour by* and *Layers* at the top,
 * the LAYERS checklist, the item or cluster card, then FILTERS and IN VIEW as collapsed sections
 * with a summary each. `layout="overlay"` (`ExplorerOverlay.png`): the map full-bleed, a floating
 * panel top-left with the selects, the layers and the filters, the toolbar top-right, and a
 * floating panel at the right with IN VIEW and the card. Under a narrow container
 * (`ExplorerNarrow.png`) the strip runs full width above a tab bar — Filters, Layers, In view,
 * Item — and each tab opens its panel as a sheet. `panels="filters legend layers artifacts
 * detail"` chooses which appear; every region is a named slot with default content.
 */
const ALL_PANELS = ['toolbar', 'legend', 'filters', 'artifacts', 'selection', 'detail'] as const;
type Panel = (typeof ALL_PANELS)[number];
type Sheet = 'filters' | 'layers' | 'artifacts' | 'detail';

export class TesseraExplorer extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        container-type: inline-size;
        container-name: explorer;
        background: var(--tessera-surface);
        --tessera-sidebar-width: 336px;
        height: var(--tessera-explorer-height, 100%);
        min-height: 320px;
      }
      [part='frame'] {
        position: relative;
        display: grid;
        height: 100%;
        min-height: inherit;
        overflow: hidden;
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
        overflow-y: auto;
        border-left: 1px solid var(--tessera-line);
        background: var(--tessera-surface);
      }
      [part='sidebar'] > *:last-child {
        border-bottom: 0;
      }
      /* Overlay: two floating columns over the map. */
      .float {
        position: absolute;
        top: 12px;
        width: 320px;
        /* Room beneath for the strip, which is always in view. */
        max-height: calc(100% - 72px);
        display: flex;
        flex-direction: column;
        gap: 10px;
        z-index: 5;
        pointer-events: none;
      }
      .float.left {
        left: 12px;
      }
      .float.right {
        right: 12px;
        /* Below the map's toolbar, which sits top-right in this layout. */
        top: 166px;
        max-height: calc(100% - 226px);
        align-items: flex-end;
      }
      .float > * {
        pointer-events: auto;
      }
      .card {
        width: 100%;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        overflow-y: auto;
        max-height: 100%;
      }
      .card > *:last-child {
        border-bottom: 0;
      }
      .card ::part(panel) {
        border-bottom: 1px solid var(--tessera-line-2);
      }
      /* Collapsed sections. */
      details {
        border-bottom: 1px solid var(--tessera-line-2);
      }
      details > summary {
        list-style: none;
        cursor: pointer;
        padding: 14px 16px;
        display: flex;
        align-items: center;
        justify-content: space-between;
        font-size: 11px;
        font-weight: 600;
        letter-spacing: 0.06em;
        text-transform: uppercase;
        color: var(--tessera-ink-2);
      }
      details > summary::-webkit-details-marker {
        display: none;
      }
      details > summary .t {
        display: flex;
        align-items: center;
        gap: 6px;
      }
      details > summary .summary {
        text-transform: none;
        letter-spacing: 0;
        font-weight: 400;
        color: var(--tessera-ink-3);
      }
      details[open] > summary {
        padding-bottom: 0;
      }
      details > summary .open-chev {
        display: none;
      }
      details[open] > summary .open-chev {
        display: inline-flex;
      }
      details[open] > summary .closed-chev {
        display: none;
      }
      details > .body > * {
        border-bottom: 0;
      }
      /* The narrow container: the strip full width, a tab bar, sheets. */
      [part='tabs'] {
        display: none;
      }
      @container explorer (max-width: 720px) {
        :host([layout='docked']) [part='frame'],
        :host([layout='overlay']) [part='frame'] {
          grid-template-columns: minmax(0, 1fr);
          grid-template-rows: minmax(0, 1fr) auto;
        }
        [part='sidebar'],
        .float {
          display: none;
        }
        [part='tabs'] {
          display: flex;
          height: 56px;
          border-top: 1px solid var(--tessera-line);
          background: var(--tessera-surface);
        }
        [part='tabs'] button {
          flex: 1;
          display: flex;
          flex-direction: column;
          align-items: center;
          justify-content: center;
          gap: 3px;
          color: var(--tessera-ink-2);
          font-size: 11px;
          font-weight: 500;
        }
        [part='tabs'] button[aria-selected='true'] {
          color: var(--tessera-accent);
          font-weight: 600;
        }
        [part='sheet'] {
          position: absolute;
          left: 0;
          right: 0;
          bottom: 56px;
          max-height: 70%;
          overflow-y: auto;
          background: var(--tessera-surface);
          border-top: 1px solid var(--tessera-line);
          border-radius: 12px 12px 0 0;
          box-shadow: var(--tessera-shadow);
          z-index: 6;
        }
        .sheet-footer {
          position: sticky;
          bottom: 0;
          display: flex;
          gap: 10px;
          padding: 10px 16px;
          border-top: 1px solid var(--tessera-line-2);
          background: var(--tessera-surface);
        }
        .sheet-footer .btn {
          height: 44px;
          flex: 1;
          justify-content: center;
        }
        .sheet-footer .btn.primary {
          flex: 2;
        }
        [part='sheet']::before {
          content: '';
          display: block;
          width: 36px;
          height: 4px;
          border-radius: 2px;
          background: var(--tessera-line);
          margin: 8px auto 0;
        }
        .narrow-strip {
          display: block;
        }
        [part='strip-row'] tessera-status {
          display: block;
        }
        [part='strip-row'] tessera-status::part(strip) {
          display: flex;
          width: 100%;
          box-shadow: none;
          border-radius: 0;
          border-width: 1px 0 0;
          height: 44px;
          overflow-x: auto;
        }
        .in-map-strip {
          display: none;
        }
      }
      [part='strip-row'] {
        display: none;
      }
      @container explorer (max-width: 720px) {
        [part='strip-row'] {
          display: block;
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
  @state() accessor sheet: Sheet | null = null;
  /** The level chosen through the legend's select; the map colours and labels at it. */
  @state() accessor level: number | null = null;

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
    const meta = s?.get('meta') ?? null;
    const active = s ? activeCount(s.get('filters').draft) : 0;
    const artifacts = s?.get('artifacts');
    const inView = artifacts && artifacts.status === 'shown' ? (artifacts.lineage.linked ? artifacts.lineage.roots.length : artifacts.served.length) : 0;
    // The level drawn: the one chosen through the legend, else — for a tiered layer the server
    // served whole — the level the view's budget would have cut at (design §6), else the deepest
    // served. The legend shows which; the map colours, outlines and labels at it.
    const autoLevel = this.autoLevel();
    const level = this.level ?? autoLevel;
    const hasDetail = Boolean(selection?.item || selection?.artifact || selection?.artifactRefusal || selection?.itemRefusal || this.map?.lastPick);
    // The detail region shows whichever changed last.
    const showArtifact = this.lastDetail === 'artifact' && (selection?.artifact || selection?.artifactRefusal);
    const detail = html`<slot name="detail">${showArtifact ? html`<tessera-artifact-card></tessera-artifact-card>` : html`<tessera-item-card .pick=${this.map?.lastPick ?? null}></tessera-item-card>`}</slot>`;
    const toolbar = html`<slot name="toolbar"><tessera-legend selectable .level=${this.level} .autoLevel=${autoLevel} @tessera-levelchange=${(e: CustomEvent<{level: number | null}>) => (this.level = e.detail.level)}></tessera-legend></slot>`;
    const layersPanel = html`<slot name="layers"><tessera-layer-picker></tessera-layer-picker></slot>`;
    const filters = html`<slot name="filters"><tessera-filter-panel></tessera-filter-panel></slot>`;
    const list = html`<slot name="artifacts"><tessera-artifact-list></tessera-artifact-list></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><tessera-selection></tessera-selection></slot>` : nothing;
    const section = (name: IconName, title: string, summary: string, body: unknown, open = false) =>
      html`<details ?open=${open}><summary><span class="t"><span class="closed-chev">${icon('chevr', 14)}</span><span class="open-chev">${icon('chev', 14)}</span>${title}</span><span class="summary">${summary}</span></summary><div class="body" data-section=${name}>${body}</div></details>`;

    const docked = html`<aside part="sidebar" aria-label="Explorer panels">
      ${this.has('toolbar') ? toolbar : nothing}
      ${this.has('legend') ? layersPanel : nothing}
      ${selectionPanel}
      ${this.has('detail') && hasDetail ? detail : nothing}
      ${this.has('filters') ? section('filter', 'Filters', active > 0 ? `${active} applied` : '', filters) : nothing}
      ${this.has('artifacts') ? section('list', 'In view', inView > 0 ? `${inView.toLocaleString('en-GB')} cluster${inView === 1 ? '' : 's'}` : '', list) : nothing}
    </aside>`;

    const overlayLeft = html`<div class="float left">
      <div class="card">
        ${this.has('toolbar') ? toolbar : nothing}
        ${this.has('legend') ? layersPanel : nothing}
        ${this.has('filters') ? filters : nothing}
      </div>
    </div>`;
    const overlayRight = html`<div class="float right">
      <slot name="top-right"></slot>
      ${this.has('artifacts') || (this.has('detail') && hasDetail) || selectionPanel !== nothing
        ? html`<div class="card">${selectionPanel}${this.has('artifacts') ? list : nothing}${this.has('detail') && hasDetail ? detail : nothing}</div>`
        : nothing}
    </div>`;

    const tab = (name: Sheet, ic: IconName, label: string) =>
      html`<button type="button" role="tab" aria-selected=${this.sheet === name ? 'true' : 'false'} @click=${() => (this.sheet = this.sheet === name ? null : name)}>${icon(ic, 18)}${label}</button>`;
    // The filters sheet's primary action names the number it will produce (`ExplorerNarrow.png`).
    const matched = s?.get('view').matched;
    const matchedText = matched && matched.exact && s?.get('status').status === 'shown' ? `Show ${matched.value.toLocaleString('en-GB')} matched` : 'Show';
    const sheetFooter = html`<div class="sheet-footer">
      <button class="btn" type="button" @click=${() => {
        const meta = s?.get('meta');
        if (s && meta) s.setFilters(emptyDraft(meta.filterOperands));
      }}>Clear</button>
      <button class="btn primary" type="button" @click=${() => (this.sheet = null)}>${matchedText}</button>
    </div>`;
    const sheetBody = this.sheet === 'filters' ? html`${filters}${sheetFooter}` : this.sheet === 'layers' ? html`${toolbar}${layersPanel}` : this.sheet === 'artifacts' ? html`${selectionPanel}${list}` : this.sheet === 'detail' ? detail : nothing;

    return html`<div part="frame" @tessera-artifactfit=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))} @tessera-close=${() => this.closeDetail()}>
      <tessera-map
        colour-by=${this.colourBy || nothing}
        layers=${this.layers || nothing}
        tooltip-fields=${this.tooltipFields}
        budget=${this.budget || nothing}
        controls-corner=${this.layout === 'overlay' ? 'top-right' : 'top-left'}
        .clusterLevel=${level}
        @tessera-viewchange=${() => this.requestUpdate()}
        @tessera-pick=${() => this.requestUpdate()}
        @tessera-hover=${() => nothing}
        @click=${() => this.requestUpdate()}
      >
        <div slot="bottom-left" class="in-map-strip"><slot name="status"><tessera-status></tessera-status></slot></div>
        <slot name="tooltip" slot="tooltip"></slot>
      </tessera-map>
      ${this.layout === 'overlay' ? html`${overlayLeft}${overlayRight}` : docked}
      ${this.sheet && sheetBody !== nothing ? html`<div part="sheet" role="dialog">${sheetBody}</div>` : nothing}
      <div part="strip-row"><tessera-status></tessera-status></div>
      <div part="tabs" role="tablist">
        ${this.has('filters') ? tab('filters', 'filter', 'Filters') : nothing}
        ${this.has('legend') ? tab('layers', 'layers', 'Layers') : nothing}
        ${this.has('artifacts') ? tab('artifacts', 'list', 'In view') : nothing}
        ${this.has('detail') ? tab('detail', 'info', 'Item') : nothing}
      </div>
      ${meta ? nothing : nothing}
    </div>`;
  }

  /** See `render`: the level a tiered layer draws at when nothing was chosen. */
  private autoLevel(): number | null {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const a = s?.get('artifacts');
    if (!s || !meta || !a) return null;
    const layer = clusterLayerOf(s.get('legend').colourBy) ?? a.layers[0] ?? null;
    const declared = layer ? meta.layers.find((l) => l.name === layer) : null;
    if (!declared || declared.levels.length === 0) return null;
    // **The served artifact's own `rung`**, straight off the wire — on a levelled layer, which is
    // the only kind reaching here, that is its declared level (contracts §3.2 r43), and never a
    // count of parent links, which answered a different question.
    const counts: number[] = [];
    for (const x of a.served) {
      if (x.layer !== layer) continue;
      counts[x.rung] = (counts[x.rung] ?? 0) + 1;
    }
    for (let i = 0; i < counts.length; i++) counts[i] ??= 0;
    if (counts.length <= 1) return null;
    return levelForBudget(counts, artifactBudgetFor(this.map?.zoom ?? 0));
  }

  /** The card's `×`: drop the selection it shows. */
  private closeDetail(): void {
    const s = this.resolvedStore;
    if (!s) return;
    const m = this.map;
    if (m) m.lastPick = null;
    s.clearSelection?.();
    this.requestUpdate();
  }
}

attachContextRoot();
defineOnce('tessera-explorer', TesseraExplorer);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-explorer': TesseraExplorer;
  }
}
