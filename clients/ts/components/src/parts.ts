/**
 * The parts each element renders, for an element that renders it inside its own shadow root and
 * forwards them. A forwarded part is named `<element>-<part>`, the element being its tag without
 * `tessera-`: through `<tessera-explorer>` the map's toolbar is `::part(map-controls)` and the
 * item card's title is `::part(item-card-title)`.
 */

/** What `renderState` draws, in every element that shows a state. */
const STATE = ['state', 'refusal', 'refresh', 'retry', 'reauthorise'];

/**
 * The parts each element may render, by element name without `tessera-`, as the lists an element
 * forwards with `exportparts`. A host that renders an element inside its own shadow root forwards
 * `PARTS[name]` to style them from outside. Each element's reference page says when each part is
 * rendered.
 */
export const PARTS = {
  map: ['canvas', 'controls', 'overlay', 'tooltip', ...STATE],
  status: ['card', 'strip', 'count-shown', 'count-matched', 'count-highlighted', 'count-visible', ...STATE],
  'view-picker': ['field', 'select'],
  'key-picker': ['entry', 'field', 'label', 'select', 'step'],
  legend: ['cluster-option', 'label', 'level-select', 'more', 'ramp', 'select', 'swatch', 'swatches', 'title', 'value', ...STATE],
  'layer-picker': ['entry', 'group', 'name', 'title', ...STATE],
  filter: ['entry', 'label', 'mode', 'more', 'tick', 'values', 'refusal'],
  'filter-panel': ['chip', 'chips', 'clear', 'title', 'verb', ...STATE],
  hierarchy: ['actions', 'also', 'children', 'count-masked', 'count-matched', 'counts', 'expander', 'filter', 'fit', 'highlight', 'layer', 'more', 'name', 'row', 'search', 'title', 'tree', ...STATE],
  'artifact-list': ['count', 'item', 'items', 'more', 'name', 'title', ...STATE],
  selection: ['action', 'actions', 'count-matched', 'count-served', 'count-visible', 'counts', 'item', 'items', 'label', 'refusal', 'state', 'title'],
  'item-card': ['close', 'copy', 'field', 'headline', 'key', 'label', 'open', 'scoped', 'title', 'value', 'view-chip', ...STATE],
  'artifact-card': ['child', 'children', 'close', 'content', 'count', 'filter', 'fit', 'headline', 'highlight', 'label', 'name', 'outside', 'parent', 'parents', 'shape', 'title', 'value', 'verbs', ...STATE]
} as const satisfies Record<string, readonly string[]>;

/** An element name `PARTS` lists: `map`, `status`, `item-card`, and so on. */
export type PartsOf = keyof typeof PARTS;

/**
 * The `exportparts` value that forwards every part of `element` under its prefixed name, and passes
 * on unchanged any names the element itself already forwards.
 */
export function exportparts(element: PartsOf, passed: readonly string[] = []): string {
  return [...PARTS[element].map((p) => `${p}: ${element}-${p}`), ...passed].join(', ');
}

/** The prefixed names `exportparts(element)` produces, for an element that passes them on again. */
export function forwarded(element: PartsOf): string[] {
  return PARTS[element].map((p) => `${element}-${p}`);
}
