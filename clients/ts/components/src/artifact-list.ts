import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {type Artifact, type ArtifactsProjection, type Masked, type ServedLineage} from '@tesseradb/client';
import {artifactName} from '@tesseradb/deck';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-artifact-list>` — what the layers served for this view (design §5.3 tier 2, §6): the
 * boards' *IN VIEW · N clusters* list, a tree built from `parentId` with a row's children beneath
 * it, each with its name and its `Masked` count, the opened one highlighted. A click selects —
 * the card and the outline — and never moves the camera.
 *
 * The count is over the whole membership as this principal sees it and does not move with the
 * viewport; only *whether* an artifact appears depends on where you are looking.
 */
export class TesseraArtifactList extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 240px;
        overflow-y: auto;
      }
      [part='item'] {
        padding-left: calc(6px + var(--depth, 0) * 18px);
      }
      [part='item'] tessera-count::part(count) {
        margin-left: auto;
        color: var(--tessera-ink-2);
        font-size: 12px;
        font-weight: 400;
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
    const heading = (summary: unknown = nothing) => html`<h2 part="title">In view<span class="summary">${summary}</span></h2>`;
    if (!a) return html`<div class="panel">${heading()}${renderState('detached', null)}</div>`;
    if (a.layers.length === 0) return html`<div class="panel">${heading()}<span part="state" data-state="empty">No layer on</span></div>`;
    if (a.status === 'idle' || a.status === 'loading') return html`<div class="panel">${heading()}${renderState('loading', this.resolvedStore?.get('status') ?? null)}</div>`;
    if (a.status === 'refused') {
      return html`<div class="panel">${heading()}<span part="state" data-state="refused"><span part="refusal">${a.refusal?.code}: ${a.refusal?.detail}</span></span></div>`;
    }
    if (a.served.length === 0) return html`<div class="panel">${heading()}<span part="state" data-state="empty">Nothing in this view</span></div>`;
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const opened = this.resolvedStore?.get('selection').artifact?.id ?? null;
    const listed = flatten(a.lineage);
    const shown = listed.slice(0, this.rows);
    const n = a.lineage.linked ? a.lineage.roots.length : a.served.length;
    return html`<div class="panel">${heading(`${n.toLocaleString('en-GB')} cluster${n === 1 ? '' : 's'}`)}
      <span part="state" data-state="shown"></span>
      <ul part="items" class="list">
        ${shown.map(({artifact, depth}) => {
          const masked: Masked = {value: Number(artifact.maskedCount), exact: true};
          return html`<li
            part="item"
            class="item"
            role="button"
            tabindex="0"
            style=${`--depth:${depth}`}
            data-id=${idString(artifact.tesseraId)}
            aria-selected=${opened === artifact.tesseraId ? 'true' : 'false'}
            ?data-opened=${opened === artifact.tesseraId}
            @click=${() => this.open(artifact)}
            @keydown=${(e: KeyboardEvent) => {
              if (e.key === 'Enter' || e.key === ' ') this.open(artifact);
            }}
          >
            <span part="name" class="name" title=${artifact.layer}>${artifactName(artifact)}</span>
            <tessera-count part="count" .masked=${masked} .stale=${stale}></tessera-count>
          </li>`;
        })}
      </ul>
      ${listed.length > shown.length ? html`<div class="muted xs">and ${(listed.length - shown.length).toLocaleString('en-GB')} more</div>` : nothing}
    </div>`;
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
