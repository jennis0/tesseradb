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
  map: ['canvas', 'controls', 'density-key', 'overlay', 'region-tag', 'tooltip', ...STATE],
  status: ['card', 'strip', 'count-shown', 'count-matched', 'count-highlighted', 'count-of', 'count-visible', ...STATE],
  'view-picker': ['field', 'select'],
  'key-picker': ['entry', 'field', 'label', 'select', 'step'],
  'layer-picker': ['entry', 'name', 'note', 'title', ...STATE],
  filter: ['bar', 'entry', 'mode', 'more', 'operator', 'operators', 'refusal', 'tick', 'value-count', 'values'],
  'cluster-filter': ['entry', 'more', 'name', 'option', 'path', 'refusal', 'value-count', 'values'],
  'filter-panel': ['add', 'add-list', 'add-note', 'add-option', 'add-search', 'all', 'all-count', 'card', 'chip', 'chips', 'clear', 'clear-highlight', 'edit', 'subject', 'subject-count', 'subject-key', 'subject-name', ...STATE],
  'field-card': [
    'axis',
    'band',
    'bar-match',
    'bar-subject',
    'bin',
    'brush',
    'brush-filter',
    'brush-highlight',
    'choice',
    'colour-popover',
    'count',
    'filter',
    'fold',
    'head',
    'hex',
    'highlight',
    'hue',
    'level-select',
    'more',
    'name',
    'paint',
    'path',
    'plot',
    'ramp',
    'reset',
    'row',
    'rows',
    'spark',
    'sub',
    'sv',
    'swatch',
    'title',
    'verbs'
  ],
  hierarchy: ['actions', 'also', 'children', 'count-masked', 'count-matched', 'counts', 'dismiss', 'expander', 'filter', 'fit', 'highlight', 'layer', 'more', 'name', 'row', 'search', 'title', 'tree', ...STATE],
  selection: ['action', 'actions', 'count-matched', 'count-served', 'count-visible', 'counts', 'item', 'items', 'label', 'refusal', 'state', 'title'],
  'item-card': ['close', 'copy', 'field', 'headline', 'key', 'label', 'open', 'scoped', 'show-all', 'subtitle', 'title', 'value', 'view-chip', ...STATE],
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
