import {Deck} from '@deck.gl/core';
import {TesseraClient} from '@tessera/client';
import presetsJson from '../presets.json';
import {readConfig} from './config.js';
import {esc} from './html.js';
import {INITIAL_VIEW_STATE, VIEW, buildLayers} from './map.js';
import {renderCounts} from './panels/counts.js';
import {renderPrincipal, type Preset} from './panels/principal.js';
import {coalesce, createStore} from './state.js';

const config = readConfig();
const client = new TesseraClient({
  viewerUrl: config.viewerUrl,
  sessionUrl: config.sessionUrl,
  sessionCredential: config.sessionCredential
});

const presets = presetsJson as Preset[];
const first = presets[0]!;

const store = createStore({
  meta: null,
  session: null,
  slice: '',
  termsLabel: first.label,
  terms: first.terms,
  k: undefined,
  underlayOffset: 0,
  tiles: new Map(),
  lastTimings: null,
  lastBytes: 0,
  inFlight: 0,
  failures: [],
  selected: null,
  selectedWorldXY: null
});

const deck = new Deck({
  parent: document.getElementById('map') as HTMLDivElement,
  views: VIEW,
  initialViewState: INITIAL_VIEW_STATE,
  controller: true,
  layers: []
});

const panels = document.getElementById('panels')!;

function render() {
  panels.innerHTML = renderPrincipal(store.state, presets) + renderCounts(store.state);

  const select = document.getElementById('principal') as HTMLSelectElement | null;
  select?.addEventListener('change', () => {
    const preset = presets[Number(select.value)]!;
    // Re-authorise rather than reuse the token: a different principal is a different mask, and
    // the session is where that lives.
    client
      .authorise(preset.terms)
      .then((session) => {
        store.update((s) => {
          s.session = session;
          s.terms = preset.terms;
          s.termsLabel = preset.label;
          s.tiles.clear();
          s.selected = null;
          s.selectedWorldXY = null;
        });
      })
      .catch((error) => {
        const e = error as {code?: string; detail?: string};
        store.update((s) => {
          s.failures.push({
            tileId: `authorise ${preset.label}`,
            code: e.code ?? 'fetch-failed',
            detail: e.detail ?? String(error),
            at: Date.now()
          });
        });
      });
  });
}

const rerender = coalesce(render);
store.subscribe(
  coalesce(() => {
    deck.setProps({layers: buildLayers(store, client)});
  })
);
store.subscribe(rerender);

async function start() {
  const session = await client.authorise(store.state.terms);
  const meta = await client.meta(session.token);
  store.update((s) => {
    s.session = session;
    s.meta = meta;
    s.slice = meta.slices[0]!.id;
  });
}

start().catch((error) => {
  // Startup failure is never a blank map: an empty viewport and a failed one are opposites, and
  // the whole point of this instrument is that it does not misreport what it could not fetch.
  panels.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
