import {Deck, OrthographicView} from '@deck.gl/core';
import {createStore, type Store} from '@tesseradb/client';
import {TesseraLayer, viewInputOf} from '@tesseradb/deck';
import {fitWorld, worldEdge, type ViewState} from './view.js';

/**
 * `TesseraLayer` in a `Deck` the host builds. The host owns the view and the camera, tells the
 * store where the camera is through `viewInputOf`, and draws a layer of its own under the marks.
 * The layer makes its GPU buffers on the deck's device and releases them itself.
 */
const parent = document.getElementById('map') as HTMLDivElement;
const size = () => ({width: parent.clientWidth || 1, height: parent.clientHeight || 1});

let store: Store | null = null;
let viewState: ViewState = fitWorld(size().width, size().height);
let unsubscribe: (() => void) | null = null;

const deck = new Deck<OrthographicView>({
  parent,
  views: new OrthographicView({id: 'ortho', flipY: true}),
  viewState,
  controller: true,
  layers: [],
  onViewStateChange: ({viewState: next}) => {
    const v = next as {target: number[]; zoom: number};
    viewState = {...viewState, target: [v.target[0]!, v.target[1]!, 0], zoom: v.zoom};
    deck.setProps({viewState});
    tell();
    return next;
  }
});

/** Tell the store where the camera is looking. Nothing is sent before `meta` gives it a frame. */
function tell(): void {
  if (!store) return;
  const {width, height} = size();
  const input = viewInputOf(store, viewState, width, height);
  if (input) store.setView(input);
}

/** The layer list: the host's own layer, then Tessera's over the current store. */
function draw(): void {
  deck.setProps({layers: [worldEdge(), store ? new TesseraLayer({id: 'tessera', store}) : null]});
}

/** The newest sign-in asked for; an older one's token that lands later is dropped. */
let opening = 0;

async function open(user: string): Promise<void> {
  const ticket = ++opening;
  unsubscribe?.();
  unsubscribe = null;
  store?.dispose();
  store = null;
  draw();
  const {token} = (await (await fetch(`/token?user=${encodeURIComponent(user)}`, {method: 'POST'})).json()) as {token: string};
  if (ticket !== opening) return;
  const s = createStore({viewerUrl: location.origin, token});
  store = s;
  // The first view goes out once `meta` has arrived: the store's frame comes with it.
  unsubscribe = s.subscribe('meta', (meta) => {
    if (meta) tell();
  });
  draw();
}

new ResizeObserver(() => tell()).observe(parent);

const principal = document.getElementById('principal') as HTMLSelectElement;
const users = (await (await fetch('/users')).json()) as {name: string; label: string}[];
for (const u of users) principal.add(new Option(u.label, u.name));
principal.value = users.at(-1)?.name ?? '';
principal.addEventListener('change', () => void open(principal.value));
await open(principal.value);
