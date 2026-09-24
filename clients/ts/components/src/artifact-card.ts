import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {isFilterLayer, withMember, withoutMember, type Artifact, type ArtifactDetail, type ClauseVerb, type Masked, type Refusal} from '@tesseradb/client';
import {attachedTopics, displayName} from '@tesseradb/deck/internal';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/** A `role="button"` row's keys: Enter and Space press it. */
const onKeys = (press: () => void) => (e: KeyboardEvent) => {
  if (e.key !== 'Enter' && e.key !== ' ') return;
  e.preventDefault();
  press();
};

/** What each kind of drawn shape says on the card. */
const SHAPE_TEXT: Record<'derived' | 'predicate' | 'authored', string> = {
  derived: 'derived: the hull of the members you can see',
  predicate: 'boundary: the same for every viewer',
  authored: 'authored: the same for every viewer'
};

/**
 * `<tessera-artifact-card>`: the selected artifact. Its name, its masked count, its description,
 * layer and key, the kind of shape its layer draws, its parents and children in this view, Fit to
 * cluster, and the `member_of` clause buttons for inside and outside it. A `member_of` clause
 * selects by membership, a drawn `region` by shape; they differ for an artifact whose members are
 * spread across the map. Fit is absent on a layer that draws nothing.
 *
 * The count is the drill-down's, over the whole membership this principal sees; it does not
 * change with the viewport. An artifact with no text and no topic shows {@link UNNAMED} in the
 * headline and its key in its own field. The server gives one refusal for every withheld case.
 */
export class TesseraArtifactCard extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='headline'] {
        margin-bottom: 6px;
      }
      [part='count'] {
        display: flex;
        align-items: baseline;
        gap: 6px;
        margin-bottom: 10px;
      }
      [part='count'] tessera-count::part(count) {
        font-size: 20px;
        font-weight: 500;
        font-family: var(--_tessera-font-mono);
      }
      [part='count'] tessera-count::part(label) {
        font-size: 13px;
        margin-left: 0;
      }
      [part='content'] {
        margin: 0 0 10px;
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      .field .v {
        font-family: var(--_tessera-font-mono);
        font-size: 12px;
      }
      .children-label {
        margin: 12px 0 4px;
      }
      [part='children'] {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      [part='child'] tessera-count::part(count) {
        margin-left: auto;
        color: var(--_tessera-ink-2);
        font-size: 12px;
        font-weight: 400;
      }
      [part='fit'] {
        margin-top: 12px;
      }
      [part='verbs'] {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin-top: 8px;
      }
      [part='verbs'] .btn[aria-pressed='true'] {
        border-color: currentColor;
      }
      [part='verbs'] [data-verb='highlight'][aria-pressed='true'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      .parents-label {
        margin: 12px 0 4px;
      }
      [part='parents'] {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      [part='parent'] tessera-count::part(count) {
        margin-left: auto;
        color: var(--_tessera-ink-2);
        font-size: 12px;
        font-weight: 400;
      }
      [part='close'] {
        display: inline-flex;
        color: var(--_tessera-ink-3);
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

  /**
   * One of the card's clause buttons: a `member_of` clause on this artifact, as a filter or a
   * highlight, inside or outside. The button shows whether the clause is on; clicking a pressed
   * button withdraws it, and pressing the other verb moves the clause.
   */
  private verb(layer: string, artifact: bigint, outside: boolean, verb: ClauseVerb, label: string, name: string | null) {
    const s = this.resolvedStore;
    const held = s?.get('filters').members ?? [];
    const on = held.some((c) => c.layer === layer && c.artifact === artifact && c.outside === outside && c.verb === verb);
    const title =
      verb === 'filter'
        ? outside
          ? 'Narrow the map and every count to what is not in this artifact'
          : "Narrow the map and every count to this artifact's members"
        : "Keep the map and light this artifact's members";
    return html`<button
      part=${outside ? 'outside' : verb === 'filter' ? 'filter' : 'highlight'}
      class="btn"
      type="button"
      data-verb=${verb}
      aria-pressed=${on ? 'true' : 'false'}
      title=${title}
      @click=${() => {
        if (!s) return;
        const members = on
          ? withoutMember(s.get('filters').members, layer, artifact)
          : withMember(s.get('filters').members, {layer, artifact, outside, verb, ...(name === null ? {} : {label: name})});
        s.setMembers(members);
        emit(this, 'tessera-clausechange', {id: idString(artifact), layer, outside, verb, on: !on});
      }}
    >
      ${icon(verb === 'filter' ? 'filter' : 'highlight', 14)}${label}
    </button>`;
  }

  override render() {
    const {artifact, refusal} = this.shown;
    // The heading is the level's title on a levelled layer (County, Admin 2), else the layer's.
    const metaLayers = this.resolvedStore?.get('meta')?.layers ?? [];
    const row = artifact ? this.resolvedStore?.get('artifacts')?.served.find((a) => a.tesseraId === artifact.id) : undefined;
    // A filter layer's artifacts are never served, so the layer falls back to the drill-down's;
    // otherwise the card would offer Fit on a layer with nothing drawn.
    const declaredLayer = row?.layer ?? artifact?.detail.layer;
    const decl = declaredLayer === undefined ? undefined : metaLayers.find((l) => l.name === declaredLayer);
    const what = (row && decl?.levels.find((lv) => lv.level === row.rung)?.title) || decl?.title || 'Artifact';
    const heading = html`<h2 part="title">${what}<button part="close" type="button" aria-label="Close" @click=${() => emit(this, 'tessera-close', {what: 'artifact'})}>${icon('close', 14)}</button></h2>`;
    if (refusal) {
      return html`<div class="panel">${heading}<span part="state" data-state="refused"><span part="refusal">${refusal.code}: ${refusal.detail}</span></span></div>`;
    }
    if (!artifact) {
      if (!this.resolvedStore) return html`<div class="panel">${heading}${renderState('detached', null)}</div>`;
      return html`<div class="panel">${heading}<span part="state" data-state="empty">No cluster selected</span></div>`;
    }
    const s = this.resolvedStore;
    const artifacts = s?.get('artifacts');
    const served = artifacts?.served ?? [];
    const topics = artifacts ? attachedTopics(artifacts, s?.get('meta') ?? null) : new Map<bigint, string>();
    // The artifact's row and children are read from the served set now, on every render.
    const here = served.find((a) => a.tesseraId === artifact.id);
    // On a `dag` layer a child appears on the card of each served parent.
    const children = served.filter((a) => a.parentIds.includes(artifact.id)).sort((a, b) => (a.maskedCount < b.maskedCount ? 1 : a.maskedCount > b.maskedCount ? -1 : 0));
    // Parents from the served set, which holds every parent the map draws. The hierarchy panel
    // asks `POST /v1/artifacts/browse` for the rest.
    const parents = here ? served.filter((a) => here.parentIds.includes(a.tesseraId)).sort((a, b) => (a.maskedCount < b.maskedCount ? 1 : a.maskedCount > b.maskedCount ? -1 : 0)) : [];
    const stale = s?.get('status').stale ?? false;
    const count: Masked = {value: Number(artifact.detail.maskedCount), exact: true};
    const id = idString(artifact.id);
    // The kind of shape the layer draws, so a reader knows whether the outline depends on what
    // they may see.
    const shape = decl?.shape ? {kind: decl.shape, text: SHAPE_TEXT[decl.shape]} : null;
    // A clause made here carries the drawn name as its label, since nothing downstream can resolve it.
    const clauseName = here ? displayName(here, topics) : null;
    return html`<div class="panel">${heading}
      <span part="state" data-state="shown"></span>
      <div part="headline" class="card-title">${(here ? displayName(here, topics) : null) ?? UNNAMED}</div>
      <div part="count"><tessera-count .masked=${count} .stale=${stale} label="members visible to you"></tessera-count></div>
      ${here && here.content.length > 0 && topics.has(here.tesseraId) ? html`<p part="content">${topics.get(here.tesseraId)}</p>` : here && here.content.length > 1 ? html`<p part="content">${here.content.slice(1).join(' · ')}</p>` : nothing}
      <div class="field">
        <div class="k">layer</div><div part="value" class="v">${artifact.detail.layer}</div>
        ${artifact.detail.key ? html`<div class="k">key</div><div part="value" class="v">${artifact.detail.key}</div>` : nothing}
        ${shape ? html`<div class="k">shape</div><div part="shape" class="v" data-kind=${shape.kind}>${shape.text}</div>` : nothing}
      </div>
      ${parents.length > 0
        ? html`<div part="label" class="xs muted parents-label">Parents</div>
            <ul part="parents" class="list">
              ${parents.map(
                (pnt: Artifact) => html`<li part="parent" class="item child" role="button" tabindex="0" data-id=${idString(pnt.tesseraId)} @click=${() => void s?.openArtifact(pnt.tesseraId)} @keydown=${onKeys(() => void s?.openArtifact(pnt.tesseraId))}>
                  <span part="name" class="name">${displayName(pnt, topics) ?? UNNAMED}</span>
                  <tessera-count .masked=${{value: Number(pnt.maskedCount), exact: true} as Masked} .stale=${stale}></tessera-count>
                </li>`
              )}
            </ul>`
        : nothing}
      ${children.length > 0
        ? html`<div part="label" class="xs muted children-label">Children in this view</div>
            <ul part="children" class="list">
              ${children.map(
                (c: Artifact) => html`<li part="child" class="item child" role="button" tabindex="0" data-id=${idString(c.tesseraId)} @click=${() => void s?.openArtifact(c.tesseraId)} @keydown=${onKeys(() => void s?.openArtifact(c.tesseraId))}>
                  <span part="name" class="name">${displayName(c, topics) ?? UNNAMED}</span>
                  <tessera-count .masked=${{value: Number(c.maskedCount), exact: true} as Masked} .stale=${stale}></tessera-count>
                </li>`
              )}
            </ul>`
        : nothing}
      ${
        // A filter layer draws nothing, so there is nothing to fit to.
        decl && isFilterLayer(decl)
          ? nothing
          : html`<button part="fit" class="btn" type="button" @click=${() => emit(this, 'tessera-artifactfit', {id})}>${icon('fit', 14)}Fit to cluster</button>`
      }
      <div part="verbs" role="group" aria-label="This artifact">
        ${this.verb(artifact.detail.layer, artifact.id, false, 'filter', 'Filter to this', clauseName)}
        ${this.verb(artifact.detail.layer, artifact.id, false, 'highlight', 'Highlight this', clauseName)}
        ${this.verb(artifact.detail.layer, artifact.id, true, 'filter', 'Outside this', clauseName)}
      </div>
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-artifact-card', TesseraArtifactCard);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-artifact-card': TesseraArtifactCard;
  }
}
