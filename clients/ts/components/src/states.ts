import {html, nothing, type TemplateResult} from 'lit';
import type {StatusProjection} from '@tesseradb/client';

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

/**
 * The state region. Rendered by every panel in the same markup: a `part="state"` carrying
 * `data-state`, a badge naming the state, and the words the state needs — a skeleton while
 * loading, "empty" as an answer rather than a blank, a refusal's code with a `422` named as the
 * host's bug, the refresh control when stale, the prompt when expired.
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
      return html`<span part="state" data-state="loading"
        ><span class="badge">loading</span
        >${status && !status.sessionWarm
          ? html`<span class="muted">establishing the session — the first request materialises this principal's visible set</span>`
          : html`<span class="skeleton" aria-hidden="true"></span>`}</span
      >`;
    case 'retrying':
      return html`<span part="state" data-state="retrying"
        ><span class="badge">retrying</span><span class="skeleton" aria-hidden="true"></span
      ></span>`;
    case 'shown':
      return html`<span part="state" data-state="shown"></span>`;
    case 'empty':
      return html`<span part="state" data-state="empty"
        ><span class="badge">empty</span><span class="muted">nothing here for this principal — an answer, not a failure</span></span
      >`;
    case 'refused':
      return html`<span part="state" data-state="refused"
        ><span class="badge">refused</span
        ><span part="refusal"
          >${refusal ? `${refusal.code}: ${refusal.detail}` : 'the request was refused'}${refusal?.code === 'contract' ||
          /^422/.test(refusal?.detail ?? '')
            ? ' — a 422 is the host’s bug, not the server’s'
            : ''}</span
        ></span
      >`;
    case 'expired':
      return html`<span part="state" data-state="expired"
        ><span class="badge">expired</span><span part="refusal">the session ended</span
        >${actions.onReauthorise
          ? html`<button part="reauthorise" type="button" @click=${actions.onReauthorise}>sign in again</button>`
          : nothing}</span
      >`;
    case 'stale':
      return html`<span part="state" data-state="stale"
        ><span class="badge">stale</span><span class="muted">the corpus moved — the numbers are current, the picture is older</span
        ><button part="refresh" type="button" @click=${actions.onRefresh ?? undefined}>refresh</button></span
      >`;
  }
}
