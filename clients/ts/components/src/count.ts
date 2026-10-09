import {LitElement, css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {formatCount, formatMasked, type Count, type Masked} from '@mosaica/client';
import {attachContextRoot, defineOnce} from './define.js';
import {tokens} from './tokens.js';

/**
 * Renders a `Count` or a `Masked` through the client's `formatCount` and `formatMasked`, in the text
 * face with tabular figures. A sample (`count`) shows both figures, as `4,812 of 12,465`, or neither
 * when the count is inexact; a scalar (`masked`) shows one figure, marked `≈` when inexact. Nothing
 * is rendered while `stale` is set. With both properties set, `count` is rendered.
 *
 * This element takes no store; the host or another element sets its properties.
 *
 * @summary A sample count or a masked count, formatted.
 * @tagname mosaica-count
 * @category Elements
 * @csspart count - The figure. Carries `data-kind` (`sample`, `scalar` or `none`), `data-exact`
 *   (`true` or `false`), `data-empty` (`true` when nothing was rendered) and, under
 *   `figure="shown"`, `data-total` with the total.
 * @csspart label - The `label` text after the figure, rendered only when the figure is.
 */
export class MosaicaCount extends LitElement {
  static override styles = [
    tokens,
    css`
      :host {
        display: inline;
      }
      [part='count'] {
        color: var(--_mosaica-ink);
        font-variant-numeric: tabular-nums;
        font-weight: 500;
      }
      [part='label'] {
        color: var(--_mosaica-ink-2);
        margin-left: 0.4em;
      }
    `
  ];

  /** A sample count: how many marks are shown of how many are served. */
  @property({attribute: false}) accessor count: Count | null = null;
  /** A masked count over the viewer's visible set, rendered where `count` is not set. */
  @property({attribute: false}) accessor masked: Masked | null = null;
  /** Renders nothing, since the figures predate a change to the corpus. */
  @property({type: Boolean}) accessor stale = false;
  /** Text rendered after the figure, such as `matched`. */
  @property() accessor label = '';
  /**
   * `both` renders a sample as `shown of total`; `shown` renders the shown figure alone and puts
   * the total on the part's `data-total`. The figure is empty exactly when the pair would be.
   */
  @property() accessor figure: 'both' | 'shown' = 'both';
  /** Shortens figures of a thousand or more to one decimal, as `16.8M`, where room is short. */
  @property({type: Boolean}) accessor compact = false;

  override render(): TemplateResult | typeof nothing {
    const kind = this.count ? 'sample' : this.masked ? 'scalar' : 'none';
    const opts = {stale: this.stale, compact: this.compact};
    const pair = this.count ? formatCount(this.count, opts) : this.masked ? formatMasked(this.masked, opts) : '';
    const text = this.count && this.figure === 'shown' && pair !== '' ? pair.split(' of ')[0]! : pair;
    const total = this.count && this.figure === 'shown' && pair !== '' ? pair.split(' of ')[1] : undefined;
    const exact = this.count ? this.count.exact : this.masked ? this.masked.exact : false;
    return html`<span
        part="count"
        data-kind=${kind}
        data-exact=${exact ? 'true' : 'false'}
        data-empty=${text === '' ? 'true' : 'false'}
        data-total=${total ?? nothing}
        >${text}</span
      >${this.label && text !== '' ? html`<span part="label">${this.label}</span>` : nothing}`;
  }
}

attachContextRoot();
defineOnce('mosaica-count', MosaicaCount);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-count': MosaicaCount;
  }
}
