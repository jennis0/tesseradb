import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {REGION_HELD_LIMIT, type RegionProjection} from '@tesseradb/client';
import {TesseraElement, emit, idString, shapeDetail} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-selection>`: the selected region's counts, the held marks inside it as a list (a click
 * picks one), and the actions. Shown inside is the held marks against `matched`. Matched inside is
 * exact for the shape unless the server answered with a cover (`x-tessera-region`). Visible inside
 * shows only while no other filter narrows the view.
 *
 * The selection is itself a filter: the map and every count narrow to it once it settles.
 * Outside flips it to the complement. Not built yet: export and save as artifact, which need
 * server verbs; their buttons are disabled with the reason on hover.
 */
export class TesseraSelection extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='counts'] {
        margin-bottom: 4px;
      }
      [part='counts'] tessera-count::part(count) {
        font-weight: 500;
      }
      [part='counts'] tessera-count::part(label) {
        display: none;
      }
      .items-label {
        margin: 10px 0 4px;
      }
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 160px;
        overflow-y: auto;
      }
      [part='item'] {
        font-family: var(--_tessera-font-mono);
        font-size: 12px;
      }
      [part='actions'] {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin-top: 12px;
      }
    `
  ];

  /** By property, for a host with its own region. */
  @property({attribute: false}) accessor region: RegionProjection | null = null;

  private get shown(): RegionProjection | null {
    return this.region ?? this.resolvedStore?.get('region') ?? null;
  }

  override render(): TemplateResult | typeof nothing {
    const r = this.shown;
    const heading = (shape: string = '') => html`<h2 part="title">Selection<span class="summary">${shape}</span></h2>`;
    if (!r) return html`<div class="panel">${heading()}<span part="state" data-state="detached"></span></div>`;
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const state = r.status === 'shown' ? (stale ? 'stale' : 'shown') : r.status;
    const kv = (label: string, count: unknown) => html`<div class="k">${label}</div><div class="v">${count}</div>`;
    const counts =
      r.status === 'shown'
        ? html`<div part="counts" class="kv">
            ${kv('Shown inside', html`<tessera-count part="count-served" .count=${r.served} .stale=${stale} figure="shown"></tessera-count>`)}
            ${kv(r.shape.outside ? 'Matched outside' : 'Matched inside', html`<tessera-count part="count-matched" .masked=${r.matched} .stale=${stale}></tessera-count>`)}
            ${r.visible ? kv(r.shape.outside ? 'Visible outside' : 'Visible inside', html`<tessera-count part="count-visible" .masked=${r.visible} .stale=${stale}></tessera-count>`) : nothing}
          </div>`
        : nothing;
    const stateRegion =
      r.status === 'loading'
        ? html`<span part="state" data-state="loading"><span class="dot"></span>Counting<span class="skel" aria-hidden="true"></span></span>`
        : r.status === 'refused'
          ? html`<span part="state" data-state="refused">${icon('warn', 14)}Refused<span part="refusal" class="mono">${r.refusal?.code}</span></span>`
          : html`<span part="state" data-state=${state} title=${r.verdict === null ? 'not yet answered' : r.verdict.exact ? 'exact for the shape' : `a cover of the shape at depth ${r.verdict.depth}`}>${stale ? html`${icon('clock', 14)}Corpus updated` : nothing}</span>`;
    const ids = Array.from(r.held.ids, idString);
    const waiting = (label: unknown, reason: string) => html`<button part="action" class="btn off" type="button" disabled title=${reason}>${label}</button>`;
    return html`<div class="panel">${heading(r.shape.outside ? `outside ${r.shape.kind}` : r.shape.kind)}${stateRegion}${counts}
      ${ids.length > 0
        ? html`<div part="label" class="xs muted items-label">Shown items</div>
          <ul part="items" class="list" aria-label="held marks inside the selection">
            ${ids.map(
              (id) => html`<li part="item" class="item" tabindex="0" role="button"
                  @click=${() => void this.resolvedStore?.pick(BigInt(id))}
                  @keydown=${(e: KeyboardEvent) => {
                    if (e.key === 'Enter' || e.key === ' ') void this.resolvedStore?.pick(BigInt(id));
                  }}><span class="name">${id}</span></li>`
            )}
            ${r.held.count > ids.length ? html`<li class="muted xs">and ${(r.held.count - ids.length).toLocaleString('en-GB')} more (the first ${REGION_HELD_LIMIT} listed)</li>` : nothing}
          </ul>`
        : nothing}
      <div part="actions">
        <button part="action" class="btn" type="button" @click=${() => {
          this.resolvedStore?.select(null);
          emit(this, 'tessera-selectchange', {shape: null});
        }}>${icon('close', 13)}Clear</button>
        <button part="action" class="btn" type="button" title=${r.shape.outside ? 'Filter to the inside of the shape' : 'Filter to the outside of the shape'} @click=${() => {
          const next = {...r.shape, outside: !r.shape.outside};
          this.resolvedStore?.select(next);
          emit(this, 'tessera-selectchange', {shape: shapeDetail(next), status: 'loading'});
        }}>${icon('filter', 13)}${r.shape.outside ? 'Inside' : 'Outside'}</button>
        ${waiting('Export', 'Needs the export verb (not yet served)')}
        ${waiting('Save as artifact', 'Needs the runtime-artifact path (not yet served)')}
      </div>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-selection', TesseraSelection);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-selection': TesseraSelection;
  }
}
