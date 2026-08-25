import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {type Artifact, type ArtifactsProjection, type Masked, type ServedLineage} from '@tesseradb/client';
import {artifactName} from '@tesseradb/deck';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-artifact-list>` — what the layers served for this view, as a list or a tree built
 * from `parentId`, each with its content and its `Masked` count (design §5.3 tier 2, §6). A
 * click selects and fits.
 *
 * The count is over the whole membership as this principal sees it and does not move with the
 * viewport; only *whether* an artifact appears depends on where you are looking. An absent
 * artifact carries no reason, and one whose parent was not served is a root of what was given —
 * there is no "hidden" row because there is nothing on the wire to fill one from.
 */
export class TesseraArtifactList extends TesseraElement {
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
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 240px;
        overflow-y: auto;
      }
      [part='item'] {
        display: flex;
        justify-content: space-between;
        gap: var(--tessera-space);
        padding: 1px 0;
        padding-left: calc(var(--depth, 0) * 12px);
        cursor: pointer;
      }
      [part='item']:hover,
      [part='item'][data-opened] {
        color: var(--tessera-fg-strong);
      }
      [part='item'][data-opened] [part='name'] {
        color: var(--tessera-accent);
      }
      [part='name'] {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
    `
  ];

  /** By property, for a host with its own served set. */
  @property({attribute: false}) accessor artifacts: ArtifactsProjection | null = null;
  /** How many rows before the list stops and says how many it did not show. */
  @property({type: Number}) accessor rows = 40;

  private get shown(): ArtifactsProjection | null {
    return this.artifacts ?? this.resolvedStore?.get('artifacts') ?? null;
  }

  private open(a: Artifact): void {
    void this.resolvedStore?.openArtifact(a.tesseraId);
    emit(this, 'tessera-artifactselect', {id: idString(a.tesseraId), layer: a.layer});
  }

  override render() {
    const a = this.shown;
    const heading = html`<h2 part="title">Artifacts</h2>`;
    if (!a) return html`${heading}${renderState('detached', null)}`;
    if (a.layers.length === 0) return html`${heading}<span part="state" data-state="empty"><span class="muted">no layer on</span></span>`;
    if (a.status === 'idle' || a.status === 'loading') return html`${heading}${renderState('loading', this.resolvedStore?.get('status') ?? null)}`;
    if (a.status === 'refused') {
      return html`${heading}<span part="state" data-state="refused"><span class="badge">refused</span><span part="refusal">${a.refusal?.code}: ${a.refusal?.detail} — not an empty view</span></span>`;
    }
    if (a.served.length === 0) {
      return html`${heading}<span part="state" data-state="empty"><span class="muted">nothing served here — no artifact in this view has a member this principal can see, or none clears its layer's criterion; the response does not say which</span></span>`;
    }
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const opened = this.resolvedStore?.get('selection').artifact?.id ?? null;
    const listed = flatten(a.lineage);
    const shown = listed.slice(0, this.rows);
    return html`${heading}
      <span part="state" data-state="shown"></span>
      <div class="muted">${a.served.length.toLocaleString('en-GB')} served${a.lineage.linked ? `, ${a.lineage.roots.length.toLocaleString('en-GB')} at the top` : ''}</div>
      <ul part="items">
        ${shown.map(({artifact, depth}) => {
          const masked: Masked = {value: Number(artifact.maskedCount), exact: true};
          return html`<li
            part="item"
            role="button"
            tabindex="0"
            style=${`--depth:${depth}`}
            data-id=${idString(artifact.tesseraId)}
            ?data-opened=${opened === artifact.tesseraId}
            @click=${() => this.open(artifact)}
            @keydown=${(e: KeyboardEvent) => {
              if (e.key === 'Enter' || e.key === ' ') this.open(artifact);
            }}
          >
            <span part="name" title=${artifact.layer}>${artifactName(artifact)}</span>
            <tessera-count part="count" .masked=${masked} .stale=${stale}></tessera-count>
          </li>`;
        })}
      </ul>
      ${listed.length > shown.length ? html`<div class="muted">…and ${(listed.length - shown.length).toLocaleString('en-GB')} more</div>` : nothing}
      <div class="muted">members visible to this principal, over the whole artifact — never its size, and never over the viewport</div>`;
  }
}

/**
 * The served tree as a list: parents immediately above their own children, largest count first
 * at every level — a row's position says what contains it, which a flat sort by count loses.
 */
export function flatten(lineage: ServedLineage): {artifact: Artifact; depth: number}[] {
  const out: {artifact: Artifact; depth: number}[] = [];
  const bigger = (a: Artifact, b: Artifact) => (a.maskedCount < b.maskedCount ? 1 : a.maskedCount > b.maskedCount ? -1 : 0);
  const seen = new Set<bigint>();
  const push = (into: {artifact: Artifact; depth: number}[], of: Artifact[], depth: number) => {
    for (const artifact of [...of].sort(bigger).reverse()) into.push({artifact, depth});
  };
  const stack: {artifact: Artifact; depth: number}[] = [];
  push(stack, lineage.roots, 0);
  while (stack.length > 0) {
    const node = stack.pop()!;
    if (seen.has(node.artifact.tesseraId)) continue;
    seen.add(node.artifact.tesseraId);
    out.push(node);
    push(stack, lineage.childrenOf.get(node.artifact.tesseraId) ?? [], node.depth + 1);
  }
  return out;
}

attachContextRoot();
defineOnce('tessera-artifact-list', TesseraArtifactList);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-artifact-list': TesseraArtifactList;
  }
}
