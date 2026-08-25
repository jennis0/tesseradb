import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {NO_COUNT, NO_MASKED, type StatusProjection, type ViewProjection} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, showsContent, stateOf, type PanelState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-status>` — the state and the three counts, as one line (design §5.3, decision 0098):
 * *shown · matched · visible*, in that order because their relationship is the content — a
 * filter narrows the answer and never the grant — with the state as a badge and the refresh
 * control when stale. The detail behind the numbers is a hover; `expanded` renders it as a card.
 *
 * An `aria-live` region, so a refusal, an expiry or a stale signal is announced (§5.8). Fires
 * `tessera-statechange` on every transition of §5.4 and `tessera-expired` once per expiry.
 */
export class TesseraStatus extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='strip'] {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: calc(var(--tessera-space) * 1.5);
        padding: var(--tessera-space) calc(var(--tessera-space) * 1.6);
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
      }
      .sep {
        color: var(--tessera-fg-muted);
      }
      [part='card'] {
        margin-top: var(--tessera-space);
        padding: var(--tessera-space) calc(var(--tessera-space) * 1.6);
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
      }
    `
  ];

  @property({type: Boolean}) accessor expanded = false;
  /** The host's renewal, shown on expiry beside the prompt. */
  @property({attribute: false}) accessor reauthorise: (() => void) | null = null;

  private lastState: PanelState | null = null;
  private expiryFired = false;

  private get status(): StatusProjection | null {
    return this.resolvedStore?.get('status') ?? null;
  }

  private get view(): ViewProjection | null {
    return this.resolvedStore?.get('view') ?? null;
  }

  protected override updated(): void {
    const state = stateOf(this.status);
    if (state !== this.lastState) {
      const from = this.lastState;
      this.lastState = state;
      emit(this, 'tessera-statechange', {from, to: state});
    }
    if (state === 'expired' && !this.expiryFired) {
      this.expiryFired = true;
      emit(this, 'tessera-expired', {refusal: this.status?.refusal ?? null});
    } else if (state !== 'expired') {
      this.expiryFired = false;
    }
  }

  private hover(): string {
    const v = this.view;
    const r = this.resolvedStore?.get('replica');
    if (!v || !r) return '';
    const parts = [`drawn at depth ${v.depth}`];
    if (v.provisional > 0) parts.push(`${v.provisional.toLocaleString('en-GB')} provisional marks (uncounted)`);
    // Exact only (§5.10): every band on screen resolves to the served set, or some are refetching.
    const a = this.resolvedStore?.get('artifacts');
    if (a && a.layers.length > 0 && a.status === 'shown') {
      parts.push(a.coverage.stale === 0 ? 'colours exact' : `refreshing ${a.coverage.stale.toLocaleString('en-GB')} tiles`);
    }
    parts.push(`replica holds ${(r.bytes / 1e6).toFixed(1)} MB in ${r.bands.toLocaleString('en-GB')} bands`);
    return parts.join(' · ');
  }

  override render() {
    const status = this.status;
    const state = stateOf(status);
    const stale = status?.stale ?? false;
    const v = this.view;
    const content = showsContent(state) && v;
    const counts = content
      ? html`<tessera-count part="count-shown" .count=${v.served} .stale=${stale} label="shown"></tessera-count
          ><span class="sep">·</span
          ><tessera-count part="count-matched" .masked=${v.matched} .stale=${stale} label="matched"></tessera-count
          ><span class="sep">·</span
          ><tessera-count part="count-visible" .masked=${v.visible} .stale=${stale} label="visible"></tessera-count>`
      : nothing;
    return html`<div part="strip" role="status" aria-live="polite" title=${this.hover()}>
        ${renderState(state, status, {onRefresh: () => this.resolvedStore?.refresh(), onReauthorise: this.reauthorise})}
        ${counts}
      </div>
      ${this.expanded && content ? this.card(v, stale) : nothing}`;
  }

  private card(v: ViewProjection, stale: boolean) {
    const r = this.resolvedStore?.get('replica');
    const row = (label: string, value: unknown) =>
      html`<div class="row"><span part="label">${label}</span><span part="value">${value}</span></div>`;
    return html`<div part="card">
      ${row('visible (in mask)', html`<tessera-count .masked=${v.visible ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
      ${row('matched (after filters)', html`<tessera-count .masked=${v.matched ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
      ${row('served (drawn)', html`<tessera-count .count=${v.served ?? NO_COUNT} .stale=${stale}></tessera-count>`)}
      ${v.provisional > 0 ? row('provisional marks (uncounted)', v.provisional.toLocaleString('en-GB')) : nothing}
      ${row('drawn depth', v.depth)}
      ${r ? row('replica', `${(r.bytes / 1e6).toFixed(1)} MB · ${r.points.toLocaleString('en-GB')} points · ${r.bands.toLocaleString('en-GB')} bands`) : nothing}
      <div class="muted">exact over the drawn region, which is wider than the viewport</div>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-status', TesseraStatus);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-status': TesseraStatus;
  }
}
