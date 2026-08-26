import {svg, type TemplateResult} from 'lit';

/**
 * The icons, as the boards draw them (`gen.py`'s `_ICONS`): 16-unit line icons, `currentColor`,
 * a 1.5 stroke. Words never stand in for these on a control the boards draw as an icon.
 */
const PATHS = {
  pan: svg`<path d="M9 3v7M5 6v4M13 6v4M5 10c0 3 2 5 4 5s4-2 4-5"/>`,
  box: svg`<rect x="3" y="3" width="10" height="10" stroke-dasharray="2.5 2"/>`,
  lasso: svg`<path d="M8 2.5c3.3 0 5.5 1.6 5.5 3.7S11.3 10 8 10 2.5 8.4 2.5 6.2 4.7 2.5 8 2.5z"/><path d="M6 9.5c-.5 1.5-.5 3 .8 4"/>`,
  fit: svg`<path d="M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4"/>`,
  refresh: svg`<path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9"/><path d="M13.5 2.5v3h-3"/>`,
  close: svg`<path d="M4 4l8 8M12 4l-8 8"/>`,
  search: svg`<circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5L14 14"/>`,
  chev: svg`<path d="M4 6l4 4 4-4"/>`,
  chevr: svg`<path d="M6 4l4 4-4 4"/>`,
  layers: svg`<path d="M8 2l6 3-6 3-6-3 6-3z"/><path d="M2 8l6 3 6-3M2 11l6 3 6-3"/>`,
  filter: svg`<path d="M2 3h12l-4.5 5.5V13l-3 1.5V8.5L2 3z"/>`,
  info: svg`<circle cx="8" cy="8" r="6"/><path d="M8 7v4M8 5v.5"/>`,
  warn: svg`<path d="M8 2l6.5 11.5h-13L8 2z"/><path d="M8 6.5v3M8 11.5v.5"/>`,
  check: svg`<path d="M3 8.5l3 3 7-7"/>`,
  open: svg`<path d="M9 3h4v4M13 3l-6 6M7 3H3v10h10V9"/>`,
  list: svg`<path d="M5 4h9M5 8h9M5 12h9M2 4h.5M2 8h.5M2 12h.5"/>`,
  clock: svg`<circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.5 1.5"/>`,
  lock: svg`<rect x="3" y="7" width="10" height="7" rx="1"/><path d="M5 7V5a3 3 0 0 1 6 0v2"/>`,
  menu: svg`<path d="M2 4h12M2 8h12M2 12h12"/>`
};

export type IconName = keyof typeof PATHS;

export function icon(name: IconName, size = 16, strokeWidth = 1.5): TemplateResult {
  return svg`<svg width=${size} height=${size} viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width=${strokeWidth} stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${PATHS[name]}</svg>`;
}
