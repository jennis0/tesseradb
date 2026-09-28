import {svg, type TemplateResult} from 'lit';

/** The icons: line icons in `currentColor`, most drawn on a 16-unit grid, the map tools on 24. */
const PATHS = {
  /** An open hand, for panning. */
  pan: {
    box: 24,
    body: svg`<path d="M18 11V6a2 2 0 0 0-4 0"/><path d="M14 10V4a2 2 0 0 0-4 0v2"/><path d="M10 10.5V6a2 2 0 0 0-4 0v8"/><path d="M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-6-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15"/>`
  },
  box: {box: 24, body: svg`<rect x="4" y="4" width="16" height="16" rx="1.5" stroke-dasharray="3 2.4"/>`},
  lasso: {box: 24, body: svg`<path d="M8 17c-2.5-1-4-3-4-5.5C4 7.4 7.6 4 12 4s8 3.4 8 7.5S16.4 19 12 19c-.8 0-1.6-.1-2.3-.3"/><circle cx="8" cy="19" r="1.8"/>`},
  fit: {box: 24, body: svg`<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/>`},
  layers: {box: 24, body: svg`<path d="M12 3l9 5-9 5-9-5z"/><path d="M3 13l9 5 9-5"/>`},
  filter: {box: 24, body: svg`<path d="M4 5h16l-6 7v6l-4 2v-8z"/>`},
  /** A frame around a dashed hole, for the complement of a set: everything outside it. */
  outside: {box: 24, body: svg`<rect x="3.5" y="3.5" width="17" height="17" rx="2"/><circle cx="12" cy="12" r="4.5" stroke-dasharray="2.2 2.2"/>`},
  close: {box: 24, body: svg`<path d="M6 6l12 12M18 6L6 18"/>`},
  plus: {box: 24, body: svg`<path d="M12 5v14M5 12h14"/>`},
  search: {box: 24, body: svg`<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>`},
  chev: {box: 24, body: svg`<path d="M6 9l6 6 6-6"/>`},
  chevr: {box: 24, body: svg`<path d="M9 6l6 6-6 6"/>`},
  info: {box: 16, body: svg`<circle cx="8" cy="8" r="6"/><path d="M8 7v4M8 5v.5"/>`},
  open: {box: 16, body: svg`<path d="M9 3h4v4M13 3l-6 6M7 3H3v10h10V9"/>`},
  list: {box: 16, body: svg`<path d="M5 4h9M5 8h9M5 12h9M2 4h.5M2 8h.5M2 12h.5"/>`},
  /** Density drawn not at all: an empty frame struck through. */
  'density-none': {box: 16, body: svg`<rect x="2.5" y="3.5" width="11" height="9" rx="1"/><path d="M3.5 12l9-8"/>`},
  /** Density as a soft wash: rings fading outwards. */
  'density-smooth': {box: 16, body: svg`<circle cx="8" cy="8" r="5.5" stroke-opacity="0.3"/><circle cx="8" cy="8" r="3" stroke-opacity="0.65"/><circle cx="8" cy="8" r="0.8" fill="currentColor"/>`},
  /** Density in hexagons. */
  'density-hex': {box: 16, body: svg`<path d="M5 2.8h6L14 8l-3 5.2H5L2 8z"/>`},
  /** Density in square cells. */
  'density-grid': {box: 16, body: svg`<rect x="2.5" y="2.5" width="11" height="11" rx="1"/><path d="M8 2.5v11M2.5 8h11"/>`},
  /** Density as contour lines. */
  'density-lines': {box: 16, body: svg`<ellipse cx="8" cy="8" rx="6" ry="4.6"/><ellipse cx="8.6" cy="8.2" rx="3" ry="2.1"/>`},
  /** A marker pen over a ruled line, for the highlight verb. */
  highlight: {box: 16, body: svg`<path d="M4.5 10.5l5.5-5.5 2.5 2.5-5.5 5.5H4.5v-2.5z"/><path d="M2.5 14.5h11"/>`}
};

export type IconName = keyof typeof PATHS;

/** An icon `size` pixels square. The stroke is in pixels whatever grid the icon is drawn on. */
export function icon(name: IconName, size = 16, strokeWidth = 1.3): TemplateResult {
  const {box, body} = PATHS[name];
  return svg`<svg width=${size} height=${size} viewBox=${`0 0 ${box} ${box}`} fill="none" stroke="currentColor" stroke-width=${(strokeWidth * box) / size} stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}
