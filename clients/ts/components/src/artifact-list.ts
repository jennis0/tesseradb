import {css, html, nothing, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {type Artifact, type ArtifactsProjection, type Masked, type ServedLineage} from '@tesseradb/client';
import {attachedTopics, displayName} from '@tesseradb/deck/internal';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {refusalText, renderState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * The artifacts served for the current view, as a tree built from their parents, each row with its
 * name and masked count; the opened artifact is highlighted. Pressing a row opens the artifact's
 * card and outline without moving the camera. An artifact with no name shows a dash.
 *
 * The count is the artifact's whole membership as the viewer sees it, not the part in view; the
 * view decides only whether an artifact is listed.
 *
 * @summary The artifacts served for the view, with their counts.
 * @tagname tessera-artifact-list
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactselect']>} tessera-artifactselect - A
 *   row was pressed.
 * @csspart title - The heading, with how many are listed.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "Layer unavailable", with `data-code` set to the refusal's code.
 * @csspart items - The list.
 * @csspart item - One artifact, with `data-id`, and `data-opened` on the opened one.
 * @csspart name - An artifact's name, with `data-unnamed` where it has none.
 * @csspart count - An artifact's `<tessera-count>`.
 * @csspart more - The "N more" button, which shows the next `rows` rows.
 */
export class TesseraArtifactList extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='more'] {
        margin-top: 6px;
      }
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 240px;
        overflow-y: auto;
      }
      [part='name'][data-unnamed] {
        color: var(--_tessera-ink-2);
      }
      [part='item'] tessera-count::part(count) {
        margin-left: auto;
        color: var(--_tessera-ink-2);
        font-size: 12px;
        font-weight: 400;
      }
    `
  ];

  /** The served artifacts to list in place of the store's `artifacts` projection. */
  @property({attribute: false}) accessor artifacts: ArtifactsProjection | null = null;
  /** How many rows the list shows before it offers the rest under "N more". */
  @property({type: Number}) accessor rows = 40;
  /** Rows shown beyond `rows` after "N more" was pressed. */
  @state() private accessor extra = 0;

  private get shown(): ArtifactsProjection | null {
    return this.artifacts ?? this.resolvedStore?.get('artifacts') ?? null;
  }

  private open(a: Artifact): void {
    void this.resolvedStore?.openArtifact(a.tesseraId);
    emit(this, 'tessera-artifactselect', {id: idString(a.tesseraId), layer: a.layer});
  }

  override render(): TemplateResult | typeof nothing {
    const a = this.shown;
    const heading = (summary: unknown = nothing) => html`<h2 part="title">In view<span class="summary">${summary}</span></h2>`;
    if (!a) return html`<div class="panel">${heading()}${renderState('detached', null)}</div>`;
    if (a.layers.length === 0) return html`<div class="panel">${heading()}<span part="state" data-state="empty">No layer on</span></div>`;
    if (a.status === 'idle' || a.status === 'loading') return html`<div class="panel">${heading()}${renderState('loading', this.resolvedStore?.get('status') ?? null)}</div>`;
    if (a.status === 'refused') {
      return html`<div class="panel">${heading()}<span part="state" data-state="refused"><span class="dot refuse"></span>${refusalText('Layer unavailable', a.refusal?.code)}</span></div>`;
    }
    if (a.served.length === 0) return html`<div class="panel">${heading()}<span part="state" data-state="empty">Nothing in view</span></div>`;
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const opened = this.resolvedStore?.get('selection').artifact?.id ?? null;
    const topics = attachedTopics(a, this.resolvedStore?.get('meta') ?? null);
    const listed = flatten(a.lineage).filter(({artifact}) => !topics.size || !(this.resolvedStore?.get('meta')?.layers.find((l) => l.name === artifact.layer)?.depsOn.length));
    const shown = listed.slice(0, this.rows + this.extra);
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
            <span part="name" class="name" title=${artifact.layer} ?data-unnamed=${displayName(artifact, topics) === null}
              >${displayName(artifact, topics) ?? UNNAMED}</span
            >
            <tessera-count part="count" .masked=${masked} .stale=${stale}></tessera-count>
          </li>`;
        })}
      </ul>
      ${listed.length > shown.length
        ? html`<button class="more-link" part="more" type="button" @click=${() => (this.extra += this.rows)}>${(listed.length - shown.length).toLocaleString('en-GB')} more</button>`
        : nothing}
    </div>`;
  }
}

/**
 * The served tree as a list: parents immediately above their children, largest count first at
 * every level.
 *
 * An artifact appears once. On a `dag` layer a child may be served under several parents; it is
 * listed under the first this depth-first walk reaches (roots in count order, ties by lowest
 * identifier), so the placement depends on the served set and not on row order.
 */
function flatten(lineage: ServedLineage): {artifact: Artifact; depth: number}[] {
  const out: {artifact: Artifact; depth: number}[] = [];
  const bigger = (a: Artifact, b: Artifact) =>
    a.maskedCount < b.maskedCount ? 1 : a.maskedCount > b.maskedCount ? -1 : a.tesseraId < b.tesseraId ? -1 : a.tesseraId > b.tesseraId ? 1 : 0;
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
