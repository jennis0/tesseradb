import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {REGION_HELD_LIMIT, type RegionProjection} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-selection>` — the selected region (design §5.3 tier 2, §5.11): its three numbers
 * through `<tessera-count>` — `visible` and `matched` as `Masked`, inexact where a counted cell
 * exceeded a pixel; `served` as the held marks inside against `matched`, both figures always —
 * the served items inside as a list (click picks), and the actions.
 *
 * *Clear* works now. *Filter to this*, *export* and *save as artifact* are greyed with the
 * reason on hover rather than omitted: each is a server-side verb asked for in D11 — the
 * selection operand, the bulk-export verb, the runtime-artifact path — and ⊘ none is built.
 */
export class TesseraSelection extends TesseraElement {
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
      [part='counts'] {
        display: flex;
        flex-wrap: wrap;
        gap: calc(var(--tessera-space) * 1.5);
        margin-bottom: var(--tessera-space);
      }
      [part='items'] {
        max-height: 132px;
        overflow-y: auto;
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
        padding: 4px 6px;
        margin-bottom: var(--tessera-space);
        list-style: none;
        margin-top: 0;
        padding-left: 6px;
      }
      [part='item'] {
        cursor: pointer;
        font-variant-numeric: tabular-nums;
      }
      [part='item']:hover {
        color: var(--tessera-fg-strong);
      }
      [part='actions'] {
        display: flex;
        flex-wrap: wrap;
        gap: var(--tessera-space);
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
    const heading = html`<h2 part="title">Selection</h2>`;
    if (!r) return html`${heading}<span part="state" data-state="detached"></span>`;
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const state = r.status === 'shown' ? (stale ? 'stale' : 'shown') : r.status;
    const shape = r.shape.kind === 'box' ? `box ${r.shape.bbox.map((v) => v.toFixed(1)).join(', ')}` : `lasso (${r.shape.points.length} points)`;
    const counts =
      r.status === 'shown'
        ? html`<div part="counts">
            <tessera-count part="count-served" .count=${r.served} .stale=${stale} label="held marks inside"></tessera-count>
            <tessera-count part="count-matched" .masked=${r.matched} .stale=${stale} label="matched"></tessera-count>
            <tessera-count part="count-visible" .masked=${r.visible} .stale=${stale} label="visible"></tessera-count>
          </div>`
        : nothing;
    const stateRegion =
      r.status === 'loading'
        ? html`<span part="state" data-state="loading"><span class="badge">counting</span><span class="skeleton" aria-hidden="true"></span></span>`
        : r.status === 'refused'
          ? html`<span part="state" data-state="refused"><span class="badge">refused</span><span part="refusal">${r.refusal?.code}: ${r.refusal?.detail}</span></span>`
          : html`<span part="state" data-state=${state}
              >${stale ? html`<span class="badge">stale</span>` : nothing}<span class="muted"
                >${shape} · counted at depth ${r.depth} over ${r.tiles.toLocaleString('en-GB')} cells${r.visible.exact
                  ? ''
                  : ' — a cell is wider than a pixel here, so the numbers are exact for the cells, not the shape'}</span
              ></span
            >`;
    const ids = Array.from(r.held.ids, idString);
    const disabled = (reason: string) => html`<button part="action" type="button" disabled title=${reason}>`;
    return html`${heading}${stateRegion}${counts}
      ${ids.length > 0
        ? html`<ul part="items" aria-label="held marks inside the selection">
            ${ids.map(
              (id) => html`<li part="item" tabindex="0" role="button"
                  @click=${() => void this.resolvedStore?.pick(BigInt(id))}
                  @keydown=${(e: KeyboardEvent) => {
                    if (e.key === 'Enter' || e.key === ' ') void this.resolvedStore?.pick(BigInt(id));
                  }}>${id}</li>`
            )}
            ${r.held.count > ids.length ? html`<li class="muted">…and ${(r.held.count - ids.length).toLocaleString('en-GB')} more held (the first ${REGION_HELD_LIMIT} listed)</li>` : nothing}
          </ul>`
        : nothing}
      <div part="actions">
        <button part="action" type="button" @click=${() => {
          this.resolvedStore?.select(null);
          emit(this, 'tessera-selectchange', {shape: null});
        }}>clear</button>
        ${disabled('the selection operand is not built (D11, asked for): a region cannot yet be composed with the other filters')}filter to this</button>
        ${disabled('the bulk-export verb is not built (D11, asked for)')}export</button>
        ${disabled('the runtime-artifact path is not built (D11, asked for)')}save as artifact</button>
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
