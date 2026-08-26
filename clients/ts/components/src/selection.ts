import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {REGION_HELD_LIMIT, type RegionProjection} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-selection>` — the selected region (design §5.3 tier 2, §5.11), as the boards draw it
 * (`SelectionFlow.png`): *Shown inside · Matched inside · Visible inside* through
 * `<tessera-count>` — `visible` and `matched` as `Masked`, inexact where a counted cell exceeded a
 * pixel; `served` as the held marks inside against `matched`, both figures always — the shown
 * items as a list (click picks), and the actions.
 *
 * *Clear* works now. *Filter to this* and *Export* are greyed with the reason on hover rather
 * than omitted: each is a server-side verb asked for in D11 — the selection operand, the
 * bulk-export verb — and ⊘ neither is built.
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
        font-family: var(--tessera-font-mono);
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

  override render() {
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
            ${kv('Matched inside', html`<tessera-count part="count-matched" .masked=${r.matched} .stale=${stale}></tessera-count>`)}
            ${kv('Visible inside', html`<tessera-count part="count-visible" .masked=${r.visible} .stale=${stale}></tessera-count>`)}
          </div>`
        : nothing;
    const stateRegion =
      r.status === 'loading'
        ? html`<span part="state" data-state="loading"><span class="dot"></span>Counting<span class="skel" aria-hidden="true"></span></span>`
        : r.status === 'refused'
          ? html`<span part="state" data-state="refused">${icon('warn', 14)}Refused<span part="refusal" class="mono">${r.refusal?.code}</span></span>`
          : html`<span part="state" data-state=${state} title=${`counted at depth ${r.depth} over ${r.tiles.toLocaleString('en-GB')} cells`}>${stale ? html`${icon('clock', 14)}Corpus updated` : nothing}</span>`;
    const ids = Array.from(r.held.ids, idString);
    // Greyed with the reason on hover, never omitted: each waits on a server verb (D11).
    const waiting = (label: unknown, reason: string) => html`<button part="action" class="btn off" type="button" disabled title=${reason}>${label}</button>`;
    return html`<div class="panel">${heading(r.shape.kind)}${stateRegion}${counts}
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
        ${waiting(html`${icon('filter', 13)}Filter to this`, 'Needs the selection operand (not yet served)')}
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
