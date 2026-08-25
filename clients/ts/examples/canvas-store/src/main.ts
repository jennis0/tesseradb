import {createStore, formatCount, formatMasked, type MarksProjection, type Store} from '@tesseradb/client';
import {fitWorld, pan, toScreen, worldBox, zoomAt, type Camera} from './camera.js';

/**
 * C2: the store under a host's own camera and a 2D canvas, with none of Tessera's rendering
 * (design client-components §4, §9 step 4 — the check that the store is usable on its own).
 * Three calls: `createStore`, `setView` on every camera change, `subscribe('marks')` to draw.
 * The counts are formatted by the host with the package's two formatters, so a sample shows
 * both figures or neither and a masked scalar shows one — and nothing against a stale view.
 */
const canvas = document.getElementById('map') as HTMLCanvasElement;
const ctx = canvas.getContext('2d')!;
const el = (id: string) => document.getElementById(id)!;

let store: Store | null = null;
let cam: Camera | null = null;
let marks: MarksProjection | null = null;
let unsubscribe: (() => void)[] = [];

const size = () => {
  const dpr = devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
    canvas.width = w * dpr;
    canvas.height = h * dpr;
  }
  return {w, h, dpr};
};

/** Tell the store where the camera is looking: the canvas's world bbox, in data coordinates. */
function tell() {
  if (!store || !cam) return;
  const {w, h} = size();
  const [x0, y0, x1, y1] = worldBox(cam, w, h);
  const a = store.dataXY(x0, y0);
  const b = store.dataXY(x1, y1);
  store.setView({bbox: [Math.min(a[0], b[0]), Math.min(a[1], b[1]), Math.max(a[0], b[0]), Math.max(a[1], b[1])], width: w, height: h});
  draw();
}

/** Dots from the marks projection: exact bands whole, stand-in pieces by their index list or prefix. */
function draw() {
  const {w, h, dpr} = size();
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = '#111';
  ctx.fillRect(0, 0, w, h);
  if (!cam || !marks) return;
  const dot = (positions: Float32Array, i: number, colour: string) => {
    const [sx, sy] = toScreen(cam!, w, h, positions[2 * i]!, positions[2 * i + 1]!);
    if (sx < 0 || sy < 0 || sx > w || sy > h) return;
    ctx.fillStyle = colour;
    ctx.fillRect(sx, sy, 1.5, 1.5);
  };
  for (const piece of marks.standIn) {
    const p = piece.band.positions;
    if (piece.indices) for (const i of piece.indices) dot(p, i, '#557');
    else for (let i = 0; i < piece.limit; i++) dot(p, i, '#557');
  }
  for (const band of marks.bands) {
    const p = band.positions;
    for (let i = 0; i < band.ids.length; i++) dot(p, i, '#9cf');
  }
}

async function open(user: string) {
  for (const u of unsubscribe) u();
  unsubscribe = [];
  store?.dispose();
  marks = null;
  cam = null;
  draw();
  const {token} = (await (await fetch(`/token?user=${encodeURIComponent(user)}`, {method: 'POST'})).json()) as {token: string};
  const s = createStore({viewerUrl: location.origin, token});
  store = s;
  unsubscribe.push(
    // The camera opens on the whole extent once `meta` says what the extent is.
    s.subscribe('meta', (meta) => {
      if (!meta || cam) return;
      const {w, h} = size();
      cam = fitWorld(w, h);
      tell();
    }),
    s.subscribe('marks', (m) => {
      marks = m;
      el('marks').textContent = `${m.bands.length} exact bands, ${m.standIn.length} stand-in pieces`;
      draw();
    }),
    s.subscribe('status', (st) => {
      const state = st.status === 'shown' && st.stale ? 'stale' : st.status;
      const status = el('status');
      status.dataset['state'] = state;
      status.textContent = st.refusal ? `${state} — ${st.refusal.code}: ${st.refusal.detail}` : state;
      (el('refresh') as HTMLButtonElement).hidden = !st.stale;
      numbers();
    }),
    s.subscribe('view', numbers)
  );
}

/** The host's own count panel: the type says which formatter, and stale blanks all three. */
function numbers() {
  if (!store) return;
  const st = store.get('status');
  const v = store.get('view');
  const opts = {stale: st.stale};
  const shown = st.status === 'shown';
  el('shown').textContent = shown ? formatCount(v.served, opts) : '';
  el('matched').textContent = shown ? formatMasked(v.matched, opts) : '';
  el('visible').textContent = shown ? formatMasked(v.visible, opts) : '';
}

el('refresh').addEventListener('click', () => store?.refresh());

// ---- the camera's gestures -------------------------------------------------------------------
let drag: {x: number; y: number} | null = null;
canvas.addEventListener('pointerdown', (e) => {
  drag = {x: e.clientX, y: e.clientY};
  canvas.setPointerCapture(e.pointerId);
});
canvas.addEventListener('pointermove', (e) => {
  if (!drag || !cam) return;
  cam = pan(cam, e.clientX - drag.x, e.clientY - drag.y);
  drag = {x: e.clientX, y: e.clientY};
  tell();
});
canvas.addEventListener('pointerup', () => (drag = null));
canvas.addEventListener('wheel', (e) => {
  if (!cam) return;
  e.preventDefault();
  const {w, h} = size();
  const r = canvas.getBoundingClientRect();
  cam = zoomAt(cam, w, h, e.clientX - r.left, e.clientY - r.top, Math.exp(-e.deltaY / 300));
  tell();
}, {passive: false});
new ResizeObserver(() => tell()).observe(canvas);

// ---- who is signed in --------------------------------------------------------------------------
const principal = document.getElementById('principal') as HTMLSelectElement;
const users = (await (await fetch('/users')).json()) as {name: string; label: string}[];
for (const u of users) principal.add(new Option(u.label, u.name));
principal.value = users.at(-1)?.name ?? '';
principal.addEventListener('change', () => void open(principal.value));
await open(principal.value);
