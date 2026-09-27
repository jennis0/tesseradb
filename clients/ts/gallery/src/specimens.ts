import type {Artifact, ArtifactDetail, BrowsePage, PaletteScheme, Projections, RegionProjection, Store} from '@tesseradb/client';
import {NO_MASKED} from '@tesseradb/client';
import type {TesseraMap} from '@tesseradb/components';
import {fakeStore, meta as baseMeta, settle, status, type FakeStore} from '../../components/test/fake-store.js';
import {AREA, FIELDS, LAYERS, MANY_FIELDS, META, TOPIC, VENUES, browseRow, emptyDraft, filtersOf, legendOf, mapState, paper, ranksOf, suggestion, topicArtifacts, withDraft} from './corpus.js';

/**
 * Every element in every state its source handles, each built from its own fake store. A specimen
 * builds its element, and `ready` waits for whatever the state needs after mounting: a click, a
 * typed query, a map's first paint.
 */

export type Specimen = {
  state: string;
  /** Drawn across the page's width by default, as a map is, where the gallery's width is natural. */
  wide?: boolean;
  /** A width the specimen keeps whatever the gallery's width is, because the state is about it. */
  pinned?: number;
  /** Sized to its content where the gallery's width is natural, as a strip is. */
  fit?: boolean;
  build(ctx: Context): HTMLElement;
  ready?(el: HTMLElement): Promise<void>;
};

export type Section = {name: string; tag: string; note?: string; specimens: Specimen[]};

export type Context = {scheme: PaletteScheme};

/** Make an element, set its properties and attributes. */
function make<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<Record<string, unknown>> = {}, attrs: Record<string, string> = {}): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) (el as unknown as Record<string, unknown>)[k] = v;
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  return el;
}

/** A fake store holding the corpus's meta and a shown status, and whatever `over` names. */
function store(over: Partial<Projections> = {}): FakeStore {
  return fakeStore({meta: META, status: status({}), filters: filtersOf(emptyDraft()), ...over});
}

const SHOWN_VIEW = mapState().view;

/** Wait until `test` holds, checking each frame, or give up after `ms` and report it as an error. */
async function until(test: () => boolean, what: string, ms = 5000): Promise<void> {
  const start = performance.now();
  while (!test()) {
    if (performance.now() - start > ms) {
      console.error(`gallery: gave up waiting for ${what}`);
      return;
    }
    await new Promise((r) => requestAnimationFrame(r));
  }
}

const frames = (n: number) => new Promise<void>((r) => {
  const step = (left: number) => (left === 0 ? r() : requestAnimationFrame(() => step(left - 1)));
  step(n);
});

const shadow = (el: Element, selector: string): HTMLElement | null => el.shadowRoot?.querySelector<HTMLElement>(selector) ?? null;

/** Fit the map to its extent once it has a size, and wait for a paint with marks where marks are expected. */
async function mapReady(map: TesseraMap, marks: boolean): Promise<void> {
  await map.updateComplete;
  map.fit();
  if (map.activeStore) await until(() => map.probe.paints > 0 && (!marks || map.probe.marks > 0), 'a map paint');
  await frames(3);
}

const REFUSAL = {code: 'forbidden-view', detail: 'this session’s grants do not reach the view umap; ask the publisher for the reviewer grant'};
const EXPIRED = {code: 'expired-token', detail: 'the viewer token expired at 2026-09-27T14:02:11Z'};

const detail = (a: Artifact): ArtifactDetail => ({layer: a.layer, key: a.key, maskedCount: a.maskedCount, centroid: a.centroid, box: a.box, shape: a.shape});

const heldIds = (n: number) => BigUint64Array.from({length: n}, (_, i) => 1_883_204_551_000_000n + BigInt(i) * 97_531n);

function region(over: Partial<RegionProjection> = {}): RegionProjection {
  const ids = heldIds(500);
  return {
    shape: {kind: 'box', bbox: [0.18, 0.16, 0.44, 0.42]},
    status: 'shown',
    refusal: null,
    visible: {value: 4_212_774, exact: true},
    matched: {value: 4_212_774, exact: true},
    served: {shown: 1_350, total: 4_212_774, exact: true},
    verdict: {exact: true, depth: null},
    held: {ids, positions: new Float32Array(ids.length * 2), count: 1_350},
    ...over
  };
}

const page = (rows: BrowsePage['artifacts'], next: string | null = null): BrowsePage => ({artifacts: rows, parents: [], next});

/** A store whose browse answers the topic hierarchy: the areas, and the topics under the first. */
function hierarchyStore(over: Partial<Projections> = {}, matched = false): FakeStore {
  const s = store(over);
  const {areas, topics} = topicArtifacts();
  const m = (a: Artifact) => (matched ? a.maskedCount / 7n : null);
  s.setBrowse('roots', page(areas.map((a) => browseRow(a, m(a))), 'c1'));
  s.setBrowse(`p:${areas[0]!.tesseraId}`, page(topics.filter((t) => t.parentIds[0] === areas[0]!.tesseraId).map((t) => browseRow(t, m(t)))));
  s.setBrowse('q:galaxy', page([browseRow(topics[4]!)]));
  s.setBrowse('q:zzz', page([]));
  return s;
}

async function expandFirst(el: HTMLElement): Promise<void> {
  await until(() => !!shadow(el, '[part="expander"]'), 'hierarchy roots');
  shadow(el, '[part="expander"]')!.click();
  await until(() => !!shadow(el, '[part="children"] [part="row"]'), 'hierarchy children');
}

async function search(el: HTMLElement, q: string, done: string): Promise<void> {
  await until(() => !!shadow(el, '[part="search"] input'), 'the search box');
  const input = shadow(el, '[part="search"] input') as HTMLInputElement;
  input.value = q;
  input.dispatchEvent(new Event('input'));
  await until(() => !!shadow(el, done), `search results for ${q}`);
}

const legendStore = (colourBy: string | null, over: Partial<Projections> = {}) => store({legend: legendOf(colourBy), ...over});

export const SECTIONS: Section[] = [
  {
    name: 'store',
    tag: 'tessera-store',
    note: 'Renders nothing of its own (display: contents); shown here providing a store to the elements inside it.',
    specimens: [
      {
        state: 'provides its store to a status strip and a layer picker',
        build: () => {
          const el = make('tessera-store', {store: store({view: SHOWN_VIEW})});
          el.append(make('tessera-status'), make('tessera-layer-picker'));
          return el;
        }
      },
      {
        state: 'detached: no store, so its children are detached',
        build: () => {
          const el = make('tessera-store');
          el.append(make('tessera-status'), make('tessera-layer-picker'));
          return el;
        }
      }
    ]
  },
  {
    name: 'count',
    tag: 'tessera-count',
    note: 'Takes no store. Each specimen sets its count or masked property directly.',
    specimens: [
      {state: 'sample, exact, with label', build: () => make('tessera-count', {count: {shown: 48_120, total: 12_465_020, exact: true}, label: 'shown'})},
      {state: 'sample, figure="shown"', build: () => make('tessera-count', {count: {shown: 48_120, total: 12_465_020, exact: true}, label: 'shown'}, {figure: 'shown'})},
      {state: 'sample, inexact (renders nothing)', build: () => make('tessera-count', {count: {shown: 48_120, total: 12_465_020, exact: false}, label: 'shown'})},
      {state: 'masked, exact, large', build: () => make('tessera-count', {masked: {value: 18_204_993_117, exact: true}, label: 'visible'})},
      {state: 'masked, inexact', build: () => make('tessera-count', {masked: {value: 4_212_774, exact: false}, label: 'matched'})},
      {state: 'masked, zero', build: () => make('tessera-count', {masked: {value: 0, exact: true}, label: 'matched'})},
      {state: 'stale (renders nothing)', build: () => make('tessera-count', {masked: {value: 4_212_774, exact: true}, stale: true, label: 'matched'})},
      {state: 'no count set', build: () => make('tessera-count', {label: 'matched'})}
    ]
  },
  {
    name: 'status',
    tag: 'tessera-status',
    specimens: [
      {fit: true, state: 'detached', build: () => make('tessera-status')},
      {fit: true, state: 'loading, session starting', build: () => make('tessera-status', {store: store({status: status({status: 'loading', sessionWarm: false})})})},
      {fit: true, state: 'loading, session warm', build: () => make('tessera-status', {store: store({status: status({status: 'loading'})})})},
      {fit: true, state: 'retrying', build: () => make('tessera-status', {store: store({status: status({status: 'retrying'})})})},
      {fit: true, state: 'shown', build: () => make('tessera-status', {store: store({view: SHOWN_VIEW})})},
      {
        fit: true,
        state: 'shown, highlight active',
        build: () => make('tessera-status', {store: store({view: {...SHOWN_VIEW, highlighting: true, highlighted: {value: 2_812_336, exact: true}}})})
      },
      {
        fit: true,
        state: 'shown, inexact counts',
        build: () => make('tessera-status', {store: store({view: {...SHOWN_VIEW, served: {...SHOWN_VIEW.served, exact: false}, matched: {value: 12_465_020, exact: false}}})})
      },
      {fit: true, state: 'shown, expanded card', build: () => make('tessera-status', {store: store({view: {...SHOWN_VIEW, provisional: 1_204}, replica: {bytes: 48_812_000, points: 6_390, bands: 58, views: 1, lastPlan: null}}), expanded: true})},
      {fit: true, state: 'empty', build: () => make('tessera-status', {store: store({status: status({status: 'empty'})})})},
      {fit: true, state: 'refused', build: () => make('tessera-status', {store: store({status: status({status: 'refused', refusal: REFUSAL})})})},
      {fit: true, state: 'expired, no reauthorise', build: () => make('tessera-status', {store: store({status: status({status: 'refused', expired: true, refusal: EXPIRED})})})},
      {fit: true, state: 'expired, with reauthorise', build: () => make('tessera-status', {store: store({status: status({status: 'refused', expired: true, refusal: EXPIRED})}), reauthorise: () => {}})},
      {fit: true, state: 'stale', build: () => make('tessera-status', {store: store({view: SHOWN_VIEW, status: status({stale: true})})})}
    ]
  },
  {
    name: 'view-picker',
    tag: 'tessera-view-picker',
    specimens: [
      {state: 'detached (renders nothing)', build: () => make('tessera-view-picker')},
      {state: 'plain view current, views and a group offered', build: () => make('tessera-view-picker', {store: store({view: SHOWN_VIEW})})},
      {state: 'long view name current', build: () => make('tessera-view-picker', {store: store({view: {...SHOWN_VIEW, id: 'specter'}})})},
      {state: 'a group current', build: () => make('tessera-view-picker', {store: store({view: {...SHOWN_VIEW, id: 'decade:2010s'}})})},
      {state: 'one layout (renders nothing)', build: () => make('tessera-view-picker', {store: store({meta: baseMeta(), view: {...SHOWN_VIEW, id: 's0'}})})}
    ]
  },
  {
    name: 'key-picker',
    tag: 'tessera-key-picker',
    specimens: [
      {state: 'middle of the group', build: () => make('tessera-key-picker', {store: store({view: {...SHOWN_VIEW, id: 'decade:2010s'}})})},
      {state: 'first key (previous disabled)', build: () => make('tessera-key-picker', {store: store({view: {...SHOWN_VIEW, id: 'decade:1990s'}})})},
      {state: 'last key, long label (next disabled)', build: () => make('tessera-key-picker', {store: store({view: {...SHOWN_VIEW, id: 'decade:2020s'}})})},
      {state: 'plain view (renders nothing)', build: () => make('tessera-key-picker', {store: store({view: SHOWN_VIEW})})},
      {state: 'detached (renders nothing)', build: () => make('tessera-key-picker')}
    ]
  },
  {
    name: 'layer-picker',
    tag: 'tessera-layer-picker',
    specimens: [
      {state: 'detached', build: () => make('tessera-layer-picker')},
      {state: 'loading (no meta yet)', build: () => make('tessera-layer-picker', {store: store({meta: null, status: status({status: 'loading'})})})},
      {state: 'refused (no meta)', build: () => make('tessera-layer-picker', {store: store({meta: null, status: status({status: 'refused', refusal: REFUSAL})})})},
      {state: 'no layers', build: () => make('tessera-layer-picker', {store: store({meta: {...META, layers: []}})})},
      {state: 'none on', build: () => make('tessera-layer-picker', {store: store()})},
      {state: 'topics on, with a filter layer and a long name', build: () => make('tessera-layer-picker', {store: store({artifacts: mapState().artifacts})})}
    ]
  },
  {
    name: 'legend',
    tag: 'tessera-legend',
    specimens: [
      {state: 'detached', build: () => make('tessera-legend')},
      {state: 'loading (no meta yet)', build: () => make('tessera-legend', {store: store({meta: null, status: status({status: 'loading'})})})},
      {state: 'colour by none', build: () => make('tessera-legend', {store: legendStore(null)})},
      {state: 'category', build: () => make('tessera-legend', {store: legendStore('field')})},
      {state: 'category, names loading', build: () => make('tessera-legend', {store: store({legend: legendOf('field', {categories: {}})})})},
      {
        state: 'category, more values than the palette',
        build: () => make('tessera-legend', {store: store({legend: legendOf('field', {categories: {field: MANY_FIELDS}, ranks: {field: ranksOf(MANY_FIELDS)}})})})
      },
      {
        state: 'category, names refused',
        build: () => make('tessera-legend', {store: store({legend: legendOf('field', {categoryErrors: {field: {code: 'vocabulary-withheld', detail: 'arxiv_fields is not listable for this session'}}})})})
      },
      {state: 'numeric ramp', build: () => make('tessera-legend', {store: legendStore('citations')})},
      {state: 'numeric, no values on screen', build: () => make('tessera-legend', {store: store({legend: legendOf('citations', {domains: {}})})})},
      {state: 'a column that is not rendered', build: () => make('tessera-legend', {store: legendStore('published_at')})},
      {state: 'colour by cluster', build: () => make('tessera-legend', {store: legendStore('cluster:topics', {artifacts: mapState().artifacts})})},
      {state: 'selectable, selects only', build: () => make('tessera-legend', {store: legendStore('field', {artifacts: mapState().artifacts}), selectable: true})},
      {state: 'selectable with readout, category', build: () => make('tessera-legend', {store: legendStore('field', {artifacts: mapState().artifacts}), selectable: true, readout: true})},
      {
        state: 'selectable with readout, cluster with a level select',
        build: () => make('tessera-legend', {store: legendStore('cluster:topics', {artifacts: mapState().artifacts}), selectable: true, readout: true})
      }
    ]
  },
  {
    name: 'filter',
    tag: 'tessera-filter',
    specimens: [
      {
        state: 'category checklist',
        build: () => make('tessera-filter', {store: store({filters: filtersOf(emptyDraft(), {suggestions: {field: {q: '', values: FIELDS.map((v) => suggestion(v)), more: false}}})})}, {column: 'field'})
      },
      {
        state: 'category checklist, two chosen',
        build: () =>
          make(
            'tessera-filter',
            {store: store({filters: filtersOf(withDraft({field: {family: 'category', keys: ['cs.CV', 'cs.LG'], verb: 'filter'}}), {suggestions: {field: {q: '', values: FIELDS.map((v) => suggestion(v)), more: false}}})})},
            {column: 'field'}
          )
      },
      {
        state: 'category typeahead, first page',
        build: () => make('tessera-filter', {store: store({filters: filtersOf(emptyDraft(), {suggestions: {field: {q: '', values: FIELDS.slice(0, 6).map((v) => suggestion(v)), more: true}}})})}, {column: 'field'})
      },
      {
        state: 'category typeahead, typed with matches marked, one chosen',
        build: () =>
          make(
            'tessera-filter',
            {
              store: store({
                filters: filtersOf(withDraft({field: {family: 'category', keys: ['stat.ML'], verb: 'filter'}}), {
                  suggestions: {field: {q: 'learn', values: [FIELDS[0]!, FIELDS[3]!].map((v) => suggestion(v, 'learn')), more: false}}
                })
              })
            },
            {column: 'field'}
          ),
        ready: async (el) => {
          await (el as HTMLElement & {updateComplete: Promise<unknown>}).updateComplete;
          await until(() => !!shadow(el, 'input[part="entry"]'), 'the typeahead input');
          const input = shadow(el, 'input[part="entry"]') as HTMLInputElement;
          input.value = 'learn';
          input.dispatchEvent(new Event('input'));
          await until(() => !!shadow(el, '[part="tick"] mark'), 'marked suggestions');
        }
      },
      {state: 'category typeahead, loading', build: () => make('tessera-filter', {store: store()}, {column: 'field'})},
      {
        state: 'category typeahead, refused',
        build: () => make('tessera-filter', {store: store({filters: filtersOf(emptyDraft(), {suggestErrors: {field: {code: 'vocabulary-withheld', detail: 'not listable'}}})})}, {column: 'field'})
      },
      {state: 'text, all words / phrase', build: () => make('tessera-filter', {store: store({filters: filtersOf(withDraft({title: {family: 'text', query: 'score matching', mode: 'phrase', verb: 'filter'}}))})}, {column: 'title'})},
      {state: 'text, all words / any word', build: () => make('tessera-filter', {store: store()}, {column: 'abstract'})},
      {state: 'keyword with operator', build: () => make('tessera-filter', {store: store({filters: filtersOf(withDraft({authors: {family: 'keyword', needle: 'Okonkwo', op: 'prefix', verb: 'filter'}}))})}, {column: 'authors'})},
      {state: 'number range', build: () => make('tessera-filter', {store: store({filters: filtersOf(withDraft({citations: {family: 'numeric', gte: 100, lte: 25_000, verb: 'filter'}}))})}, {column: 'citations'})},
      {
        state: 'date range',
        build: () =>
          make('tessera-filter', {store: store({filters: filtersOf(withDraft({published_at: {family: 'numeric', gte: Date.UTC(2020, 0, 1) * 1000, lte: Date.UTC(2024, 11, 31) * 1000, verb: 'filter'}}))})}, {column: 'published_at'})
      },
      {state: 'unknown column (renders nothing)', build: () => make('tessera-filter', {store: store()}, {column: 'no_such_column'})}
    ]
  },
  {
    name: 'filter-panel',
    tag: 'tessera-filter-panel',
    specimens: [
      {state: 'detached', build: () => make('tessera-filter-panel')},
      {state: 'loading (no meta yet)', build: () => make('tessera-filter-panel', {store: store({meta: null, status: status({status: 'loading'})})})},
      {state: 'refused (no meta)', build: () => make('tessera-filter-panel', {store: store({meta: null, status: status({status: 'refused', refusal: REFUSAL})})})},
      {state: 'nothing filterable', build: () => make('tessera-filter-panel', {store: store({meta: {...META, filterOperands: []}})})},
      {state: 'every control empty', build: () => make('tessera-filter-panel', {store: store({filters: filtersOf(emptyDraft(), {suggestions: {field: {q: '', values: FIELDS.map((v) => suggestion(v)), more: false}}})})})},
      {
        state: 'clauses applied: filter, highlight and member chips',
        build: () =>
          make('tessera-filter-panel', {
            store: store({
              artifacts: mapState().artifacts,
              filters: filtersOf(
                withDraft({
                  field: {family: 'category', keys: ['cs.CV', 'cs.LG', 'stat.ML'], verb: 'filter'},
                  title: {family: 'text', query: 'classifier-free guidance', mode: 'phrase', verb: 'highlight'},
                  authors: {family: 'keyword', needle: 'Okonkwo', op: 'prefix', verb: 'filter'},
                  citations: {family: 'numeric', gte: 100, lte: null, verb: 'filter'},
                  published_at: {family: 'numeric', gte: Date.UTC(2020, 0, 1) * 1000, lte: Date.UTC(2024, 11, 31) * 1000, verb: 'filter'}
                }),
                {
                  suggestions: {field: {q: '', values: FIELDS.map((v) => suggestion(v)), more: false}},
                  members: [
                    {layer: 'topics', artifact: TOPIC(1).tesseraId, outside: false, verb: 'filter'},
                    {layer: 'topics', artifact: TOPIC(5).tesseraId, outside: true, verb: 'filter'},
                    {layer: 'venues', artifact: VENUES.conferences[2]!.tesseraId, outside: false, verb: 'highlight', label: VENUES.conferences[2]!.name}
                  ]
                }
              )
            })
          })
      }
    ]
  },
  {
    name: 'selection',
    tag: 'tessera-selection',
    specimens: [
      {state: 'nothing selected', build: () => make('tessera-selection', {store: store()})},
      {state: 'counting', build: () => make('tessera-selection', {store: store({region: region({status: 'loading', visible: null, matched: NO_MASKED, served: {shown: 0, total: 0, exact: false}, verdict: null, held: {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0}})})})},
      {state: 'box, exact, many held marks', build: () => make('tessera-selection', {store: store({region: region()})})},
      {
        state: 'lasso outside, a cover, a filter narrowing (no visible)',
        build: () =>
          make('tessera-selection', {
            store: store({
              region: region({
                shape: {kind: 'lasso', points: [[0.1, 0.1], [0.5, 0.12], [0.4, 0.6]], outside: true},
                visible: null,
                matched: {value: 9_881_201, exact: false},
                verdict: {exact: false, depth: 9},
                held: {ids: heldIds(6), positions: new Float32Array(12), count: 6}
              })
            })
          })
      },
      {state: 'refused', build: () => make('tessera-selection', {store: store({region: region({status: 'refused', refusal: {code: 'region-too-complex', detail: 'more than 10000 vertices'}})})})},
      {state: 'stale', build: () => make('tessera-selection', {store: store({status: status({stale: true}), region: region()})})}
    ]
  },
  {
    name: 'item-card',
    tag: 'tessera-item-card',
    specimens: [
      {state: 'detached', build: () => make('tessera-item-card')},
      {state: 'no item selected', build: () => make('tessera-item-card', {store: store()})},
      {state: 'nothing under the cursor', build: () => make('tessera-item-card', {store: store(), pick: {kind: 'miss'}})},
      {state: 'broken pick', build: () => make('tessera-item-card', {store: store(), pick: {kind: 'broken', index: 4211, layer: 'marks-p0', hasIds: false, idCount: 0}})},
      {state: 'refused', build: () => make('tessera-item-card', {store: store({selection: {item: null, itemRefusal: {code: 'not-found', detail: 'no item 1883204551907331 in this session'}, artifact: null, artifactRefusal: null}})})},
      {state: 'shown, titled by a field', build: () => make('tessera-item-card', {store: store({view: SHOWN_VIEW, selection: {item: paper(), itemRefusal: null, artifact: null, artifactRefusal: null}})}, {'title-field': 'title'})},
      {state: 'shown, titled by tessera_id', build: () => make('tessera-item-card', {store: store({view: SHOWN_VIEW, selection: {item: paper(), itemRefusal: null, artifact: null, artifactRefusal: null}})})},
      {
        state: 'shown, long content',
        build: () => make('tessera-item-card', {store: store({view: SHOWN_VIEW, selection: {item: paper(true), itemRefusal: null, artifact: null, artifactRefusal: null}})}, {'title-field': 'title'})
      },
      {state: 'fed by property, no store', build: () => make('tessera-item-card', {item: paper(), meta: META}, {'title-field': 'title'})}
    ]
  },
  {
    name: 'artifact-card',
    tag: 'tessera-artifact-card',
    specimens: [
      {state: 'detached', build: () => make('tessera-artifact-card')},
      {state: 'nothing opened', build: () => make('tessera-artifact-card', {store: store()})},
      {state: 'refused', build: () => make('tessera-artifact-card', {store: store({selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: {code: 'not-found', detail: 'no artifact 9184302775000 in this session'}}})})},
      {
        state: 'a topic, with its parent and keywords',
        build: () => make('tessera-artifact-card', {store: store({artifacts: mapState().artifacts, selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(1).tesseraId, detail: detail(TOPIC(1))}, artifactRefusal: null}})})
      },
      {
        state: 'an area, with its children in view',
        build: () => make('tessera-artifact-card', {store: store({artifacts: mapState().artifacts, selection: {item: null, itemRefusal: null, artifact: {id: AREA(0).tesseraId, detail: detail(AREA(0))}, artifactRefusal: null}})})
      },
      {
        state: 'filter and highlight clauses on',
        build: () =>
          make('tessera-artifact-card', {
            store: store({
              artifacts: mapState().artifacts,
              filters: filtersOf(emptyDraft(), {
                members: [
                  {layer: 'topics', artifact: TOPIC(0).tesseraId, outside: false, verb: 'highlight'},
                  {layer: 'topics', artifact: TOPIC(0).tesseraId, outside: true, verb: 'filter'}
                ]
              }),
              selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(0).tesseraId, detail: detail(TOPIC(0))}, artifactRefusal: null}
            })
          })
      },
      {
        state: 'unnamed artifact',
        build: () => make('tessera-artifact-card', {store: store({artifacts: mapState().artifacts, selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(8).tesseraId, detail: detail(TOPIC(8))}, artifactRefusal: null}})})
      },
      {
        state: 'a filter layer’s artifact (no Fit)',
        build: () =>
          make('tessera-artifact-card', {
            store: store({selection: {item: null, itemRefusal: null, artifact: {id: VENUES.conferences[0]!.tesseraId, detail: {layer: 'venues', key: 'neurips', maskedCount: 1_402_118n, centroid: null, box: null, shape: null}}, artifactRefusal: null}})
          })
      },
      {
        state: 'stale',
        build: () =>
          make('tessera-artifact-card', {store: store({status: status({stale: true}), artifacts: mapState().artifacts, selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(1).tesseraId, detail: detail(TOPIC(1))}, artifactRefusal: null}})})
      }
    ]
  },
  {
    name: 'artifact-list',
    tag: 'tessera-artifact-list',
    specimens: [
      {state: 'detached', build: () => make('tessera-artifact-list')},
      {state: 'no layer on', build: () => make('tessera-artifact-list', {store: store()})},
      {state: 'loading', build: () => make('tessera-artifact-list', {store: store({artifacts: {...mapState().artifacts, status: 'loading', served: []}})})},
      {state: 'refused', build: () => make('tessera-artifact-list', {store: store({artifacts: {...mapState().artifacts, status: 'refused', served: [], refusal: {code: 'layer-withheld', detail: 'topics is not granted to this session'}}})})},
      {state: 'nothing in this view', build: () => {
        const a = mapState().artifacts;
        return make('tessera-artifact-list', {store: store({artifacts: {...a, served: [], lineage: {...a.lineage, roots: [], childrenOf: new Map()}}})});
      }},
      {
        state: 'a tree, one opened',
        build: () => make('tessera-artifact-list', {store: store({artifacts: mapState().artifacts, selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(1).tesseraId, detail: detail(TOPIC(1))}, artifactRefusal: null}})})
      },
      {state: 'more rows than it shows (rows=6)', build: () => make('tessera-artifact-list', {store: store({artifacts: mapState().artifacts})}, {rows: '6'})},
      {state: 'stale', build: () => make('tessera-artifact-list', {store: store({status: status({stale: true}), artifacts: mapState().artifacts})})}
    ]
  },
  {
    name: 'hierarchy',
    tag: 'tessera-hierarchy',
    specimens: [
      {state: 'detached', build: () => make('tessera-hierarchy')},
      {state: 'loading (no meta yet)', build: () => make('tessera-hierarchy', {store: store({meta: null, status: status({status: 'loading'})})})},
      {state: 'no hierarchical layers', build: () => make('tessera-hierarchy', {store: store({meta: {...META, layers: LAYERS.filter((l) => l.hierarchy.kind === 'flat')}})})},
      {
        state: 'roots loading',
        build: () => {
          const s = store();
          s.browse = () => new Promise(() => {});
          return make('tessera-hierarchy', {store: s});
        }
      },
      {state: 'roots, with More', build: () => make('tessera-hierarchy', {store: hierarchyStore()}), ready: (el) => until(() => !!shadow(el, '[part="row"]'), 'hierarchy roots')},
      {state: 'a row expanded', build: () => make('tessera-hierarchy', {store: hierarchyStore()}), ready: expandFirst},
      {
        state: 'a filter set: matched counts, a highlight and a filter on rows',
        build: () =>
          make('tessera-hierarchy', {
            store: hierarchyStore(
              {
                filters: filtersOf(withDraft({field: {family: 'category', keys: ['cs.CV'], verb: 'filter'}}), {
                  members: [
                    {layer: 'topics', artifact: AREA(0).tesseraId, outside: false, verb: 'highlight'},
                    {layer: 'topics', artifact: TOPIC(1).tesseraId, outside: false, verb: 'filter'}
                  ]
                })
              },
              true
            )
          }),
        ready: expandFirst
      },
      {state: 'search with a match', build: () => make('tessera-hierarchy', {store: hierarchyStore()}), ready: (el) => search(el, 'galaxy', '[part="row"]')},
      {state: 'search with no match', build: () => make('tessera-hierarchy', {store: hierarchyStore()}), ready: (el) => search(el, 'zzz', '[part="state"][data-state="empty"]')},
      {
        state: 'refused',
        build: () => {
          const s = store();
          s.browse = () => Promise.reject({code: 'layer-withheld', detail: 'topics is not granted to this session'});
          return make('tessera-hierarchy', {store: s});
        },
        ready: (el) => until(() => !!shadow(el, '[part="refusal"]'), 'the refusal')
      },
      {
        state: 'filter layer, a child under two parents',
        build: () => {
          const s = store();
          s.setBrowse('roots', page(VENUES.roots));
          s.setBrowse(`p:${VENUES.roots[0]!.tesseraId}`, page(VENUES.conferences, 'c2'));
          return make('tessera-hierarchy', {store: s}, {layer: 'venues'});
        },
        ready: expandFirst
      }
    ]
  },
  {
    name: 'map',
    tag: 'tessera-map',
    specimens: mapSpecimens()
  },
  {
    name: 'explorer',
    tag: 'tessera-explorer',
    specimens: explorerSpecimens()
  }
];

/** A store holding the whole map state: marks, tiles, view, artifacts and a legend. */
function mapStore(ctx: Context, colourBy: string | null, opts: Parameters<typeof mapState>[0] = {}, over: Partial<Projections> = {}): FakeStore {
  const m = mapState({scheme: ctx.scheme, ...opts});
  return store({...m, legend: legendOf(colourBy), ...over});
}

function mapSpecimens(): Specimen[] {
  const map = (s: Store | null, attrs: Record<string, string> = {}) => make('tessera-map', s ? {store: s} : {}, attrs);
  const ready = (marks: boolean) => (el: HTMLElement) => mapReady(el as TesseraMap, marks);
  return [
    {state: 'detached', wide: true, build: () => map(null), ready: ready(false)},
    {state: 'loading', wide: true, build: () => map(store({status: status({status: 'loading'})})), ready: ready(false)},
    {state: 'coloured by field (category)', wide: true, build: (ctx) => map(mapStore(ctx, 'field')), ready: ready(true)},
    {state: 'coloured by citations, density wash on', wide: true, build: (ctx) => map(mapStore(ctx, 'citations'), {wash: ''}), ready: ready(true)},
    {
      state: 'coloured by cluster, a topic opened (outline and labels)',
      wide: true,
      build: (ctx) => map(mapStore(ctx, 'cluster:topics', {}, {selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(0).tesseraId, detail: detail(TOPIC(0))}, artifactRefusal: null}})),
      ready: ready(true)
    },
    {state: 'highlight active (cs.CV lit)', wide: true, build: (ctx) => map(mapStore(ctx, 'field', {highlight: (m) => m.field === 2})), ready: ready(true)},
    {
      state: 'a box selected, toolbar top-right',
      wide: true,
      build: (ctx) => map(mapStore(ctx, 'field', {}, {region: region()}), {'controls-corner': 'top-right'}),
      ready: ready(true)
    },
    {state: 'refused', wide: true, build: () => map(store({status: status({status: 'refused', refusal: REFUSAL})})), ready: ready(false)},
    {state: 'expired', wide: true, build: () => map(store({status: status({status: 'refused', expired: true, refusal: EXPIRED})})), ready: ready(false)},
    {state: 'empty', wide: true, build: () => map(store({status: status({status: 'empty'})})), ready: ready(false)}
  ];
}

function explorerSpecimens(): Specimen[] {
  const explorer = (s: Store, attrs: Record<string, string> = {}) => {
    const el = make('tessera-explorer', {store: s}, {'title-field': 'title', 'tooltip-fields': 'field year citations', ...attrs});
    el.style.setProperty('--tessera-explorer-height', '640px');
    return el;
  };
  const readyFor = (marks: boolean) => async (el: HTMLElement) => {
    await settle(el.parentElement!);
    const map = (el as HTMLElementTagNameMap['tessera-explorer']).map;
    if (map) await mapReady(map, marks);
    await settle(el.parentElement!);
  };
  const ready = readyFor(true);
  const full = (ctx: Context, over: Partial<Projections> = {}) =>
    mapStore(ctx, 'field', {}, {
      view: {...mapState().view},
      filters: filtersOf(withDraft({citations: {family: 'numeric', gte: 100, lte: null, verb: 'filter'}}), {suggestions: {field: {q: '', values: FIELDS.map((v) => suggestion(v)), more: false}}}),
      ...over
    });
  return [
    {
      state: 'docked, an item selected',
      wide: true,
      build: (ctx) => explorer(full(ctx, {selection: {item: paper(), itemRefusal: null, artifact: null, artifactRefusal: null}}), {layout: 'docked'}),
      ready
    },
    {
      state: 'overlay, a topic opened',
      wide: true,
      build: (ctx) => explorer(full(ctx, {selection: {item: null, itemRefusal: null, artifact: {id: TOPIC(1).tesseraId, detail: detail(TOPIC(1))}, artifactRefusal: null}}), {layout: 'overlay'}),
      ready
    },
    {
      state: 'narrow container, the filters sheet open',
      pinned: 420,
      build: (ctx) => explorer(full(ctx), {layout: 'docked'}),
      ready: async (el) => {
        await ready(el);
        await until(() => !!shadow(el, '[role="tab"][data-sheet="filters"]'), 'the filters tab');
        shadow(el, '[role="tab"][data-sheet="filters"]')?.click();
        await until(() => !!shadow(el, '[part="sheet"]'), 'the filters sheet');
        await settle(el.parentElement!);
      }
    },
    {state: 'loading (no meta yet)', wide: true, build: () => explorer(store({meta: null, status: status({status: 'loading', sessionWarm: false})}), {layout: 'docked'}), ready: readyFor(false)}
  ];
}
