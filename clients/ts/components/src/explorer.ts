import {ContextProvider} from '@lit/context';
import {css, html, nothing, type PropertyValues} from 'lit';
import {property, state} from 'lit/decorators.js';
import type {Store} from '@tesseradb/client';
import {activeCount, artifactBudgetFor, browsableLayers, emptyDraft, levelForBudget} from '@tesseradb/client';
import {clusterLayerOf} from '@tesseradb/deck/internal';
import './hierarchy.js';
import {TesseraElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {sameFrame} from './view-switch.js';
import type {TesseraMap} from './map.js';
import {chrome, tokens} from './tokens.js';
import './map.js';
import './status.js';
import './filter-panel.js';
import './item-card.js';
import './selection.js';
import './layer-picker.js';
import './view-picker.js';
import './key-picker.js';
import './artifact-list.js';
import './artifact-card.js';
import './legend.js';

/** Every part of every element the explorer renders, forwarded (`parts.ts`). */
const FORWARD = {
  map: exportparts('map'),
  status: exportparts('status'),
  'view-picker': exportparts('view-picker'),
  'key-picker': exportparts('key-picker'),
  legend: exportparts('legend'),
  'layer-picker': exportparts('layer-picker'),
  'filter-panel': exportparts('filter-panel', forwarded('filter')),
  hierarchy: exportparts('hierarchy'),
  'artifact-list': exportparts('artifact-list'),
  selection: exportparts('selection'),
  'item-card': exportparts('item-card'),
  'artifact-card': exportparts('artifact-card')
};

const ALL_PANELS = ['toolbar', 'legend', 'filters', 'hierarchy', 'artifacts', 'selection', 'detail'] as const;
type Panel = (typeof ALL_PANELS)[number];
type Sheet = 'filters' | 'layers' | 'artifacts' | 'detail';
/** The narrow layout's tabs, each opening a sheet, drawn where its panel is. */
const TABS: readonly {sheet: Sheet; icon: IconName; label: string; panel: Panel}[] = [
  {sheet: 'filters', icon: 'filter', label: 'Filters', panel: 'filters'},
  {sheet: 'layers', icon: 'layers', label: 'Layers', panel: 'legend'},
  {sheet: 'artifacts', icon: 'list', label: 'In view', panel: 'artifacts'},
  {sheet: 'detail', icon: 'info', label: 'Item', panel: 'detail'}
];

/**
 * `<tessera-explorer>`: the map with its status strip, toolbar, layer picker, filters, artifact
 * list and detail card in a default layout. It builds its own store from `viewer-url` and `token`
 * or an `authorise` property, or takes a `.store`, and is the context provider for its pieces.
 *
 * `layout="docked"` puts the map beside a sidebar: the colour and layer selects at the top, the
 * layer checklist, the item or cluster card, then Filters and In view as collapsed sections with
 * a summary each. `layout="overlay"` draws the map full-bleed with a floating panel top-left (the
 * selects, the layers and the filters), the toolbar top-right, and a floating panel at the right
 * with In view and the card. In a narrow container the strip runs full width above a tab bar
 * (Filters, Layers, In view, Item), and each tab opens its panel as a sheet. `panels` chooses which
 * appear; every region is a named slot with default content.
 *
 * `<tessera-hierarchy>` sits beneath the filters, drawn only where the bundle has a hierarchical
 * layer to browse, in a section collapsed by default: In view answers for the viewport and the
 * hierarchy for the corpus.
 */
export class TesseraExplorer extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        container-type: inline-size;
        container-name: explorer;
        background: var(--_tessera-surface);
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
        grid-template-columns: minmax(0, 1fr) var(--tessera-sidebar-width, 336px);
      }
      :host([layout='overlay']) [part='frame'] {
        grid-template-columns: minmax(0, 1fr);
      }
      tessera-map,
      ::slotted(tessera-map) {
        height: 100%;
        min-height: 320px;
      }
      [part='sidebar'] {
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        border-left: 1px solid var(--_tessera-line);
        background: var(--_tessera-surface);
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
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        overflow-y: auto;
        max-height: 100%;
      }
      .card > *:last-child {
        border-bottom: 0;
      }
      .card ::part(panel) {
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      /* Collapsed sections. */
      details {
        border-bottom: 1px solid var(--_tessera-line-2);
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
        color: var(--_tessera-ink-2);
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
        color: var(--_tessera-ink-3);
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
          border-top: 1px solid var(--_tessera-line);
          background: var(--_tessera-surface);
        }
        [part='tabs'] button {
          flex: 1;
          display: flex;
          flex-direction: column;
          align-items: center;
          justify-content: center;
          gap: 3px;
          color: var(--_tessera-ink-2);
          font-size: 11px;
          font-weight: 500;
        }
        [part='tabs'] button[aria-selected='true'] {
          color: var(--_tessera-accent);
          font-weight: 600;
        }
        [part='sheet'] {
          position: absolute;
          left: 0;
          right: 0;
          bottom: 56px;
          max-height: 70%;
          overflow-y: auto;
          background: var(--_tessera-surface);
          border-top: 1px solid var(--_tessera-line);
          border-radius: 12px 12px 0 0;
          box-shadow: var(--_tessera-shadow);
          z-index: 6;
        }
        .sheet-footer {
          position: sticky;
          bottom: 0;
          display: flex;
          gap: 10px;
          padding: 10px 16px;
          border-top: 1px solid var(--_tessera-line-2);
          background: var(--_tessera-surface);
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
          background: var(--_tessera-line);
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
  /** The record field that titles a point, for the map's hover and the item card; unset, its id. */
  @property({attribute: 'title-field'}) accessor titleField = '';
  @property({type: Number}) accessor budget = 0;
  @state() accessor sheet: Sheet | null = null;
  /** The narrow layout's tab focused last, which keeps the tab list's one place in the tab order. */
  @state() private accessor tabFocus: Sheet | null = null;
  /** The level chosen through the legend's select; the map colours and labels at it. */
  @state() accessor level: number | null = null;

  private provider = new ContextProvider(this, {context: storeContext, initialValue: null});
  /** Which of the two selections changed last — what the detail region shows. */
  private lastDetail: 'item' | 'artifact' = 'item';
  private seenItem: object | null = null;
  private seenArtifact: object | null = null;

  protected override onStoreAdopted(store: Store | null): void {
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

  override disconnectedCallback(): void {
    // **A follow in flight is released here, not only at dispose.** The base class drops this
    // element's own subscription; the follow's is a second one, and a callback left running on a
    // detached explorer would eventually centre a map that is no longer in the document.
    this.following?.();
    super.disconnectedCallback();
  }

  override dispose(): void {
    this.following?.();
    this.map?.dispose();
    super.dispose();
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
    // The level drawn: the one chosen through the legend, else, for a tiered layer the server
    // served whole, the level the view's budget would have cut at, else the deepest served. The
    // legend shows which; the map colours, outlines and labels at it.
    const autoLevel = this.autoLevel();
    const level = this.level ?? autoLevel;
    const hasDetail = Boolean(selection?.item || selection?.artifact || selection?.artifactRefusal || selection?.itemRefusal || this.map?.lastPick);
    // The detail region shows whichever changed last.
    const showArtifact = this.lastDetail === 'artifact' && (selection?.artifact || selection?.artifactRefusal);
    const detail = html`<slot name="detail">${showArtifact ? html`<tessera-artifact-card exportparts=${FORWARD['artifact-card']}></tessera-artifact-card>` : html`<tessera-item-card exportparts=${FORWARD['item-card']} title-field=${this.titleField || nothing} .pick=${this.map?.lastPick ?? null}></tessera-item-card>`}</slot>`;
    // The two view pickers head the toolbar slot, which the docked sidebar, the overlay's left
    // card and the narrow layout's Layers sheet all render. Both draw nothing for a one-view corpus.
    const toolbar = html`<slot name="toolbar"><tessera-view-picker exportparts=${FORWARD['view-picker']}></tessera-view-picker><tessera-key-picker exportparts=${FORWARD['key-picker']}></tessera-key-picker><tessera-legend exportparts=${FORWARD.legend} selectable .level=${this.level} .autoLevel=${autoLevel} @tessera-levelchange=${(e: CustomEvent<{level: number | null}>) => (this.level = e.detail.level)}></tessera-legend></slot>`;
    const layersPanel = html`<slot name="layers"><tessera-layer-picker exportparts=${FORWARD['layer-picker']}></tessera-layer-picker></slot>`;
    const filters = html`<slot name="filters"><tessera-filter-panel exportparts=${FORWARD['filter-panel']}></tessera-filter-panel></slot>`;
    const hierarchy = html`<slot name="hierarchy"><tessera-hierarchy exportparts=${FORWARD.hierarchy}></tessera-hierarchy></slot>`;
    // Drawn only where there is a lineage to walk: a bundle of flat clusterings has none, and an
    // empty section reads as a panel that failed rather than one with nothing to say.
    const hasHierarchy = browsableLayers(meta?.layers ?? []).length > 0;
    const list = html`<slot name="artifacts"><tessera-artifact-list exportparts=${FORWARD['artifact-list']}></tessera-artifact-list></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><tessera-selection exportparts=${FORWARD.selection}></tessera-selection></slot>` : nothing;
    const section = (name: IconName, title: string, summary: string, body: unknown, open = false) =>
      html`<details ?open=${open}><summary><span class="t"><span class="closed-chev">${icon('chevr', 14)}</span><span class="open-chev">${icon('chev', 14)}</span>${title}</span><span class="summary">${summary}</span></summary><div class="body" data-section=${name}>${body}</div></details>`;

    const docked = html`<aside part="sidebar" aria-label="Explorer panels">
      ${this.has('toolbar') ? toolbar : nothing}
      ${this.has('legend') ? layersPanel : nothing}
      ${selectionPanel}
      ${this.has('detail') && hasDetail ? detail : nothing}
      ${this.has('filters') ? section('filter', 'Filters', active > 0 ? `${active} applied` : '', filters) : nothing}
      ${this.has('hierarchy') && hasHierarchy ? section('layers', 'Hierarchy', '', hierarchy) : nothing}
      ${this.has('artifacts') ? section('list', 'In view', inView > 0 ? `${inView.toLocaleString('en-GB')} cluster${inView === 1 ? '' : 's'}` : '', list) : nothing}
    </aside>`;

    const overlayLeft = html`<div class="float left">
      <div class="card">
        ${this.has('toolbar') ? toolbar : nothing}
        ${this.has('legend') ? layersPanel : nothing}
        ${this.has('filters') ? filters : nothing}
        ${this.has('hierarchy') && hasHierarchy ? hierarchy : nothing}
      </div>
    </div>`;
    const overlayRight = html`<div class="float right">
      <slot name="top-right"></slot>
      ${this.has('artifacts') || (this.has('detail') && hasDetail) || selectionPanel !== nothing
        ? html`<div class="card">${selectionPanel}${this.has('artifacts') ? list : nothing}${this.has('detail') && hasDetail ? detail : nothing}</div>`
        : nothing}
    </div>`;

    const tabs = TABS.filter((t) => this.has(t.panel));
    // One tab is in the page's tab order: the one focused last, else the open sheet's, else the first.
    const reachable = tabs.find((t) => t.sheet === this.tabFocus) ?? tabs.find((t) => t.sheet === this.sheet) ?? tabs[0];
    const tab = ({sheet, icon: ic, label}: (typeof TABS)[number]) =>
      html`<button type="button" role="tab" id=${`tab-${sheet}`} data-sheet=${sheet} tabindex=${reachable?.sheet === sheet ? '0' : '-1'}
        aria-selected=${this.sheet === sheet ? 'true' : 'false'} aria-controls=${this.sheet === sheet ? 'sheet' : nothing}
        @click=${() => {
          this.tabFocus = sheet;
          this.sheet = this.sheet === sheet ? null : sheet;
        }}>${icon(ic, 18)}${label}</button>`;
    // The filters sheet's primary action names the number it will produce.
    const matched = s?.get('view').matched;
    const matchedText = matched && matched.exact && s?.get('status').status === 'shown' ? `Show ${matched.value.toLocaleString('en-GB')} matched` : 'Show';
    const sheetFooter = html`<div class="sheet-footer">
      <button class="btn" type="button" @click=${() => {
        const meta = s?.get('meta');
        if (s && meta) s.setFilters(emptyDraft(meta.filterOperands));
      }}>Clear</button>
      <button class="btn primary" type="button" @click=${() => (this.sheet = null)}>${matchedText}</button>
    </div>`;
    const sheetBody =
      this.sheet === 'filters'
        ? html`${filters}${this.has('hierarchy') && hasHierarchy ? hierarchy : nothing}${sheetFooter}`
        : this.sheet === 'layers'
          ? html`${toolbar}${layersPanel}`
          : this.sheet === 'artifacts'
            ? html`${selectionPanel}${list}`
            : this.sheet === 'detail'
              ? detail
              : nothing;

    // The tooltip slot is forwarded only when the host supplied one: a slot assigned another slot
    // counts as filled even when that slot is empty, which would hide the map's own tooltip.
    return html`<div part="frame" @tessera-artifactfit=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))} @tessera-viewfollow=${(e: CustomEvent<{view: string; x: number; y: number}>) => this.followItem(e.detail)} @tessera-close=${() => this.closeDetail()}>
      <tessera-map
        exportparts=${FORWARD.map}
        colour-by=${this.colourBy || nothing}
        layers=${this.layers || nothing}
        tooltip-fields=${this.tooltipFields}
        title-field=${this.titleField || nothing}
        budget=${this.budget || nothing}
        controls-corner=${this.layout === 'overlay' ? 'top-right' : 'top-left'}
        .clusterLevel=${level}
        @tessera-viewchange=${() => this.requestUpdate()}
        @tessera-pick=${() => this.requestUpdate()}
        @click=${() => this.requestUpdate()}
      >
        <div slot="bottom-left" class="in-map-strip"><slot name="status"><tessera-status exportparts=${FORWARD.status}></tessera-status></slot></div>
        ${this.querySelector('[slot="tooltip"]') ? html`<slot name="tooltip" slot="tooltip"></slot>` : nothing}
      </tessera-map>
      ${this.layout === 'overlay' ? html`${overlayLeft}${overlayRight}` : docked}
      ${this.sheet && sheetBody !== nothing
        ? html`<div part="sheet" id="sheet" role="dialog" aria-labelledby=${`tab-${this.sheet}`} tabindex="-1" @keydown=${this.onSheetKey}>${sheetBody}</div>`
        : nothing}
      <div part="strip-row"><tessera-status exportparts=${FORWARD.status}></tessera-status></div>
      <div part="tabs" role="tablist" aria-label="Explorer panels" @keydown=${this.onTabKey}>${tabs.map(tab)}</div>
    </div>`;
  }

  /** The tab list's keys: the arrows move between tabs, wrapping, and Home and End go to the ends. */
  private onTabKey = (e: KeyboardEvent): void => {
    const buttons = Array.from(this.renderRoot.querySelectorAll<HTMLButtonElement>('[part="tabs"] [role="tab"]'));
    const at = buttons.indexOf(e.target as HTMLButtonElement);
    if (at < 0) return;
    const n = buttons.length;
    const next = {ArrowRight: (at + 1) % n, ArrowLeft: (at - 1 + n) % n, Home: 0, End: n - 1}[e.key];
    if (next === undefined) return;
    e.preventDefault();
    const target = buttons[next]!;
    this.tabFocus = target.dataset.sheet as Sheet;
    target.focus();
  };

  /** Escape closes the sheet. */
  private onSheetKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    this.sheet = null;
  };

  /** Focus goes into a sheet as it opens, and back to its tab as it closes. */
  protected override updated(changed: PropertyValues<this>): void {
    if (!changed.has('sheet')) return;
    const before = changed.get('sheet');
    if (this.sheet) this.renderRoot.querySelector<HTMLElement>('[part="sheet"]')?.focus();
    else if (before) this.renderRoot.querySelector<HTMLElement>(`[role="tab"][data-sheet="${before}"]`)?.focus();
  }

  /** See `render`: the level a tiered layer draws at when nothing was chosen. */
  private autoLevel(): number | null {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const a = s?.get('artifacts');
    if (!s || !meta || !a) return null;
    const coloured = clusterLayerOf(s.get('legend').colourBy);
    const layer = coloured ?? a.layers[0] ?? null;
    const declared = layer ? meta.layers.find((l) => l.name === layer) : null;
    if (!declared || declared.levels.length === 0) return null;
    // **The served artifact's own `rung`**, straight off the wire — on a levelled layer, which is
    // the only kind reaching here, that is its declared level (contracts §3.2 r44), and never a
    // count of parent links, which answered a different question.
    const counts: number[] = [];
    for (const x of coloured ? a.colourServed : a.served) {
      if (x.layer !== layer) continue;
      counts[x.rung] = (counts[x.rung] ?? 0) + 1;
    }
    for (let i = 0; i < counts.length; i++) counts[i] ??= 0;
    if (counts.length <= 1) return null;
    return levelForBudget(counts, artifactBudgetFor(this.map?.zoom ?? 0));
  }

  /** A follow in flight: dropped when it lands, when another starts, and at dispose. */
  private following: (() => void) | null = null;

  /**
   * Follow an item into another view (`view-switching.md` §6.4): switch, then centre the camera on
   * the position the item's detail already held for that view.
   *
   * **The wait is for the frame, not for a timer.** Across frames the map refits under the new
   * view and the store answers a fresh viewport, so centring before that composition is drawn
   * would put the camera where the *old* frame's extent said, and the refit would then move it
   * again. Within one frame — a step along a group's roster — there is nothing to wait for and the
   * camera moves at once.
   */
  private followItem(detail: {view: string; x: number; y: number}): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    this.following?.();
    this.following = null;
    const target = meta.views.find((v) => v.id === detail.view);
    if (!target) return;
    const centre = () => void this.updateComplete.then(() => this.map?.lookAt(detail.x, detail.y));
    if (s.get('view').id === detail.view) {
      centre();
      return;
    }
    const held = sameFrame(s.frame(), target.quantisation);
    s.setCurrentView(detail.view);
    if (held) {
      centre();
      return;
    }
    const stop = s.subscribe(() => {
      const view = s.get('view');
      // Another switch took over — the user chose elsewhere while this one was waiting.
      if (view.id !== detail.view) {
        this.following?.();
        return;
      }
      if (!view.composition) {
        // **A view that answers without a frame ends the wait.** A refusal, an expiry, or a mask
        // that leaves this principal nothing to draw are all settled answers: there will be no
        // composition to centre on, and a subscription left waiting for one never ends.
        const status = s.get('status');
        if (status.status === 'refused' || status.status === 'empty' || status.expired) this.following?.();
        return;
      }
      this.following?.();
      centre();
    });
    this.following = () => {
      stop();
      this.following = null;
    };
  }

  /** The card's `×`: drop the selection it shows. */
  private closeDetail(): void {
    const s = this.resolvedStore;
    if (!s) return;
    const m = this.map;
    if (m) m.lastPick = null;
    s.clearSelection();
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
