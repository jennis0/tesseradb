import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import type {Artifact, ArtifactDetail, Masked, Refusal} from '@tesseradb/client';
import {artifactName} from '@tesseradb/deck';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-artifact-card>` — the opened artifact (design §5.3 tier 2, §6): its content, layer,
 * key, its `Masked` count, and its children from the held set. **Steady during a pan by
 * construction**: the count is the drill-down's, over the whole membership as this principal sees
 * it, and moves with the mask and never with the viewport. Its place in the tree comes from the
 * served set, since a parent is named only where it was served alongside; where the map has
 * moved on and the artifact is no longer among what is held, no place is invented.
 *
 * One refusal covers every withheld case — an identifier naming nothing, one naming a point,
 * one whose layer this principal cannot reach, one suppressed, one below its criterion — and
 * nothing here tells them apart.
 */
export class TesseraArtifactCard extends TesseraElement {
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
      [part='headline'] {
        font-size: 15px;
        color: var(--tessera-fg-strong);
        margin: 2px 0 var(--tessera-space);
      }
      [part='children'] {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      [part='child'] {
        display: flex;
        justify-content: space-between;
        gap: var(--tessera-space);
        cursor: pointer;
      }
      [part='child']:hover {
        color: var(--tessera-fg-strong);
      }
      [part='fit'] {
        width: 100%;
        margin-top: var(--tessera-space);
      }
    `
  ];

  /** By property, for a host feeding the card from its own drill-down. */
  @property({attribute: false}) accessor artifact: {id: bigint; detail: ArtifactDetail} | null = null;
  @property({attribute: false}) accessor refusal: Refusal | null = null;

  private get shown(): {artifact: {id: bigint; detail: ArtifactDetail} | null; refusal: Refusal | null} {
    if (this.artifact || this.refusal) return {artifact: this.artifact, refusal: this.refusal};
    const sel = this.resolvedStore?.get('selection');
    return {artifact: sel?.artifact ?? null, refusal: sel?.artifactRefusal ?? null};
  }

  override render() {
    const {artifact, refusal} = this.shown;
    const heading = html`<h2 part="title">Artifact</h2>`;
    if (refusal) {
      return html`${heading}<span part="state" data-state="refused"><span class="badge">refused</span><span part="refusal">${refusal.code}: ${refusal.detail}</span></span>`;
    }
    if (!artifact) {
      if (!this.resolvedStore) return html`${heading}${renderState('detached', null)}`;
      return html`${heading}<span part="state" data-state="empty"><span class="muted">open an artifact</span></span>`;
    }
    const s = this.resolvedStore;
    const served = s?.get('artifacts').served ?? [];
    // A live projection read: the artifact's own row and its children are whatever the channel
    // has served for the view *now*, and this renders again on every answer.
    const here = served.find((a) => a.tesseraId === artifact.id);
    const children = served.filter((a) => a.parentId === artifact.id).sort((a, b) => (a.maskedCount < b.maskedCount ? 1 : a.maskedCount > b.maskedCount ? -1 : 0));
    const stale = s?.get('status').stale ?? false;
    const count: Masked = {value: Number(artifact.detail.maskedCount), exact: true};
    const id = idString(artifact.id);
    const row = (label: string, value: unknown) => html`<div class="row"><span part="label">${label}</span><span part="value">${value}</span></div>`;
    return html`${heading}
      <span part="state" data-state="shown"></span>
      <div part="headline">${here ? artifactName(here) : (artifact.detail.key ?? `#${id}`)}</div>
      <div part="count"><tessera-count .masked=${count} .stale=${stale} label="members visible to you"></tessera-count></div>
      ${here && here.content.length > 1 ? html`<p part="content">${here.content.slice(1).join(' · ')}</p>` : nothing}
      ${row('layer', artifact.detail.layer)}
      ${artifact.detail.key ? row('key', artifact.detail.key) : nothing}
      ${children.length > 0
        ? html`<div part="label" class="children-label">Children in this view</div>
            <ul part="children">
              ${children.map(
                (c: Artifact) => html`<li part="child" role="button" tabindex="0" data-id=${idString(c.tesseraId)} @click=${() => void s?.openArtifact(c.tesseraId)}>
                  <span part="name">${artifactName(c)}</span>
                  <tessera-count .masked=${{value: Number(c.maskedCount), exact: true} as Masked} .stale=${stale}></tessera-count>
                </li>`
              )}
            </ul>`
        : nothing}
      <button part="fit" type="button" @click=${() => emit(this, 'tessera-artifactfit', {id})}>Fit to cluster</button>`;
  }
}

attachContextRoot();
defineOnce('tessera-artifact-card', TesseraArtifactCard);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-artifact-card': TesseraArtifactCard;
  }
}
