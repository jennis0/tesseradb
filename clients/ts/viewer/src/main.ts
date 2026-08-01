import {Deck} from '@deck.gl/core';
import {TesseraClient} from '@tessera/client';
import {readConfig} from './config.js';
import {esc} from './html.js';
import {INITIAL_VIEW_STATE, VIEW, buildLayers} from './map.js';
import {coalesce, createStore} from './state.js';

const config = readConfig();
const client = new TesseraClient({
  viewerUrl: config.viewerUrl,
  sessionUrl: config.sessionUrl,
  sessionCredential: config.sessionCredential
});

const store = createStore({
  meta: null,
  session: null,
  slice: '',
  termsLabel: 'term 0',
  terms: ['0'],
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

store.subscribe(
  coalesce(() => {
    deck.setProps({layers: buildLayers(store, client)});
  })
);

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
