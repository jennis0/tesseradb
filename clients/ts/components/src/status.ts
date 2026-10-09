import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {NO_COUNT, NO_MASKED, type StatusProjection, type ViewProjection} from '@mosaica/client';
import {MosaicaElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, showsContent, stateOf, type PanelState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * The view's state and counts on one line: `● | 16,822,190 of 21,406,522 match | 5,390 shown`.
 * The counts are the store's counts in view (`ViewProjection.inView`): over the camera's box, or
 * over the selected region while one is selected, as the selection card counts it. While a
 * highlight is set, a cell reads `N highlighted of M`, M being the matched count, and the
 * match cell is left out unless a filter or selection narrows what matches. While the view is up to
 * date the first cell is a dot alone, titled "Up to date", and while it updates a grey dot alone,
 * titled "Updating", so the strip keeps its width through a pan; otherwise it names the state in two
 * or three words beside its dot (Connecting, Reconnecting, Nothing in view, Data updated, View
 * refused, Session expired) with the action
 * the state offers: Refresh when the data changed, Retry when the view was refused, and Sign in on
 * expiry where `reauthorise` is set. The counts grey out while they are not current. The strip
 * sizes to its content and does not wrap. `compact` shortens the figures (`16.8M of 21.4M match`)
 * and drops the shown count. `expanded` renders the figures again as a card below the strip.
 *
 * The state cell is an `aria-live` region, so a refusal, an expiry or a change of data is announced;
 * the counts are not, so a pan does not read them out. The
 * state is one of the eight panel states (see `PanelState`); what was refused and why stays in the
 * store's `status.refusal`, and the refusal's code is on the refusal part's `data-code`.
 *
 * @summary The state and the counts of the view, on one line.
 * @tagname mosaica-status
 * @category Elements
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-statechange']>} mosaica-statechange - On every
 *   change of panel state, including the first render.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-expired']>} mosaica-expired - Once each time the
 *   session expires.
 * @csspart strip - The one-line strip.
 * @csspart state - The state's dot and words, with `data-state` set to the panel state.
 * @csspart refusal - The words "View refused", with `data-code` set to the refusal's code.
 * @csspart refresh - The Refresh button, when the data changed under the view.
 * @csspart retry - The Retry button, when the view was refused.
 * @csspart reauthorise - The Sign in button, on expiry where `reauthorise` is set.
 * @csspart count-shown - The `<mosaica-count>` of marks shown.
 * @csspart count-matched - The `<mosaica-count>` matched by the filters.
 * @csspart count-highlighted - The `<mosaica-count>` the highlight matched, while a highlight is set.
 * @csspart count-of - The matched count the highlighted count is out of, while a highlight is set.
 * @csspart count-visible - The `<mosaica-count>` of items the viewer may see here, which the matched
 *   count is out of.
 * @csspart card - The card of figures, under `expanded`.
 */
export class MosaicaStatus extends MosaicaElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: inline-block;
        max-width: 100%;
        vertical-align: top;
      }
      [part='strip'] {
        display: inline-flex;
        align-items: stretch;
        max-width: 100%;
        min-height: 32px;
        overflow: hidden;
        background: var(--_mosaica-surface);
        border: 1px solid var(--_mosaica-line);
        border-radius: var(--_mosaica-radius);
        box-shadow: var(--_mosaica-shadow);
        font-size: 12px;
        color: var(--_mosaica-ink-2);
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
      }
      [part='strip'][data-state='detached'] {
        display: none;
      }
      .cell {
        display: flex;
        align-items: center;
        gap: 4px;
        padding: 7px 12px;
        flex: none;
      }
      .cell mosaica-count {
        font-size: inherit;
      }
      .cell + .cell {
        border-left: 1px solid var(--_mosaica-line-2);
      }
      .cell.dim {
        opacity: 0.4;
      }
      .cell.state {
        padding: 0 5px 0 12px;
      }
      .cell.state.bare {
        padding: 0 12px;
      }
      [part='state'] {
        gap: 7px;
        color: inherit;
      }
      [part='state'] .skel {
        display: none;
      }
      [part='state'] .btn {
        margin-left: 1px;
      }
      [part='state']:not(:has(.btn)) {
        padding-right: 7px;
      }
      .cell mosaica-count::part(count) {
        font-weight: 600;
      }
      .cell mosaica-count[part~='count-visible']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell mosaica-count[part~='count-shown']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell mosaica-count[part~='count-of']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell mosaica-count.lit::part(count) {
        color: var(--_mosaica-highlight);
      }
      .cell mosaica-count::part(label) {
        margin-left: 0.3em;
      }
      :host([compact]) .cell + .cell {
        border-left: 0;
        padding-left: 0;
      }
      :host([compact]) .cell.state {
        padding-right: 7px;
      }
      [part='card'] {
        margin-top: 8px;
        padding: 10px 12px;
        background: var(--_mosaica-surface);
        border: 1px solid var(--_mosaica-line);
        border-radius: var(--_mosaica-radius);
        box-shadow: var(--_mosaica-shadow);
        font-size: 12px;
      }
    `
  ];

  /** Renders the figures again as a card below the strip. */
  @property({type: Boolean}) accessor expanded = false;
  /** Shortens the figures to one decimal (`16.8M`) and drops the shown count, where room is short. */
  @property({type: Boolean, reflect: true}) accessor compact = false;
  /** Called by the Sign in button that the expired state shows; without it, no button. */
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
      emit(this, 'mosaica-statechange', {from, to: state});
    }
    if (state === 'expired' && !this.expiryFired) {
      this.expiryFired = true;
      emit(this, 'mosaica-expired', {refusal: this.status?.refusal ?? null});
    } else if (state !== 'expired') {
      this.expiryFired = false;
    }
  }

  override render(): TemplateResult | typeof nothing {
    const status = this.status;
    const state = stateOf(status);
    const stale = status?.stale ?? false;
    const v = this.view;
    const refresh = () => this.resolvedStore?.refresh();
    // Up to date and updating, the two states a pan moves between, are each a dot alone, so the
    // strip keeps its width through a gesture; every other state names itself beside its dot.
    const bare = (cls: string, words: string) => html`<span part="state" data-state=${state}><span class=${cls} role="img" aria-label=${words} title=${words}></span></span>`;
    const dotAlone = state === 'shown' || (state === 'loading' && status?.sessionWarm === true);
    const inner =
      state === 'shown'
        ? bare('dot', 'Up to date')
        : dotAlone
          ? bare('dot quiet', 'Updating')
          : state === 'empty'
            ? html`<span part="state" data-state="empty"><span class="dot quiet"></span>Nothing in view</span>`
            : renderState(state, status, {onRefresh: refresh, onRetry: refresh, onReauthorise: this.reauthorise});
    // One live element across every state, so each change of state is announced from it.
    const first = html`<div class=${`cell state${dotAlone ? ' bare' : ''}`} role="status" aria-live="polite">${inner}</div>`;
    return html`<div part="strip" data-state=${state}>${first}${this.counts(state, v, stale)}</div>
      ${this.expanded && showsContent(state) && v ? this.card(v, stale) : nothing}`;
  }

  /**
   * The count cells: current in the shown state, greyed out while a new answer is awaited or the
   * last one was refused, and skeletons where there is no answer yet. Nothing where there is no
   * view to count, or where the view is empty.
   */
  private counts(state: PanelState, v: ViewProjection | null, stale: boolean): TemplateResult | typeof nothing {
    if (state === 'detached' || state === 'empty' || !v) return nothing;
    if (state === 'loading' && this.status?.sessionWarm === false) return nothing;
    const n = v.inView;
    const dim = state !== 'shown' || n?.status !== 'shown';
    const cls = `cell${dim ? ' dim' : ''}`;
    if (!n || stale) {
      const skel = html`<span class="skel" aria-hidden="true"></span>`;
      return html`<div class=${cls}>${skel}<span>match</span></div>${this.compact ? nothing : html`<div class=${cls}>${skel}<span>shown</span></div>`}`;
    }
    // Under a highlight with nothing narrowing, the match cell would read "M of M match" beside
    // the highlight's, so the highlight's cell stands alone and names M.
    const narrowed = this.resolvedStore?.requestFilters() != null;
    const match = html`<div class=${cls}>
      <mosaica-count part="count-matched" .masked=${n.matched} .compact=${this.compact}></mosaica-count><span>of</span><mosaica-count
        part="count-visible" .masked=${n.visible} .compact=${this.compact}></mosaica-count><span>match</span>
    </div>`;
    return html`${v.highlighting && !narrowed ? nothing : match}
      ${v.highlighting
        ? html`<div class=${cls}><mosaica-count part="count-highlighted" class="lit" .masked=${n.highlighted} .compact=${this.compact}></mosaica-count><span>highlighted of</span><mosaica-count
              part="count-of" .masked=${n.matched} .compact=${this.compact}></mosaica-count></div>`
        : nothing}
      ${this.compact ? nothing : html`<div class=${cls}><mosaica-count part="count-shown" .count=${{shown: n.shown, total: n.matched.value, exact: true}} figure="shown" label="shown"></mosaica-count></div>`}`;
  }

  private card(v: ViewProjection, stale: boolean) {
    const row = (label: string, value: unknown) => html`<div class="k">${label}</div><div class="v">${value}</div>`;
    const n = v.inView;
    return html`<div part="card"><div class="kv">
      ${row('Shown', html`<mosaica-count .count=${n ? {shown: n.shown, total: n.matched.value, exact: true} : NO_COUNT} .stale=${stale}></mosaica-count>`)}
      ${row('Match the filters', html`<mosaica-count .masked=${n?.matched ?? NO_MASKED} .stale=${stale}></mosaica-count>`)}
      ${v.highlighting ? row('Highlighted', html`<mosaica-count .masked=${n?.highlighted ?? NO_MASKED} .stale=${stale}></mosaica-count>`) : nothing}
      ${row('In this view', html`<mosaica-count .masked=${n?.visible ?? NO_MASKED} .stale=${stale}></mosaica-count>`)}
    </div></div>`;
  }
}

attachContextRoot();
defineOnce('mosaica-status', MosaicaStatus);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-status': MosaicaStatus;
  }
}
