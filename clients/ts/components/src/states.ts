import {html, nothing, type TemplateResult} from 'lit';
import type {StatusProjection} from '@tesseradb/client';
import {icon} from './icons.js';

/**
 * The eight states, uniformly (design client-components §5.4). Every panel renders them through
 * one `part="state"` region so a host that restyles one has restyled them all. The mapping from
 * the store's `status` is the table's third column, exactly:
 *
 * | state | from `status` |
 * |---|---|
 * | detached | no store |
 * | loading | `idle` or `loading` |
 * | retrying | `retrying` |
 * | shown | `shown` |
 * | empty | `empty` |
 * | refused | `refused` |
 * | expired | the `expired` flag — `expired-token`, a `bad-credential` on a token this store used, or `expiresAt` passed |
 * | stale | `shown` with `stale` |
 *
 * `expired` wins over `refused` because it is a refusal with a name; `stale` is `shown` with the
 * picture marked older than the numbers, and the only state besides `shown` a number appears in
 * — through the formatter, which renders nothing against it.
 *
 * What a state *says* is one line — an answer, never an explanation of how Tessera works
 * (owner, 2026-08-25): the words are the boards' (`StatusStates.png`).
 */
export type PanelState = 'detached' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused' | 'expired' | 'stale';

export function stateOf(status: StatusProjection | null | undefined): PanelState {
  if (!status) return 'detached';
  if (status.expired) return 'expired';
  switch (status.status) {
    case 'idle':
    case 'loading':
      return 'loading';
    case 'retrying':
      return 'retrying';
    case 'empty':
      return 'empty';
    case 'refused':
      return 'refused';
    case 'shown':
      return status.stale ? 'stale' : 'shown';
  }
}

/** Whether a panel in this state shows its content — the numbers, the list, the card. */
export function showsContent(state: PanelState): boolean {
  return state === 'shown' || state === 'stale';
}

export type StateActions = {
  /** The refresh control, present when stale. */
  onRefresh?: (() => void) | null;
  /** Renewal, when the host gave the map an `authorise` property — shown on expiry. */
  onReauthorise?: (() => void) | null;
};

/** The word for a state, as the strip's badge says it. */
export function stateWord(state: PanelState, status: StatusProjection | null | undefined): string {
  switch (state) {
    case 'detached':
      return 'No data';
    case 'loading':
      return status && !status.sessionWarm ? 'Starting session…' : 'Loading';
    case 'retrying':
      return 'Retrying';
    case 'shown':
    case 'empty':
      return 'Current';
    case 'refused':
      return 'Refused';
    case 'expired':
      return 'Session expired';
    case 'stale':
      return 'Corpus updated';
  }
}

/**
 * The state region. Rendered by every panel in the same markup: a `part="state"` carrying
 * `data-state`, and the one line the state needs — a skeleton while loading, an answer when
 * empty, the refusal's code, the refresh control when stale, the way back in when expired.
 */
export function renderState(
  state: PanelState,
  status: StatusProjection | null | undefined,
  actions: StateActions = {}
): TemplateResult | typeof nothing {
  const refusal = status?.refusal ?? null;
  switch (state) {
    case 'detached':
      // Nothing — neither "empty" nor "refused", both of which are answers.
      return html`<span part="state" data-state="detached"></span>`;
    case 'loading':
      return html`<span part="state" data-state="loading"><span class="dot quiet"></span>${stateWord(state, status)}${status && !status.sessionWarm ? nothing : html`<span class="skel" aria-hidden="true"></span>`}</span>`;
    case 'retrying':
      return html`<span part="state" data-state="retrying"><span class="dot warn"></span>Retrying<span class="skel" aria-hidden="true"></span></span>`;
    case 'shown':
      return html`<span part="state" data-state="shown"></span>`;
    case 'empty':
      return html`<span part="state" data-state="empty">Nothing here</span>`;
    case 'refused':
      return html`<span part="state" data-state="refused">${icon('warn', 14)}Refused<span part="refusal" class="mono">${refusal ? `${refusal.code}${refusal.detail ? ' · ' + refusal.detail : ''}` : ''}</span></span>`;
    case 'expired':
      return html`<span part="state" data-state="expired">${icon('lock', 14)}Session expired${actions.onReauthorise
          ? html`<button part="reauthorise" class="btn primary" type="button" @click=${actions.onReauthorise}>Sign in again</button>`
          : nothing}</span>`;
    case 'stale':
      return html`<span part="state" data-state="stale">${icon('clock', 14)}Corpus updated<button part="refresh" class="btn primary" type="button" @click=${actions.onRefresh ?? undefined}>${icon('refresh', 13)}Refresh</button></span>`;
  }
}
